//! Narrow macOS owners; native presence and bootstrap are not implemented here.

pub(crate) mod logging;
#[expect(
    dead_code,
    reason = "dormant until the connected Mac shell admits screen capture"
)]
pub(crate) mod screen_access;
