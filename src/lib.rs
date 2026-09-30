//! Watch the macOS Accessibility grant.
//!
//! macOS has no public API that tells a process when the user changes its
//! Accessibility permission. Polling `AXIsProcessTrusted` works, but every
//! call is a WindowServer round trip, and the calls are slowest exactly when a
//! poller is least welcome — around a sleep transition, when WindowServer can
//! take seconds to answer.
//!
//! What macOS does have is two undocumented notifications that fire when the
//! list is edited:
//!
//! - [`ACCESSIBILITY_NOTIFICATION`] (`com.apple.accessibility.api`), a
//!   distributed notification HIServices posts when the Accessibility list
//!   changes for *any* application. WebKit lists it among the notifications
//!   it forwards into its sandboxed processes; AltTab, Hammerspoon and others
//!   observe it to learn about revocation.
//! - [`PRIVACY_NOTIFICATION`] (`com.apple.tcc.access.changed`), a Darwin
//!   (`notify(3)`) notification tccd posts when any privacy record changes —
//!   any service, any application, system-wide.
//!
//! Neither alone covers every edit. Measured on macOS 26 (September 2026):
//! adding a row, switching it off and switching it on post both; *removing*
//! the row posts only the Darwin notification, twice. This crate registers
//! for both — the distributed one with `DeliverImmediately` suspension
//! behaviour, so an app that is never frontmost still hears it — and pairs
//! them with an optional heartbeat, because both names are undocumented and
//! a few things are known to swallow them.
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
//! [`Wake::AccessibilityChanged`] and [`Wake::PrivacyChanged`] mean the
//! respective list changed somewhere — possibly for another app, possibly for
//! another privacy service — and one edit usually produces both, so the
//! handler must be cheap and idempotent. [`Wake::Heartbeat`] means nothing
//! but time passed. None of them says what this process's grant now is: read
//! it. [`is_trusted`] is the cheap answer, with one caveat that matters if
//! your safety depends on it: `AXIsProcessTrusted` keeps returning `true`
//! after the user *removes* the app's row from System Settings (as opposed
//! to switching it off), and on macOS 13+ it can lag a toggle by a moment. A
//! process holding an active `CGEventTap` should confirm with a capability
//! probe — creating a filtering tap and checking for `NULL` — rather than
//! trust the read alone.
//!
//! # What can swallow a notification
//!
//! - Delivery needs a run loop: both kinds arrive through the main thread's
//!   `CFRunLoop`, running in a common mode. A process that never runs one (a
//!   plain CLI, or a daemon that only blocks on a channel) sees only
//!   heartbeats.
//! - Since macOS 15, distributed notifications are withheld from unsigned
//!   binaries; ad-hoc-signed development builds do receive them. The Darwin
//!   notification travels through notifyd rather than distnoted and is not
//!   subject to that rule.
//! - Both names are undocumented. Which edits post them is measured above,
//!   not specified, and may change with a macOS release.
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
/// changes for any application: a row added, switched on or switched off —
/// not removed. Undocumented, long-lived, and forwarded by WebKit into its
/// sandboxed processes.
pub const ACCESSIBILITY_NOTIFICATION: &str = "com.apple.accessibility.api";

/// The Darwin (`notify(3)`) notification tccd posts when a privacy record
/// changes: any service, any application, system-wide — including the removal
/// of an Accessibility row, which posts it twice. Undocumented; observable
/// from a shell with `notifyutil -v -w com.apple.tcc.access.changed`.
pub const PRIVACY_NOTIFICATION: &str = "com.apple.tcc.access.changed";

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
    /// [`ACCESSIBILITY_NOTIFICATION`] arrived: the Accessibility list changed,
    /// for this app or another.
    AccessibilityChanged,
    /// [`PRIVACY_NOTIFICATION`] arrived: a privacy record changed, for any
    /// service and any app.
    PrivacyChanged,
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
    /// Register for both notifications and, with `heartbeat`, also call the
    /// handler every period. The handler runs on whichever thread delivers
    /// the wake — the main run loop for a notification, a private thread for
    /// the heartbeat — so keep it short and thread-safe, and do the real work
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// The heartbeat thread could not be spawned, or (macOS) a notification
    /// center is unavailable to this process.
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
    fn a_watch_can_live_in_shared_state() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Watch>();
    }

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
            "nothing but heartbeats while nobody edits privacy settings"
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
            "no heartbeat and no notification means no wake"
        );
    }
}
