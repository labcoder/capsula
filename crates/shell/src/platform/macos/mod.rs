//! Narrow macOS owners; native presence and bootstrap are not implemented here.

#[expect(dead_code, reason = "dormant logger backend has no startup caller")]
pub(crate) mod logging;
