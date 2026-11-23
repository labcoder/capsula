//! The user's settings in `config.toml`, under `%APPDATA%\\<product>`.
//!
//! A change is staged as a [`Pending`] write and becomes the settings only once the file is
//! replaced, so a failed write leaves memory and disk as they were. Keys this build doesn't know
//! survive every write, so a later slice's settings outlive a downgrade. Nothing here logs a
//! value or the parser's diagnostics: both can hold names.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::store::choice::StoredChoice;
use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::message::{
    AccountView, AssistantName, DockPosition, ErrorKey, MaxFrameRate, OnboardingStep, RepoName,
    RunId, SettingsOp, SettingsView, SttUnload, ThreadId, UserName,
};
use kumochi_protocol::voice::VoicePart;
use toml::{Table, Value};

use crate::connections::github::device::DefaultRepo;
use tuning::{
    ContextTuning, DrawingTuning, GitHubTuning, ModelTuning, OAuthTuning, RunTuning, TokenTuning,
};

pub(crate) mod tuning;

const ASSISTANT_NAME: &str = "assistant_name";
const USER_NAME: &str = "user_name";
const ONBOARDED: &str = "onboarded";
const ONBOARDING_FROM: &str = "onboarding_from";
const MAX_FRAME_RATE: &str = "max_frame_rate";
const DEFAULT_MODEL: &str = "default_model";
const CLAUDE_CODE: &str = "claude_code";
const GITHUB: &str = "github";
const DEFAULT_ACCOUNT: &str = "default_account";
const DEFAULT_REPO: &str = "default_repo";
const DOCK_POSITION: &str = "dock_position";
const SHOW_IN_TASKBAR: &str = "show_in_taskbar";
const VOICE: &str = "voice";
const TALK_BACK: &str = "talk_back";
const STT_UNLOAD: &str = "stt_unload";
const HOLD_TO_TALK: &str = "hold_to_talk";
const SCREEN_HOLDS: &str = "screen_holds";
const SHOW_SCREEN: &str = "show_screen";
const INSTALLED: &str = "installed";
const REMOVING: &str = "removing";

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
    /// A repository default must belong to a positive GitHub account ID.
    #[error("github default account must be positive")]
    InvalidGitHubAccount,
    /// A voice install, remove or cancel, which this build refuses until its installer lands.
    #[error("voice can't be installed or removed yet")]
    VoiceUnavailable,
}

impl ConfigError {
    /// What the user is told.
    pub(crate) fn key(&self) -> ErrorKey {
        match self {
            Self::Unreadable => ErrorKey::ConfigUnreadable,
            Self::NoHome | Self::Write(_) | Self::InvalidGitHubAccount => ErrorKey::SettingsRefused,
            Self::VoiceUnavailable => ErrorKey::VoiceUnavailable,
        }
    }
}

/// The settings and the file they came from.
pub(crate) struct Config {
    path: Option<PathBuf>,
    /// `Err` when the file can't be written over: no home, or unreadable.
    table: Result<Table, Refusal>,
    view: SettingsView,
    /// `[tuning]`, read once: a change applies at the next start.
    github_tuning: GitHubTuning,
    model_tuning: ModelTuning,
    run_tuning: RunTuning,
    oauth_tuning: OAuthTuning,
    token_tuning: TokenTuning,
    drawing_tuning: DrawingTuning,
    context_tuning: ContextTuning,
    /// What an absent `onboarded` key reads as, fixed at start: true when the profile was
    /// already in use, or the file can't be written, so neither ever asks for onboarding.
    in_use: bool,
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
        let view = view_of(&table, main_thread, false);
        Self {
            path: Some(path),
            github_tuning: GitHubTuning::from_table(&table),
            model_tuning: ModelTuning::from_table(&table),
            run_tuning: RunTuning::from_table(&table),
            oauth_tuning: OAuthTuning::from_table(&table),
            token_tuning: TokenTuning::from_table(&table),
            drawing_tuning: DrawingTuning::from_table(&table),
            context_tuning: ContextTuning::from_table(&table),
            table: Ok(table),
            view,
            in_use: false,
        }
    }

    fn refusing(path: Option<PathBuf>, refusal: Refusal, main_thread: ThreadId) -> Self {
        let mut view = view_of(&Table::new(), main_thread, true);
        // A file that can't be read may have turned screen holds off, or the screen's picture.
        view.screen_holds = false;
        view.show_screen = false;
        Self {
            path,
            table: Err(refusal),
            view,
            github_tuning: GitHubTuning::default(),
            model_tuning: ModelTuning::default(),
            run_tuning: RunTuning::default(),
            oauth_tuning: OAuthTuning::default(),
            token_tuning: TokenTuning::default(),
            drawing_tuning: DrawingTuning::default(),
            context_tuning: ContextTuning::default(),
            in_use: true,
        }
    }

    /// Counts a profile already in use as onboarded while `config.toml` has no `onboarded`
    /// key: the account's first load is signed in, or a run was ever issued. Called once at
    /// start, before any shell is served, so a later sign-in or write never changes the answer.
    pub(crate) fn count_in_use(&mut self, account: &AccountView, high_water: RunId) {
        let Ok(table) = &self.table else {
            return;
        };
        self.in_use = matches!(account, AccountView::SignedIn { .. }) || high_water.0 > 0;
        self.view.onboarded = onboarded(table).unwrap_or(self.in_use);
    }

    /// The settings now.
    pub(crate) fn view(&self) -> &SettingsView {
        &self.view
    }

    pub(crate) fn implicit_talk_back(&self) -> bool {
        self.table.as_ref().is_ok_and(|table| {
            table.get(VOICE).is_none_or(|voice| {
                voice
                    .as_table()
                    .is_some_and(|voice| !voice.contains_key(TALK_BACK))
            })
        })
    }

    pub(crate) fn queued_hold_preferences_known(&self) -> bool {
        let Ok(table) = &self.table else {
            return false;
        };
        match table.get(VOICE) {
            None => true,
            Some(Value::Table(voice)) => {
                let known = |key| voice.get(key).is_none_or(Value::is_bool);
                known(TALK_BACK)
                    && known(SHOW_SCREEN)
                    && (voice.contains_key(SHOW_SCREEN) || known(SCREEN_HOLDS))
            }
            Some(_) => false,
        }
    }

    pub(crate) fn github_tuning(&self) -> GitHubTuning {
        self.github_tuning
    }

    pub(crate) fn model_tuning(&self) -> ModelTuning {
        self.model_tuning
    }

    pub(crate) fn run_tuning(&self) -> RunTuning {
        self.run_tuning
    }

    pub(crate) fn oauth_tuning(&self) -> OAuthTuning {
        self.oauth_tuning
    }

    pub(crate) fn token_tuning(&self) -> TokenTuning {
        self.token_tuning
    }

    pub(crate) fn drawing_tuning(&self) -> DrawingTuning {
        self.drawing_tuning
    }

    pub(crate) fn context_tuning(&self) -> ContextTuning {
        self.context_tuning
    }

    pub(crate) fn default_choice(&self) -> StoredChoice {
        self.table
            .as_ref()
            .map_or(StoredChoice::Invalid, default_choice_of)
    }

    /// `[claude_code] enabled`: on unless it's `false`. Anything else there, or a file that
    /// couldn't be read, turns it off, because it may have said so.
    pub(crate) fn claude_code_enabled(&self) -> bool {
        let Ok(table) = &self.table else {
            return false;
        };
        match table.get(CLAUDE_CODE) {
            None => true,
            Some(Value::Table(section)) => match section.get("enabled") {
                None => true,
                Some(Value::Boolean(enabled)) => *enabled,
                Some(_) => false,
            },
            Some(_) => false,
        }
    }

    /// The saved repository and its account, only when both values are valid.
    pub(crate) fn github_default(&self) -> Option<DefaultRepo> {
        let github = self.table.as_ref().ok()?.get(GITHUB)?.as_table()?;
        let account = github.get(DEFAULT_ACCOUNT)?.as_str()?;
        if !account.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let account = account.parse::<u64>().ok().filter(|id| *id != 0)?;
        let repo = RepoName::new(github.get(DEFAULT_REPO)?.as_str()?).ok()?;
        Some(DefaultRepo { account, repo })
    }

    /// Stages an account-bound repository default without publishing it.
    ///
    /// # Errors
    /// As [`Config::stage`], or [`ConfigError::InvalidGitHubAccount`] for account zero.
    pub(crate) fn stage_github_default(
        &self,
        default: &DefaultRepo,
    ) -> Result<Pending, ConfigError> {
        let (path, mut table) = self.editable()?;
        if default.account == 0 {
            return Err(ConfigError::InvalidGitHubAccount);
        }
        let github = table
            .entry(GITHUB)
            .or_insert_with(|| Value::Table(Table::new()));
        if !github.is_table() {
            *github = Value::Table(Table::new());
        }
        let github = github.as_table_mut().expect("the github key holds a table");
        github.insert(
            DEFAULT_ACCOUNT.to_owned(),
            Value::String(default.account.to_string()),
        );
        github.insert(
            DEFAULT_REPO.to_owned(),
            Value::String(default.repo.as_str().to_owned()),
        );
        let view = view_of(&table, self.view.main_thread, self.in_use);
        Ok(Pending { path, table, view })
    }

    /// Stages `op` on top of the current table, changing nothing yet.
    ///
    /// # Errors
    /// [`ConfigError::NoHome`] or [`ConfigError::Unreadable`] when the file can't be written.
    pub(crate) fn stage(&self, op: SettingsOp) -> Result<Pending, ConfigError> {
        let (path, mut table) = self.editable()?;
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
                table.remove(ONBOARDING_FROM);
            }
            SettingsOp::CompleteOnboardingStep(step) => {
                if let Some(next) = step.next().filter(|next| *next > onboarding_from(&table)) {
                    let value = Value::try_from(next).expect("an onboarding step is a string");
                    table.insert(ONBOARDING_FROM.to_owned(), value);
                }
            }
            SettingsOp::SetMaxFrameRate(rate) => match rate.fps() {
                Some(fps) => {
                    table.insert(MAX_FRAME_RATE.to_owned(), Value::Integer(fps.into()));
                }
                None => {
                    table.remove(MAX_FRAME_RATE);
                }
            },
            SettingsOp::SetDefaultModel(choice) => {
                let value = Value::try_from(choice).expect("a model choice is two strings");
                table.insert(DEFAULT_MODEL.to_owned(), value);
            }
            SettingsOp::SetDockPosition(position) => {
                let value = Value::try_from(position).expect("a dock position is a string");
                table.insert(DOCK_POSITION.to_owned(), value);
            }
            SettingsOp::SetTaskbar(shown) => {
                table.insert(SHOW_IN_TASKBAR.to_owned(), Value::Boolean(shown));
            }
            SettingsOp::SetTalkBack(on) => {
                voice_mut(&mut table).insert(TALK_BACK.to_owned(), Value::Boolean(on));
            }
            SettingsOp::SetHoldToTalk(on) => {
                voice_mut(&mut table).insert(HOLD_TO_TALK.to_owned(), Value::Boolean(on));
            }
            SettingsOp::SetScreenHolds(on) => {
                voice_mut(&mut table).insert(SCREEN_HOLDS.to_owned(), Value::Boolean(on));
            }
            SettingsOp::SetShowScreen(on) => {
                voice_mut(&mut table).insert(SHOW_SCREEN.to_owned(), Value::Boolean(on));
            }
            SettingsOp::SetSttUnload(unload) => {
                let name = match unload {
                    SttUnload::Kept => "kept",
                    SttUnload::After15Min => "15m",
                    SttUnload::After1Hour => "1h",
                };
                voice_mut(&mut table).insert(STT_UNLOAD.to_owned(), Value::String(name.to_owned()));
            }
            SettingsOp::InstallVoice(_)
            | SettingsOp::LoadSpeechInput
            | SettingsOp::RemoveVoice(_)
            | SettingsOp::CancelVoiceInstall => return Err(ConfigError::VoiceUnavailable),
        }
        let view = view_of(&table, self.view.main_thread, self.in_use);
        Ok(Pending { path, table, view })
    }

    /// The parts `[voice] removing` names, by their folder names: a removal that hasn't
    /// finished. A name this build doesn't know names nothing.
    pub(crate) fn removing(&self) -> Vec<VoicePart> {
        self.listed(REMOVING)
    }

    /// The parts `[voice] installed` names and `removing` doesn't: a removal wins.
    pub(crate) fn installed(&self) -> Vec<VoicePart> {
        let removing = self.removing();
        let mut installed = self.listed(INSTALLED);
        installed.retain(|part| !removing.contains(part));
        installed
    }

    fn listed(&self, key: &str) -> Vec<VoicePart> {
        let names = self
            .table
            .as_ref()
            .ok()
            .and_then(|table| table.get(VOICE))
            .and_then(Value::as_table)
            .and_then(|voice| voice.get(key))
            .and_then(Value::as_array);
        VoicePart::ALL
            .into_iter()
            .filter(|part| {
                names
                    .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(part.dir())))
            })
            .collect()
    }

    /// Stages `part`'s removal as finished: it leaves `removing`, and every other name stays.
    ///
    /// # Errors
    /// As [`Config::stage`].
    pub(crate) fn stage_remove_done(&self, part: VoicePart) -> Result<Pending, ConfigError> {
        let (path, mut table) = self.editable()?;
        if let Some(names) = voice_mut(&mut table)
            .get_mut(REMOVING)
            .and_then(Value::as_array_mut)
        {
            names.retain(|name| name.as_str() != Some(part.dir()));
        }
        let view = view_of(&table, self.view.main_thread, self.in_use);
        Ok(Pending { path, table, view })
    }

    /// Stages `part`'s install: it joins `installed`, and every other name stays. The first
    /// spoken replies install also turns talk-back on, unless the user already chose.
    ///
    /// # Errors
    /// As [`Config::stage`].
    pub(crate) fn stage_activate(&self, part: VoicePart) -> Result<Pending, ConfigError> {
        let (path, mut table) = self.editable()?;
        let voice = voice_mut(&mut table);
        if part == VoicePart::SpokenReplies && !voice.contains_key(TALK_BACK) {
            voice.insert(TALK_BACK.to_owned(), Value::Boolean(true));
        }
        listed_add(voice, INSTALLED, part);
        let view = view_of(&table, self.view.main_thread, self.in_use);
        Ok(Pending { path, table, view })
    }

    /// Stages `part`'s removal: it leaves `installed` and joins `removing` in one write, and
    /// every other name stays.
    ///
    /// # Errors
    /// As [`Config::stage`].
    pub(crate) fn stage_remove_intent(&self, part: VoicePart) -> Result<Pending, ConfigError> {
        let (path, mut table) = self.editable()?;
        let voice = voice_mut(&mut table);
        if let Some(names) = voice.get_mut(INSTALLED).and_then(Value::as_array_mut) {
            names.retain(|name| name.as_str() != Some(part.dir()));
        }
        listed_add(voice, REMOVING, part);
        let view = view_of(&table, self.view.main_thread, self.in_use);
        Ok(Pending { path, table, view })
    }

    /// The file's path and a copy of its table to change.
    fn editable(&self) -> Result<(PathBuf, Table), ConfigError> {
        let table = match &self.table {
            Ok(table) => table.clone(),
            Err(Refusal::NoHome) => return Err(ConfigError::NoHome),
            Err(Refusal::Unreadable) => return Err(ConfigError::Unreadable),
        };
        Ok((self.path.clone().ok_or(ConfigError::NoHome)?, table))
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

/// Adds `part` to the voice table's `key` list once, starting a list where there's none.
fn listed_add(voice: &mut Table, key: &str, part: VoicePart) {
    let names = voice.entry(key).or_insert_with(|| Value::Array(Vec::new()));
    if !names.is_array() {
        *names = Value::Array(Vec::new());
    }
    let names = names.as_array_mut().expect("the list holds an array");
    if !names.iter().any(|name| name.as_str() == Some(part.dir())) {
        names.push(Value::String(part.dir().to_owned()));
    }
}

/// The `[voice]` table, made one if the key is absent or isn't a table.
fn voice_mut(table: &mut Table) -> &mut Table {
    let voice = table
        .entry(VOICE)
        .or_insert_with(|| Value::Table(Table::new()));
    if !voice.is_table() {
        *voice = Value::Table(Table::new());
    }
    voice.as_table_mut().expect("the voice key holds a table")
}

/// The settings a table holds. A known key with a bad value reads as its default, or as off for
/// an on/off key, and stays in the table untouched until the user sets that key.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "what an absent onboarded key reads as, fixed at start"
)]
fn view_of(table: &Table, main_thread: ThreadId, in_use: bool) -> SettingsView {
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
    let onboarded = onboarded(table).unwrap_or(in_use);
    let max_frame_rate = match table.get(MAX_FRAME_RATE) {
        None => MaxFrameRate::AUTOMATIC,
        Some(Value::Integer(fps)) => u16::try_from(*fps)
            .ok()
            .and_then(|fps| MaxFrameRate::cap(fps).ok())
            .or_else(|| invalid(MAX_FRAME_RATE))
            .unwrap_or_default(),
        Some(_) => invalid(MAX_FRAME_RATE).unwrap_or_default(),
    };
    let default_model = match default_choice_of(table) {
        choice @ StoredChoice::Valid { .. } => choice.display().ok().flatten(),
        _ if table.contains_key(DEFAULT_MODEL) => invalid(DEFAULT_MODEL),
        _ => None,
    };
    let dock_position = table
        .get(DOCK_POSITION)
        .and_then(|value| {
            value
                .clone()
                .try_into::<DockPosition>()
                .ok()
                .or_else(|| invalid(DOCK_POSITION))
        })
        .unwrap_or_default();
    let show_in_taskbar = match table.get(SHOW_IN_TASKBAR) {
        None => false,
        Some(Value::Boolean(shown)) => *shown,
        Some(_) => invalid(SHOW_IN_TASKBAR).unwrap_or(false),
    };
    let voice = match table.get(VOICE) {
        None => None,
        Some(Value::Table(voice)) => Some(voice),
        Some(_) => invalid(VOICE),
    };
    let talk_back = match voice.and_then(|voice| voice.get(TALK_BACK)) {
        None => false,
        Some(Value::Boolean(on)) => *on,
        Some(_) => invalid(TALK_BACK).unwrap_or(false),
    };
    let stt_unload = match voice.and_then(|voice| voice.get(STT_UNLOAD)) {
        None => SttUnload::Kept,
        Some(value) => match value.as_str() {
            Some("kept") => SttUnload::Kept,
            Some("15m") => SttUnload::After15Min,
            Some("1h") => SttUnload::After1Hour,
            _ => invalid(STT_UNLOAD).unwrap_or_default(),
        },
    };
    let hold_to_talk = match voice.and_then(|voice| voice.get(HOLD_TO_TALK)) {
        Some(Value::Boolean(on)) => *on,
        None => kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
        Some(_) => invalid(HOLD_TO_TALK).unwrap_or(false),
    };
    let screen_holds = match voice.and_then(|voice| voice.get(SCREEN_HOLDS)) {
        Some(Value::Boolean(on)) => *on,
        None => kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
        Some(_) => invalid(SCREEN_HOLDS).unwrap_or(false),
    };
    // Its own key. Without it the setting starts on, except for someone who refused the
    // window's picture, a stored off or a value that reads as one, who isn't switched to the
    // whole screen unasked.
    let show_screen = match voice.and_then(|voice| voice.get(SHOW_SCREEN)) {
        Some(Value::Boolean(on)) => *on,
        None => screen_holds && kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
        Some(_) => invalid(SHOW_SCREEN).unwrap_or(false),
    };
    SettingsView {
        assistant_name,
        user_name,
        onboarded,
        onboarding_from: onboarding_from(table),
        main_thread,
        max_frame_rate,
        default_model,
        dock_position,
        show_in_taskbar,
        talk_back,
        stt_unload,
        hold_to_talk,
        screen_holds,
        show_screen,
    }
}

/// The `onboarded` key; none when it's absent or bad.
fn onboarded(table: &Table) -> Option<bool> {
    match table.get(ONBOARDED) {
        None => None,
        Some(Value::Boolean(done)) => Some(*done),
        Some(_) => invalid(ONBOARDED),
    }
}

/// The `onboarding_from` key; the first step when it's absent or bad.
fn onboarding_from(table: &Table) -> OnboardingStep {
    table
        .get(ONBOARDING_FROM)
        .and_then(|value| {
            (value.clone().try_into::<OnboardingStep>().ok()).or_else(|| invalid(ONBOARDING_FROM))
        })
        .unwrap_or_default()
}

fn default_choice_of(table: &Table) -> StoredChoice {
    let Some(value) = table.get(DEFAULT_MODEL) else {
        return StoredChoice::Absent;
    };
    let connection = match value.get("connection") {
        None => "chatgpt",
        Some(Value::String(kind)) => match kind.as_str() {
            "ChatGpt" => "chatgpt",
            "ClaudeCode" => "claude_code",
            "AnthropicKey" => "anthropic_key",
            _ => return StoredChoice::Invalid,
        },
        Some(_) => return StoredChoice::Invalid,
    };
    let effort = value
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    match StoredChoice::decode(
        value.get("model").and_then(Value::as_str),
        effort.as_deref(),
        Some(connection),
    ) {
        // ChatGPT's own malformed intent reads as unset, as before, so its catalog repairs it.
        StoredChoice::Invalid if connection == "chatgpt" => StoredChoice::Absent,
        choice => choice,
    }
}

fn invalid<T>(key: &str) -> Option<T> {
    tracing::warn!(key, "invalid setting; using the default");
    None
}

#[cfg(test)]
mod tests;
