//! `config.toml` on macOS: the settings folder's parent is the Support anchor and its name the
//! product folder, read and replaced through the storage crate's held chain.

use std::io;
use std::path::Path;

use kumochi_protocol::constants::CONFIG_FILE;
use kumochi_storage::{AcquisitionError, SettingsWriteError, replace_settings};

use super::super::{SettingsRead, SettingsWrite};

/// The text of `config.toml` in `dir`; `None` only when the product folder or the file is
/// missing under an admitted anchor.
pub(crate) fn read_settings(dir: &Path) -> Result<Option<String>, SettingsRead> {
    let (support, product) = split(dir).ok_or(SettingsRead::Untrusted)?;
    match kumochi_storage::read_settings(support, product) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| SettingsRead::Read(io::ErrorKind::InvalidData)),
        Err(AcquisitionError::Open(kind)) => Err(SettingsRead::Read(kind)),
        Err(AcquisitionError::Admission(_)) => Err(SettingsRead::Untrusted),
    }
}

/// Replaces `path`, which must be `config.toml` in a product folder, with `text`.
pub(crate) fn write_settings(path: &Path, text: &[u8]) -> Result<(), SettingsWrite> {
    let (support, product) = path
        .file_name()
        .filter(|name| *name == CONFIG_FILE)
        .and(path.parent())
        .and_then(split)
        .ok_or_else(|| SettingsWrite::Unchanged(io::ErrorKind::InvalidInput.into()))?;
    replace_settings(support, product, text).map_err(|error| match error {
        SettingsWriteError::Unchanged(AcquisitionError::Open(kind)) => {
            SettingsWrite::Unchanged(kind.into())
        }
        SettingsWriteError::Unchanged(AcquisitionError::Admission(_)) => {
            SettingsWrite::Unchanged(io::ErrorKind::PermissionDenied.into())
        }
        SettingsWriteError::Unconfirmed => SettingsWrite::Unconfirmed,
    })
}

fn split(dir: &Path) -> Option<(&Path, &str)> {
    Some((dir.parent()?, dir.file_name()?.to_str()?))
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_FILE, SettingsRead, SettingsWrite, read_settings, write_settings};
    use std::io;
    use std::os::unix::fs::DirBuilderExt as _;

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    fn native_the_mac_pair_saves_reloads_and_refuses_text_that_isnt_utf8() {
        let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
            .expect("an explicitly commissioned fixture parent");
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).expect("fixture name entropy");
        let support = std::path::Path::new(&parent).join(format!(
            "brain-settings-{:032x}",
            u128::from_ne_bytes(random)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&support)
            .expect("exclusive fixture anchor");
        let dir = support.join("product");
        let path = dir.join(CONFIG_FILE);
        let absent = read_settings(&dir).map_err(|_| ());
        let saved = write_settings(&path, b"assistant_name = \"Nimbus\"\n").is_ok();
        let reloaded = read_settings(&dir);
        let other = write_settings(&dir.join("other.toml"), b"x");
        let bad = write_settings(&path, &[0xff, 0xfe]).is_ok();
        let refused = read_settings(&dir);
        let kept = std::fs::read(&path).expect("settings bytes");
        std::fs::remove_file(&path).expect("remove fixture settings");
        std::fs::remove_dir(&dir).expect("remove fixture product folder");
        std::fs::remove_dir(&support).expect("remove fixture anchor");
        assert_eq!(absent, Ok(None));
        assert!(saved, "first save");
        assert_eq!(
            reloaded,
            Ok(Some("assistant_name = \"Nimbus\"\n".to_owned()))
        );
        assert!(
            matches!(other, Err(SettingsWrite::Unchanged(error)) if error.kind() == io::ErrorKind::InvalidInput),
            "only config.toml is written"
        );
        assert!(bad, "bytes are saved as given");
        assert_eq!(refused, Err(SettingsRead::Read(io::ErrorKind::InvalidData)));
        assert_eq!(
            kept,
            [0xff, 0xfe],
            "a refused read leaves the file as it was"
        );
    }
}
