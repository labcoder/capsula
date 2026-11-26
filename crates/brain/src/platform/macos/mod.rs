#[expect(
    unsafe_code,
    reason = "loads ScreenCaptureKit, reads the OS version and calls the capture APIs"
)]
pub(super) mod capture;
pub(super) mod clock;
#[expect(
    unsafe_code,
    reason = "the Keychain store calls Security and LocalAuthentication"
)]
pub(super) mod credential;
pub(super) mod entry;
#[expect(unsafe_code, reason = "the instance item's writes call Security")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound startup is not wired yet")
)]
pub(super) mod instance;
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
    not(any(test, all(debug_assertions, feature = "test-support"))),
    expect(dead_code, reason = "parent-bound admission is not wired yet")
)]
pub(super) mod parent;
pub(super) mod settings;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the pair adapter is not wired yet")
)]
pub(super) mod stream;
