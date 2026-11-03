//! The user's settings in `config.toml`, under `%APPDATA%\\<product>`.
//!
//! A change is staged as a [`Pending`] write and becomes the settings only once the file is
//! replaced, so a failed write leaves memory and disk as they were. Keys this build doesn't know
//! survive every write, so a later slice's settings outlive a downgrade. Nothing here logs a
//! value or the parser's diagnostics: both can hold names.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::message::{
    AssistantName, ErrorKey, MaxFrameRate, SettingsOp, SettingsView, ThreadId, UserName,
};
use toml::{Table, Value};

const ASSISTANT_NAME: &str = "assistant_name";
const USER_NAME: &str = "user_name";
const ONBOARDED: &str = "onboarded";
const MAX_FRAME_RATE: &str = "max_frame_rate";

/// Why a settings change was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// `%APPDATA%` isn't an absolute path, so the settings have no home.
    #[error("%APPDATA% is not set to an absolute path")]
    NoHome,
    /// The file exists but can't be read or parsed; it's never overwritten.
    #[error("config.toml can't be read or parsed")]
    Unreadable,
    /// Writing the file failed; nothing changed.
    #[error("config.toml couldn't be written")]
    Write(#[source] io::Error),
}

impl ConfigError {
    /// What the user is told.
    pub(crate) fn key(&self) -> ErrorKey {
        match self {
            Self::Unreadable => ErrorKey::ConfigUnreadable,
            Self::NoHome | Self::Write(_) => ErrorKey::SettingsRefused,
        }
    }
}

/// The settings and the file they came from.
pub(crate) struct Config {
    path: Option<PathBuf>,
    /// `Err` when the file can't be written over: no home, or unreadable.
    table: Result<Table, Refusal>,
    view: SettingsView,
}

#[derive(Clone, Copy, Debug)]
enum Refusal {
    NoHome,
    Unreadable,
}

/// A change staged for writing. [`Pending::write`] blocks, so it runs off the core loop.
pub(crate) struct Pending {
    path: PathBuf,
    table: Table,
    view: SettingsView,
}

/// Only the table's shape: its values and the path can hold names.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("loaded", &self.table.is_ok())
            .field("keys", &self.table.as_ref().map_or(0, Table::len))
            .finish_non_exhaustive()
    }
}

/// Only the table's size: its values and the path can hold names.
impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("keys", &self.table.len())
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The settings in `config.toml` under `dir`, or the defaults, with the store's main thread,
    /// which every view names. Only a missing file is writable as new; any other read or parse
    /// failure refuses every change until the user repairs or removes the file and restarts.
    pub(crate) fn load(dir: Option<PathBuf>, main_thread: ThreadId) -> Self {
        let Some(path) = dir.map(|dir| dir.join(CONFIG_FILE)) else {
            tracing::warn!("%APPDATA% isn't an absolute path; settings won't be saved");
            return Self::refusing(None, Refusal::NoHome, main_thread);
        };
        let table = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let Ok(table) = text.parse::<Table>() else {
                    tracing::warn!("config.toml doesn't parse; using defaults, not saving");
                    return Self::refusing(Some(path), Refusal::Unreadable, main_thread);
                };
                table
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Table::new(),
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "config.toml can't be read; not saving");
                return Self::refusing(Some(path), Refusal::Unreadable, main_thread);
            }
        };
        let view = view_of(&table, main_thread);
        Self {
            path: Some(path),
            table: Ok(table),
            view,
        }
    }

    fn refusing(path: Option<PathBuf>, refusal: Refusal, main_thread: ThreadId) -> Self {
        Self {
            path,
            table: Err(refusal),
            view: view_of(&Table::new(), main_thread),
        }
    }

    /// The settings now.
    pub(crate) fn view(&self) -> &SettingsView {
        &self.view
    }

    /// Stages `op` on top of the current table, changing nothing yet.
    ///
    /// # Errors
    /// [`ConfigError::NoHome`] or [`ConfigError::Unreadable`] when the file can't be written.
    pub(crate) fn stage(&self, op: SettingsOp) -> Result<Pending, ConfigError> {
        let mut table = match &self.table {
            Ok(table) => table.clone(),
            Err(Refusal::NoHome) => return Err(ConfigError::NoHome),
            Err(Refusal::Unreadable) => return Err(ConfigError::Unreadable),
        };
        let path = self.path.clone().ok_or(ConfigError::NoHome)?;
        match op {
            SettingsOp::SetAssistantName(name) => {
                table.insert(ASSISTANT_NAME.to_owned(), Value::String(name.into()));
            }
            SettingsOp::SetUserName(Some(name)) => {
                table.insert(USER_NAME.to_owned(), Value::String(name.into()));
            }
            SettingsOp::SetUserName(None) => {
                table.remove(USER_NAME);
            }
            SettingsOp::FinishOnboarding => {
                table.insert(ONBOARDED.to_owned(), Value::Boolean(true));
            }
            SettingsOp::SetMaxFrameRate(rate) => match rate.fps() {
                Some(fps) => {
                    table.insert(MAX_FRAME_RATE.to_owned(), Value::Integer(fps.into()));
                }
                None => {
                    table.remove(MAX_FRAME_RATE);
                }
            },
        }
        let view = view_of(&table, self.view.main_thread);
        Ok(Pending { path, table, view })
    }

    /// Makes a written change the settings.
    pub(crate) fn commit(&mut self, pending: Pending) {
        self.view = pending.view;
        self.table = Ok(pending.table);
    }
}

impl Pending {
    /// Writes the whole table beside the file, then replaces it. Blocks until it's on disk.
    ///
    /// # Errors
    /// [`ConfigError::Write`] when any step fails; the old file is untouched.
    pub(crate) fn write(&self) -> Result<(), ConfigError> {
        let text = toml::to_string(&self.table)
            .map_err(|_| ConfigError::Write(io::Error::from(io::ErrorKind::InvalidData)))?;
        write_replacing(&self.path, text.as_bytes()).map_err(ConfigError::Write)
    }
}

/// A leftover temp file from a crash is never read; this overwrites it.
fn write_replacing(path: &Path, text: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(dir)?;
    let temp = path.with_file_name(format!("{CONFIG_FILE}.tmp"));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(text)?;
    file.sync_all()?;
    drop(file);
    crate::platform::replace_file(&temp, path)
}

/// The settings a table holds. A known key with a bad value reads as its default, and stays in
/// the table untouched until the user sets that key.
fn view_of(table: &Table, main_thread: ThreadId) -> SettingsView {
    let assistant_name = match table.get(ASSISTANT_NAME) {
        None => None,
        Some(Value::String(text)) => AssistantName::new(text)
            .ok()
            .or_else(|| invalid(ASSISTANT_NAME)),
        Some(_) => invalid(ASSISTANT_NAME),
    }
    .unwrap_or_else(|| {
        AssistantName::new(DEFAULT_ASSISTANT_NAME).expect("the default name is valid")
    });
    let user_name = match table.get(USER_NAME) {
        None => None,
        Some(Value::String(text)) => UserName::new(text).ok().or_else(|| invalid(USER_NAME)),
        Some(_) => invalid(USER_NAME),
    };
    let onboarded = match table.get(ONBOARDED) {
        None => false,
        Some(Value::Boolean(done)) => *done,
        Some(_) => invalid(ONBOARDED).unwrap_or(false),
    };
    let max_frame_rate = match table.get(MAX_FRAME_RATE) {
        None => MaxFrameRate::AUTOMATIC,
        Some(Value::Integer(fps)) => u16::try_from(*fps)
            .ok()
            .and_then(|fps| MaxFrameRate::cap(fps).ok())
            .or_else(|| invalid(MAX_FRAME_RATE))
            .unwrap_or_default(),
        Some(_) => invalid(MAX_FRAME_RATE).unwrap_or_default(),
    };
    SettingsView {
        assistant_name,
        user_name,
        onboarded,
        main_thread,
        max_frame_rate,
    }
}

fn invalid<T>(key: &str) -> Option<T> {
    tracing::warn!(key, "invalid setting; using the default");
    None
}

#[cfg(test)]
mod tests;
