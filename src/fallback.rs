//! Every platform but macOS: there is no Accessibility grant, so nothing to
//! watch. Trust is unconditional and a watch delivers only its heartbeat.

use std::io;

use crate::Handler;

pub(crate) fn is_trusted() -> bool {
    true
}

pub(crate) fn request() {}

/// Nothing to register: the handler is dropped, and only the heartbeat's
/// clone of it ever runs.
pub(crate) struct Observer;

impl Observer {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the macOS registration can fail; this shares its signature"
    )]
    pub(crate) fn install(_handler: Handler) -> io::Result<Self> {
        Ok(Self)
    }
}
