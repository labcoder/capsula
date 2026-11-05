//! `claude auth status --json`: whether Claude Code is signed in, and that it chose Kumochi's
//! folder. Nothing else it prints is kept.

use std::path::Path;

use serde::Deserialize;

/// Why a status couldn't be used. It carries nothing Claude Code printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusFailure {
    /// An exit code other than 0 or 1.
    Exit,
    /// Not the JSON object expected.
    Malformed,
    /// `configDirectory` isn't Kumochi's folder.
    OtherFolder,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    logged_in: bool,
    config_directory: String,
}

/// Whether the status says signed in. `config` is the `CLAUDE_CONFIG_DIR` it was given, which
/// `configDirectory` must equal exactly, or the route fails closed.
pub(crate) fn parse_status(stdout: &[u8], exit: u32, config: &Path) -> Result<bool, StatusFailure> {
    if exit > 1 {
        return Err(StatusFailure::Exit);
    }
    let fields: Fields = serde_json::from_slice(stdout).map_err(|_| StatusFailure::Malformed)?;
    if Path::new(&fields.config_directory) != config
        || fields.config_directory.as_str() != config.as_os_str()
    {
        return Err(StatusFailure::OtherFolder);
    }
    Ok(fields.logged_in)
}

#[cfg(test)]
mod tests;
