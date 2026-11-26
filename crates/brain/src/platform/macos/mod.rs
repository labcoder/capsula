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
pub(super) mod instance;
#[expect(unsafe_code, reason = "reads the inherited sockets' options")]
pub(super) mod pair;
#[expect(
    unsafe_code,
    reason = "the parent watch owns Darwin process and kqueue calls"
)]
pub(super) mod parent;
pub(super) mod settings;
pub(super) mod stream;
