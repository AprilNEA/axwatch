# axwatch

Watch the macOS Accessibility grant: the distributed notification that fires when the Accessibility list changes, a heartbeat backstop, and typed trust reads.

macOS has no public API that tells a process when the user changes its Accessibility permission. Polling `AXIsProcessTrusted` works, but each call is a WindowServer round trip, and they are slowest exactly when a poller is least welcome — around a sleep transition. What macOS does have is an undocumented distributed notification, `com.apple.accessibility.api`, that HIServices posts whenever the Accessibility list changes for any application. WebKit forwards it into its sandboxed processes; AltTab, Hammerspoon and others observe it. `axwatch` registers for it with `DeliverImmediately` suspension behaviour — so an app that is never frontmost still hears it — and pairs it with an optional heartbeat.

## Usage

```rust
use std::time::Duration;

let watch = axwatch::Watch::start(Some(Duration::from_secs(5)), |wake| {
    if !axwatch::is_trusted() {
        eprintln!("Accessibility revoked ({wake:?})");
    }
})?;
// Keep `watch` alive for as long as you want to be told; dropping it unregisters.
```

`Wake::Changed` means the Accessibility list changed somewhere, possibly for another app; `Wake::Heartbeat` means the period elapsed. Neither says what your grant now is — read it. `is_trusted()` is the cheap answer; `request()` raises the system prompt and lists the process in System Settings.

## Caveats you must design for

- `AXIsProcessTrusted` keeps returning `true` after the user *removes* the app's row from System Settings (as opposed to switching it off), and can lag a toggle on macOS 13+. A process holding an active `CGEventTap` should confirm with a capability probe — creating a filtering tap and checking for `NULL` — rather than trust the read alone.
- Delivery needs the main thread's `CFRunLoop` to be running. A process without one sees only heartbeats.
- Since macOS 15, distributed notifications are silently withheld from unsigned binaries. Ad-hoc-signed development builds may be affected; Developer ID and App Store builds are not.
- The notification name is undocumented, and which System Settings actions post it is not characterised by Apple. Pick the heartbeat period from what a missed notification would cost you.

## Platforms

Compiles everywhere; off macOS `is_trusted()` returns `true`, `request()` does nothing, and a `Watch` delivers only its heartbeat, so cross-platform code needs no `cfg`.

## Try it

```sh
cargo run --example watch -- 5
```

then change any application's switch under System Settings → Privacy & Security → Accessibility.

## License

MIT OR Apache-2.0.
