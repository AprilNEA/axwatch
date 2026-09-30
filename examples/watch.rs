//! Print every wake with a timestamp. Pass a heartbeat period in seconds as
//! the only argument, or nothing for notifications only.
//!
//! ```sh
//! cargo run --example watch -- 5
//! ```
//!
//! Then change any application's switch under System Settings → Privacy &
//! Security → Accessibility and watch `Changed` arrive.

use std::io;
use std::time::{Duration, Instant};

fn main() -> io::Result<()> {
    let heartbeat = match std::env::args().nth(1) {
        Some(seconds) => Some(Duration::from_secs(seconds.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("heartbeat period in seconds: {error}"),
            )
        })?)),
        None => None,
    };
    let started = Instant::now();
    println!(
        "trusted={} observing {} heartbeat={heartbeat:?}",
        axwatch::is_trusted(),
        axwatch::NOTIFICATION,
    );
    let _watch = axwatch::Watch::start(heartbeat, move |wake| {
        println!(
            "[{:8.3}] {wake:?} trusted={}",
            started.elapsed().as_secs_f64(),
            axwatch::is_trusted()
        );
    })?;

    // Distributed notifications are delivered through the main run loop.
    #[cfg(target_os = "macos")]
    objc2_core_foundation::CFRunLoop::run();
    #[cfg(not(target_os = "macos"))]
    std::thread::park();
    Ok(())
}
