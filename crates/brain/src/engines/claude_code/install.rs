//! Finding Claude Code: only the native installer's paths under the user's profile, never `PATH`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use kumochi_protocol::constants::CLAUDE_CODE_PATHS;
use kumochi_protocol::message::ClaudeVersion;

/// Why no usable install was found. It carries no path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindError {
    /// None of the paths holds a file.
    NotFound,
    /// A path holds something other than a native `.exe`, which would need `cmd.exe`.
    Shim,
}

/// The first of [`CLAUDE_CODE_PATHS`] under `profile` that's a file; an entry that isn't a native
/// `.exe` is refused before anything looks at it.
pub(crate) fn locate(profile: &Path) -> Result<PathBuf, FindError> {
    locate_in(profile, CLAUDE_CODE_PATHS)
}

fn locate_in(profile: &Path, paths: &[&str]) -> Result<PathBuf, FindError> {
    for relative in paths {
        let path = profile.join(relative);
        let native = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            && !path.components().any(|part| {
                part.as_os_str()
                    .eq_ignore_ascii_case(OsStr::new("node_modules"))
            });
        if !native {
            return Err(FindError::Shim);
        }
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(FindError::NotFound)
}

/// `claude --version`'s output: the version is its first word, as in `2.1.306 (Claude Code)`.
pub(crate) fn parse_version(stdout: &[u8]) -> Option<ClaudeVersion> {
    let text = std::str::from_utf8(stdout).ok()?;
    text.split_ascii_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests;
