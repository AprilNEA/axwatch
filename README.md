# axwatch

Watch the macOS Accessibility grant: the two undocumented notifications that fire when the Accessibility list is edited, a heartbeat backstop, and typed trust reads.

macOS has no public API that tells a process when the user changes its Accessibility permission. Polling `AXIsProcessTrusted` works, but each call is a WindowServer round trip, and they are slowest exactly when a poller is least welcome — around a sleep transition. What macOS does have is two undocumented notifications:

- `com.apple.accessibility.api`, a distributed notification HIServices posts when the Accessibility list changes for any application. WebKit forwards it into its sandboxed processes; AltTab, Hammerspoon and others observe it.
- `com.apple.tcc.access.changed`, a Darwin (`notify(3)`) notification tccd posts when any privacy record changes — any service, any application, system-wide.

Neither alone covers every edit. Measured on macOS 26 (September 2026), against tccd's own event log:

| Edit in System Settings | `com.apple.accessibility.api` | `com.apple.tcc.access.changed` |
|---|---|---|
| Add a row with "+" | 1 | 1 |
| Switch off | 1 | 1 |
| Switch on | 1 | 1 |
| Remove the row | — | 2 |

`axwatch` registers for both — the distributed one with `DeliverImmediately` suspension behaviour, so an app that is never frontmost still hears it — and pairs them with an optional heartbeat.

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

`Wake::AccessibilityChanged` and `Wake::PrivacyChanged` mean the respective list changed somewhere, possibly for another app or another service — one edit usually produces both, so keep the handler cheap and idempotent. `Wake::Heartbeat` means the period elapsed. None says what your grant now is — read it. `is_trusted()` is the cheap answer; `request()` raises the system prompt and lists the process in System Settings.

## Caveats you must design for

- `AXIsProcessTrusted` keeps returning `true` after the user *removes* the app's row from System Settings (as opposed to switching it off), and can lag a toggle on macOS 13+. A process holding an active `CGEventTap` should confirm with a capability probe — creating a filtering tap and checking for `NULL` — rather than trust the read alone.
- Delivery needs the main thread's `CFRunLoop` to be running in a common mode. A process without one sees only heartbeats.
- Since macOS 15, distributed notifications are withheld from unsigned binaries; ad-hoc-signed development builds do receive them. The Darwin notification travels through notifyd rather than distnoted and is not subject to that rule.
- Both names are undocumented. Which edits post them is measured, not specified, and may change with a macOS release. Pick the heartbeat period from what a missed notification would cost you.

## Platforms

Compiles everywhere; off macOS `is_trusted()` returns `true`, `request()` does nothing, and a `Watch` delivers only its heartbeat, so cross-platform code needs no `cfg`.

## Try it

```sh
cargo run --example watch -- 5
```

then edit any application's row under System Settings → Privacy & Security → Accessibility.

## License

MIT OR Apache-2.0.
