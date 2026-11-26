#[expect(
    unsafe_code,
    reason = "loads ScreenCaptureKit, reads the OS version and calls the capture APIs"
)]
pub(super) mod capture;
pub(super) mod clock;
#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "the dormant Keychain store calls Security and LocalAuthentication"
)]
pub(super) mod credential;
#[expect(unsafe_code, reason = "reads the inherited sockets' options")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound admission is not wired yet")
)]
pub(super) mod pair;
#[expect(
    unsafe_code,
    reason = "the parent watch owns Darwin process and kqueue calls"
)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound admission is not wired yet")
)]
pub(super) mod parent;
pub(super) mod settings;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the pair adapter is not wired yet")
)]
pub(super) mod stream;
