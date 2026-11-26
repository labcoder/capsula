//! macOS candidates for shell-owned platform resources.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Mac connection starts its brain child from I1e-2"
    )
)]
#[expect(
    unsafe_code,
    reason = "Darwin process calls are isolated to this platform adapter"
)]
mod child;
mod instance;
mod leg;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Mac connection starts its brain child from I1e-2"
    )
)]
mod pair;

pub(crate) use child::{NotStarted, start_brain};
pub(crate) use instance::try_acquire_instance_lock;
pub use kumochi_storage::application_support_dir as support_dir;
pub(crate) use leg::{PipeClient, initiate};
pub(crate) use pair::ShellEnds;
