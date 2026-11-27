//! The macOS model-storage forwards: the shared model calls on kumochi-storage's adapter, below the
//! Application Support anchor. Dormant: activation re-exports them in place of the unsupported
//! stubs, after the native install, remove and restart run.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};

use kumochi_storage::Seal;

use super::Untrusted;

/// The anchor every model path must be below.
fn anchor() -> Result<PathBuf, Untrusted> {
    #[cfg(test)]
    if let Some(anchor) = tests::ANCHOR.with(|anchor| anchor.borrow().clone()) {
        return Ok(anchor);
    }
    kumochi_storage::application_support_dir().map_err(|_| Untrusted::Io)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn validate_model_path(path: &Path) -> Result<(), Untrusted> {
    kumochi_storage::validate_private_path(&anchor()?, path).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn hold_model_chain(path: &Path) -> Result<Vec<File>, Untrusted> {
    kumochi_storage::hold_private_chain(&anchor()?, path).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn create_model_dir(path: &Path) -> Result<Vec<File>, Untrusted> {
    kumochi_storage::create_private_dir(&anchor()?, path).map_err(Untrusted::from)
}

/// The folder `path`, held private. Binds nothing against renames: each later step re-checks.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn hold_model_creation(path: &Path) -> Result<File, Untrusted> {
    hold_model_chain(path)?.pop().ok_or(Untrusted::Io)
}

/// A sealed final for loading.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn model_file(path: &Path) -> Result<File, Untrusted> {
    model_final(path)
}

/// A sealed final, checked before the fetch hashes it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn model_final(path: &Path) -> Result<File, Untrusted> {
    kumochi_storage::open_sealed(&anchor()?, path, Seal::ReadOnly).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn check_model_final(file: &File, path: &Path) -> Result<(), Untrusted> {
    kumochi_storage::check_named(&anchor()?, file, path).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn keep_model_final(file: &File, path: &Path) -> Result<(), Untrusted> {
    kumochi_storage::keep_sealed(&anchor()?, file, path, Seal::ReadOnly).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn open_model_write(path: &Path) -> Result<File, Untrusted> {
    kumochi_storage::open_private_write(&anchor()?, path).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn create_model_write(path: &Path) -> Result<File, Untrusted> {
    kumochi_storage::create_private_file(&anchor()?, path).map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn discard_model(file: File, parent: &File, path: &Path) -> Result<(), Untrusted> {
    kumochi_storage::discard_entry(&anchor()?, file, parent, path).map_err(Untrusted::from)
}

/// Seals the partial `file` and publishes it at `target`. The partial's name is `target`'s plus
/// `.part`, as the fetch names it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn publish_model(file: File, parent: &File, target: &Path) -> Result<(), Untrusted> {
    let mut name = OsString::from(target.file_name().ok_or(Untrusted::Unsafe)?);
    name.push(".part");
    kumochi_storage::seal_and_publish(
        &anchor()?,
        file,
        parent,
        &target.with_file_name(name),
        target,
        Seal::ReadOnly,
    )
    .map_err(Untrusted::from)
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "re-exported by the macOS voice activation")
)]
pub(crate) fn remove_model_tree(path: &Path) -> Result<(), Untrusted> {
    kumochi_storage::remove_private_tree(&anchor()?, path).map_err(Untrusted::from)
}

#[cfg(test)]
mod tests;
