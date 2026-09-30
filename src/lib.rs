//! Watch the macOS Accessibility grant.
//!
//! macOS has no public API that tells a process when the user changes its
//! Accessibility permission. Polling `AXIsProcessTrusted` works, but every
//! call is a WindowServer round trip, and the calls are slowest exactly when a
//! poller is least welcome — around a sleep transition, when WindowServer can
//! take seconds to answer.
//!
//! What macOS does have is an undocumented distributed notification,
//! [`NOTIFICATION`] (`com.apple.accessibility.api`), that HIServices posts
//! whenever the Accessibility list changes for *any* application. WebKit lists
//! it among the notifications it forwards into its sandboxed processes, and
//! AltTab, Hammerspoon and others observe it to learn about revocation. This
//! crate registers for it with `DeliverImmediately` suspension behaviour — so
//! an app that is never frontmost still hears it — and pairs it with an
//! optional heartbeat, because the notification is undocumented and a few
//! things are known to swallow it.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! let watch = axwatch::Watch::start(Some(Duration::from_secs(5)), |wake| {
//!     // `wake` says why we are here; whether the grant is still ours is a
//!     // separate question — see the caveats below.
//!     if !axwatch::is_trusted() {
//!         eprintln!("Accessibility revoked ({wake:?})");
//!     }
//! })?;
//! // Keep `watch` alive for as long as you want to be told.
//! # drop(watch);
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # What a wake means
//!
//! [`Wake::Changed`] means the Accessibility list changed somewhere — possibly
//! for another app. [`Wake::Heartbeat`] means nothing but time passed. Neither
//! says what this process's grant now is: read it. [`is_trusted`] is the cheap
//! answer, with one caveat that matters if your safety depends on it:
//! `AXIsProcessTrusted` keeps returning `true` after the user *removes* the
//! app's row from System Settings (as opposed to switching it off), and on
//! macOS 13+ it can lag a toggle by a moment. A process holding an active
//! `CGEventTap` should confirm with a capability probe — creating a filtering
//! tap and checking for `NULL` — rather than trust the read alone.
//!
//! # What can swallow the notification
//!
//! - Delivery needs a run loop: distributed notifications arrive through the
//!   main thread's `CFRunLoop`. A process that never runs one (a plain CLI,
//!   or a daemon that only blocks on a channel) sees only heartbeats.
//! - Since macOS 15, distributed notifications are silently withheld from
//!   unsigned binaries. Ad-hoc-signed development builds may be affected;
//!   Developer ID and App Store builds are not.
//! - The name is undocumented; which System Settings actions post it (switch
//!   off, remove the row, reset with `tccutil`) is not characterised by Apple.
//!
//! The heartbeat exists for those three. Choose its period from what a missed
//! notification would cost you: a few seconds for a process whose active
//! event tap would otherwise outlive its grant, a minute for one that only
//! updates a status label.
//!
//! # Platforms
//!
//! The crate compiles everywhere, so cross-platform code needs no `cfg`. Off
//! macOS there is no Accessibility grant to watch: [`is_trusted`] returns
//! `true`, [`request`] does nothing, and a [`Watch`] delivers only its
//! heartbeat.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(target_os = "macos"))]
mod fallback;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(target_os = "macos"))]
use fallback as platform;
#[cfg(target_os = "macos")]
use macos as platform;

/// The distributed notification HIServices posts when the Accessibility list
/// changes for any application. Undocumented, long-lived, and forwarded by
/// WebKit into its sandboxed processes.
pub const NOTIFICATION: &str = "com.apple.accessibility.api";

/// Whether this process is currently a trusted Accessibility client
/// (`AXIsProcessTrusted`). Never prompts. See the crate docs for when this
/// read is stale.
#[must_use]
pub fn is_trusted() -> bool {
    platform::is_trusted()
}

/// Ask macOS to prompt for Accessibility if it has not decided yet, and list
/// this process in System Settings (`AXIsProcessTrustedWithOptions` with
/// `kAXTrustedCheckOptionPrompt`). The user's decision arrives later, as a
/// [`Wake`]; read [`is_trusted`] then.
pub fn request() {
    platform::request();
}

/// Why a [`Watch`] called its handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// [`NOTIFICATION`] arrived: the Accessibility list changed, for this
    /// app or another.
    Changed,
    /// The heartbeat period elapsed.
    Heartbeat,
}

type Handler = Arc<dyn Fn(Wake) + Send + Sync>;

/// A registration for Accessibility-grant changes. Dropping it unregisters
/// the observer and stops the heartbeat; a handler call already in progress
/// finishes first.
pub struct Watch {
    // Fields drop in order: the heartbeat thread is joined before the
    // observer is torn down.
    heartbeat: Option<Heartbeat>,
    _observer: platform::Observer,
}

impl Watch {
    /// Register for [`NOTIFICATION`] and, with `heartbeat`, also call the
    /// handler every period. The handler runs on whichever thread delivers
    /// the wake — the main run loop for a notification, a private thread for
    /// the heartbeat — so keep it short and thread-safe, and do the real work
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// The heartbeat thread could not be spawned, or (macOS) the distributed
    /// notification center is unavailable to this process.
    pub fn start(
        heartbeat: Option<Duration>,
        on_wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let handler: Handler = Arc::new(on_wake);
        let observer = platform::Observer::install(Arc::clone(&handler))?;
        let heartbeat = heartbeat
            .map(|every| Heartbeat::start(every, handler))
            .transpose()?;
        Ok(Self {
            heartbeat,
            _observer: observer,
        })
    }

    /// The heartbeat period, if this watch has one.
    #[must_use]
    pub fn heartbeat(&self) -> Option<Duration> {
        self.heartbeat.as_ref().map(|heartbeat| heartbeat.every)
    }
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("heartbeat", &self.heartbeat())
            .finish_non_exhaustive()
    }
}

/// The heartbeat thread: wakes the handler every `every`, until dropped.
struct Heartbeat {
    every: Duration,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Heartbeat {
    fn start(every: Duration, handler: Handler) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("axwatch-heartbeat".into())
            .spawn(move || {
                let mut next = Instant::now() + every;
                loop {
                    // `park_timeout` may return early: sleep until the
                    // deadline, not for a duration.
                    while !stopping.load(Ordering::Acquire) {
                        let now = Instant::now();
                        if now >= next {
                            break;
                        }
                        thread::park_timeout(next - now);
                    }
                    if stopping.load(Ordering::Acquire) {
                        return;
                    }
                    handler(Wake::Heartbeat);
                    next += every;
                }
            })?;
        Ok(Self {
            every,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            // A panic in the handler has already surfaced on that thread.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn heartbeat_wakes_on_its_period_and_stops_on_drop() {
        let wakes = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&wakes);
        let watch = Watch::start(Some(Duration::from_millis(20)), move |wake| {
            seen.lock().unwrap().push(wake);
        })
        .unwrap();
        assert_eq!(watch.heartbeat(), Some(Duration::from_millis(20)));

        thread::sleep(Duration::from_millis(110));
        drop(watch);
        let after_drop = wakes.lock().unwrap().len();
        assert!(
            (3..=6).contains(&after_drop),
            "expected about five heartbeats in 110 ms, got {after_drop}"
        );
        assert!(
            wakes
                .lock()
                .unwrap()
                .iter()
                .all(|wake| *wake == Wake::Heartbeat),
            "nothing but heartbeats without an Accessibility change"
        );

        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            wakes.lock().unwrap().len(),
            after_drop,
            "no heartbeat after drop"
        );
    }

    #[test]
    fn a_watch_without_a_heartbeat_is_silent() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let watch = Watch::start(None, move |_| flag.store(true, Ordering::Release)).unwrap();
        assert_eq!(watch.heartbeat(), None);
        thread::sleep(Duration::from_millis(30));
        drop(watch);
        assert!(
            !fired.load(Ordering::Acquire),
            "no heartbeat and no Accessibility change means no wake"
        );
    }
}
