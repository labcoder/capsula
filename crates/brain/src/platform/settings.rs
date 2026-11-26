//! `config.toml` through ordinary paths: Windows, Linux and every test build.

use std::io::{self, Write as _};
use std::path::Path;

use kumochi_protocol::constants::CONFIG_FILE;

use super::{SettingsRead, SettingsWrite};

/// The text of `config.toml` in `dir`; `None` when the folder or the file is missing.
pub(crate) fn read_settings(dir: &Path) -> Result<Option<String>, SettingsRead> {
    // An existing folder is read only when it's trusted; a missing one is made on first save.
    match std::fs::symlink_metadata(dir) {
        Ok(_) if super::product_folder(dir).is_ok() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => return Err(SettingsRead::Untrusted),
    }
    match std::fs::read_to_string(dir.join(CONFIG_FILE)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SettingsRead::Read(error.kind())),
    }
}

/// Writes `text` beside `path`, then replaces it. A leftover temp file from a crash is never
/// read; this overwrites it.
pub(crate) fn write_settings(path: &Path, text: &[u8]) -> Result<(), SettingsWrite> {
    write_replacing(path, text).map_err(SettingsWrite::Unchanged)
}

fn write_replacing(path: &Path, text: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    // A settings folder made here is owner-only; one already there must be trusted.
    super::product_folder(dir)?;
    let temp = path.with_file_name(format!("{CONFIG_FILE}.tmp"));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(text)?;
    file.sync_all()?;
    drop(file);
    super::replace_file(&temp, path)
}
