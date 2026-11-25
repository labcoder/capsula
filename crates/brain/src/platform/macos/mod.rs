#[expect(
    unsafe_code,
    reason = "loads ScreenCaptureKit, reads the OS version and calls the capture APIs"
)]
pub(super) mod capture;
pub(super) mod clock;
#[expect(
    unsafe_code,
    reason = "the parent watch owns Darwin process and kqueue calls"
)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound admission is not wired yet")
)]
pub(super) mod parent;
