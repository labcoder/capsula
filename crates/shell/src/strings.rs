//! User-facing text, from `locales/en/`. English only for now.

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use gpui_kit::Global;
use kumochi_protocol::message::{ErrorArg, ErrorKey, UserError};
use unic_langid::LanguageIdentifier;

const ENGLISH: &str = include_str!("../../../locales/en/shell.ftl");

/// Every string the shell shows. A unit test checks each one is in the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
    Retry,
    Stop,
    Quit,
    Chat,
    DockPosition,
    TopLeft,
    TopCenter,
    TopRight,
    MiddleLeft,
    Center,
    MiddleRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
    TurnBusy,
    TurnOffline,
    TurnTooLong,
    Threads,
    OpenThreads,
    NewThread,
    SearchThreads,
    EmptyThreads,
    EmptyThreadSearch,
    ThreadGone,
    SelectThread,
    ThreadCut,
    OlderMessages,
    CloseThreads,
    ThreadUser,
    ThreadAssistant,
    ThreadStep,
    ThreadsOffline,
    ThreadSearchTooLong,
    ThreadsFailed,
    DeleteThread,
    DeleteThreadTitle,
    DeleteThreadBody,
    DeleteThreadCancel,
    ThreadDeleting,
    ThreadDeleteFailed,
    ThreadDeletesFull,
    BackToKumo,
    SwitchThread,
    RenameThread,
    SaveRename,
    CancelRename,
    RenameEmpty,
    RenameTooLong,
    RenameNameTooLong,
    RenameOneLine,
    TaskFallback,
    Settings,
    OpenSettings,
    CloseSettings,
    General,
    Accounts,
    AssistantName,
    UserName,
    UserNameHelp,
    SettingsSaving,
    SettingsOffline,
    DockError,
    RenameBusy,
    SettingsUnavailable,
    SettingsNotSaved,
    SettingsSaveHelp,
    DockBrainError,
    MaxFrameRate,
    FrameRateAutomatic,
    AccountProvider,
    AccountWaiting,
    AccountOffline,
    AccountUsePlan,
    AccountContinue,
    AccountSignedOut,
    AccountSigningIn,
    AccountReplaceHelp,
    AccountCancel,
    AccountOpenAgain,
    AccountBusy,
    AccountBusyHelp,
    AccountTryAgain,
    AccountIdentity,
    AccountUsingPlan,
    AccountPlanOff,
    AccountEnable,
    AccountUnavailable,
    AccountAttention,
    AccountRetryHelp,
    AccountSignInAgain,
    AccountSessionEnded,
    AccountManageUsage,
    AccountSignOut,
    AccountSignOutTitle,
    AccountSignOutBody,
    AccountSignOutRegistration,
    AccountSignOutConfirm,
    AccountDialogCancel,
    AccountSigningOut,
    AccountStopped,
    AccountCleanupRefused,
    AccountNotDurable,
    AccountRevocationPending,
    AccountRevocationUnconfirmed,
    AccountSendFailed,
}

impl Text {
    #[cfg(test)]
    const ALL: [Self; 109] = [
        Self::Connecting,
        Self::Connected,
        Self::Reconnecting,
        Self::Failed,
        Self::Retry,
        Self::Stop,
        Self::Quit,
        Self::Chat,
        Self::DockPosition,
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::MiddleLeft,
        Self::Center,
        Self::MiddleRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
        Self::TurnBusy,
        Self::TurnOffline,
        Self::TurnTooLong,
        Self::Threads,
        Self::OpenThreads,
        Self::NewThread,
        Self::SearchThreads,
        Self::EmptyThreads,
        Self::EmptyThreadSearch,
        Self::ThreadGone,
        Self::SelectThread,
        Self::ThreadCut,
        Self::OlderMessages,
        Self::CloseThreads,
        Self::ThreadUser,
        Self::ThreadAssistant,
        Self::ThreadStep,
        Self::ThreadsOffline,
        Self::ThreadSearchTooLong,
        Self::ThreadsFailed,
        Self::DeleteThread,
        Self::DeleteThreadTitle,
        Self::DeleteThreadBody,
        Self::DeleteThreadCancel,
        Self::ThreadDeleting,
        Self::ThreadDeleteFailed,
        Self::ThreadDeletesFull,
        Self::BackToKumo,
        Self::SwitchThread,
        Self::RenameThread,
        Self::SaveRename,
        Self::CancelRename,
        Self::RenameEmpty,
        Self::RenameTooLong,
        Self::RenameNameTooLong,
        Self::RenameOneLine,
        Self::TaskFallback,
        Self::Settings,
        Self::OpenSettings,
        Self::CloseSettings,
        Self::General,
        Self::Accounts,
        Self::AssistantName,
        Self::UserName,
        Self::UserNameHelp,
        Self::SettingsSaving,
        Self::SettingsOffline,
        Self::DockError,
        Self::RenameBusy,
        Self::SettingsUnavailable,
        Self::SettingsNotSaved,
        Self::SettingsSaveHelp,
        Self::DockBrainError,
        Self::MaxFrameRate,
        Self::FrameRateAutomatic,
        Self::AccountProvider,
        Self::AccountWaiting,
        Self::AccountOffline,
        Self::AccountUsePlan,
        Self::AccountContinue,
        Self::AccountSignedOut,
        Self::AccountSigningIn,
        Self::AccountReplaceHelp,
        Self::AccountCancel,
        Self::AccountOpenAgain,
        Self::AccountBusy,
        Self::AccountBusyHelp,
        Self::AccountTryAgain,
        Self::AccountIdentity,
        Self::AccountUsingPlan,
        Self::AccountPlanOff,
        Self::AccountEnable,
        Self::AccountUnavailable,
        Self::AccountAttention,
        Self::AccountRetryHelp,
        Self::AccountSignInAgain,
        Self::AccountSessionEnded,
        Self::AccountManageUsage,
        Self::AccountSignOut,
        Self::AccountSignOutTitle,
        Self::AccountSignOutBody,
        Self::AccountSignOutRegistration,
        Self::AccountSignOutConfirm,
        Self::AccountDialogCancel,
        Self::AccountSigningOut,
        Self::AccountStopped,
        Self::AccountCleanupRefused,
        Self::AccountNotDurable,
        Self::AccountRevocationPending,
        Self::AccountRevocationUnconfirmed,
        Self::AccountSendFailed,
    ];

    pub(crate) const POSITIONS: [Self; 9] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::MiddleLeft,
        Self::Center,
        Self::MiddleRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive literal table keeps every localized ID together"
    )]
    const fn id(self) -> &'static str {
        match self {
            Self::AccountProvider => "accounts-provider",
            Self::AccountWaiting => "accounts-waiting",
            Self::AccountOffline => "accounts-offline",
            Self::AccountUsePlan => "accounts-use-plan",
            Self::AccountContinue => "accounts-continue",
            Self::AccountSignedOut => "accounts-signed-out",
            Self::AccountSigningIn => "accounts-signing-in",
            Self::AccountReplaceHelp => "accounts-replace-help",
            Self::AccountCancel => "accounts-cancel",
            Self::AccountOpenAgain => "accounts-open-again",
            Self::AccountBusy => "accounts-busy",
            Self::AccountBusyHelp => "accounts-busy-help",
            Self::AccountTryAgain => "accounts-try-again",
            Self::AccountIdentity => "accounts-identity",
            Self::AccountUsingPlan => "accounts-using-plan",
            Self::AccountPlanOff => "accounts-plan-off",
            Self::AccountEnable => "accounts-enable",
            Self::AccountUnavailable => "accounts-unavailable",
            Self::AccountAttention => "accounts-attention",
            Self::AccountRetryHelp => "accounts-retry-help",
            Self::AccountSignInAgain => "accounts-sign-in-again",
            Self::AccountSessionEnded => "accounts-session-ended",
            Self::AccountManageUsage => "accounts-manage-usage",
            Self::AccountSignOut => "accounts-sign-out",
            Self::AccountSignOutTitle => "accounts-sign-out-title",
            Self::AccountSignOutBody => "accounts-sign-out-body",
            Self::AccountSignOutRegistration => "accounts-sign-out-registration",
            Self::AccountSignOutConfirm => "accounts-sign-out-confirm",
            Self::AccountDialogCancel => "accounts-dialog-cancel",
            Self::AccountSigningOut => "accounts-signing-out",
            Self::AccountStopped => "accounts-stopped",
            Self::AccountCleanupRefused => "accounts-cleanup-refused",
            Self::AccountNotDurable => "accounts-not-durable",
            Self::AccountRevocationPending => "accounts-revocation-pending",
            Self::AccountRevocationUnconfirmed => "accounts-revocation-unconfirmed",
            Self::AccountSendFailed => "accounts-send-failed",
            Self::Connecting => "status-connecting",
            Self::Connected => "status-connected",
            Self::Reconnecting => "status-reconnecting",
            Self::Failed => "status-failed",
            Self::Retry => "action-retry",
            Self::Stop => "action-stop",
            Self::Quit => "action-quit",
            Self::Chat => "action-chat",
            Self::DockPosition => "dock-position",
            Self::TopLeft => "position-top-left",
            Self::TopCenter => "position-top-center",
            Self::TopRight => "position-top-right",
            Self::MiddleLeft => "position-middle-left",
            Self::Center => "position-center",
            Self::MiddleRight => "position-middle-right",
            Self::BottomLeft => "position-bottom-left",
            Self::BottomCenter => "position-bottom-center",
            Self::BottomRight => "position-bottom-right",
            Self::TurnBusy => "turn-busy",
            Self::TurnOffline => "turn-offline",
            Self::TurnTooLong => "turn-too-long",
            Self::Threads => "threads-title",
            Self::OpenThreads => "action-threads",
            Self::NewThread => "action-new-thread",
            Self::SearchThreads => "threads-search",
            Self::EmptyThreads => "threads-empty",
            Self::EmptyThreadSearch => "threads-search-empty",
            Self::ThreadGone => "thread-gone",
            Self::SelectThread => "thread-select",
            Self::ThreadCut => "thread-cut",
            Self::OlderMessages => "thread-older",
            Self::CloseThreads => "threads-close",
            Self::ThreadUser => "thread-user",
            Self::ThreadAssistant => "thread-assistant",
            Self::ThreadStep => "thread-step",
            Self::ThreadsOffline => "threads-offline",
            Self::ThreadSearchTooLong => "threads-search-too-long",
            Self::ThreadsFailed => "threads-failed",
            Self::DeleteThread => "thread-delete",
            Self::DeleteThreadTitle => "thread-delete-title",
            Self::DeleteThreadBody => "thread-delete-body",
            Self::DeleteThreadCancel => "thread-delete-cancel",
            Self::ThreadDeleting => "thread-deleting",
            Self::ThreadDeleteFailed => "thread-delete-failed",
            Self::ThreadDeletesFull => "thread-deletes-full",
            Self::BackToKumo => "action-back-to-kumo",
            Self::SwitchThread => "action-switch-task",
            Self::RenameThread => "action-rename-task",
            Self::SaveRename => "action-save-rename",
            Self::CancelRename => "action-cancel-rename",
            Self::RenameEmpty => "rename-empty",
            Self::RenameTooLong => "rename-task-too-long",
            Self::RenameNameTooLong => "rename-name-too-long",
            Self::RenameOneLine => "rename-one-line",
            Self::TaskFallback => "task-fallback",
            Self::Settings => "settings-title",
            Self::OpenSettings => "action-settings",
            Self::CloseSettings => "settings-close",
            Self::General => "settings-general",
            Self::Accounts => "settings-accounts",
            Self::AssistantName => "settings-assistant-name",
            Self::UserName => "settings-user-name",
            Self::UserNameHelp => "settings-user-name-help",
            Self::SettingsSaving => "settings-saving",
            Self::SettingsOffline => "settings-offline",
            Self::DockError => "dock-error",
            Self::RenameBusy => "rename-busy",
            Self::SettingsUnavailable => "settings-unavailable",
            Self::SettingsNotSaved => "settings-not-saved",
            Self::SettingsSaveHelp => "settings-save-help",
            Self::DockBrainError => "dock-brain-error",
            Self::MaxFrameRate => "settings-max-frame-rate",
            Self::FrameRateAutomatic => "settings-frame-rate-automatic",
        }
    }
}

/// The loaded catalog, kept as a GPUI global on the UI thread.
pub(crate) struct Strings {
    bundle: FluentBundle<FluentResource>,
}

impl std::fmt::Debug for Strings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Strings").finish_non_exhaustive()
    }
}

impl Global for Strings {}

impl Strings {
    /// Loads the English catalog compiled into the binary.
    pub(crate) fn english() -> Self {
        let resource = FluentResource::try_new(ENGLISH.to_owned())
            .expect("the bundled English catalog parses; strings::tests checks it");
        let language: LanguageIdentifier = "en".parse().expect("\"en\" is a valid language id");
        let mut bundle = FluentBundle::new(vec![language]);
        // shortcut: isolation is off because GPUI doesn't prove FSI/PDI render invisibly; revisit with the first right-to-left locale.
        bundle.set_use_isolating(false);
        bundle
            .add_resource(resource)
            .expect("the bundled catalog has no duplicate ids; strings::tests checks it");
        Self { bundle }
    }

    /// The text for `text`. A missing entry shows its id, which the unit test rules out.
    pub(crate) fn get(&self, text: Text) -> String {
        self.format(text.id(), None)
    }

    pub(crate) fn assistant(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        self.format(Text::ThreadAssistant.id(), Some(&args))
    }

    pub(crate) fn back_to_kumo(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        self.format(Text::BackToKumo.id(), Some(&args))
    }

    pub(crate) fn frame_rate(&self, fps: f64) -> String {
        let mut args = FluentArgs::new();
        args.set("fps", (fps * 10.0).round() / 10.0);
        self.format("settings-frame-rate-now", Some(&args))
    }

    pub(crate) fn focus(&self, task: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("task", task);
        self.format("task-focus", Some(&args))
    }

    /// The text for an error the brain sent, with its typed arguments.
    pub(crate) fn error(&self, error: &UserError) -> String {
        let mut args = FluentArgs::new();
        for arg in error.args() {
            match *arg {
                ErrorArg::Seconds(seconds) => args.set("seconds", seconds),
                ErrorArg::Count(count) => args.set("count", count),
            }
        }
        self.format(error_id(error.key()), Some(&args))
    }

    fn format(&self, id: &'static str, args: Option<&FluentArgs<'_>>) -> String {
        let Some(pattern) = self.bundle.get_message(id).and_then(|m| m.value()) else {
            tracing::error!(id, "missing string");
            return id.to_owned();
        };
        let mut errors = Vec::new();
        let value = self
            .bundle
            .format_pattern(pattern, args, &mut errors)
            .into_owned();
        if !errors.is_empty() {
            tracing::error!(id, ?errors, "string failed to format");
        }
        value
    }
}

/// Each error's catalog id. Append only, beside `ErrorKey`.
const fn error_id(key: ErrorKey) -> &'static str {
    match key {
        ErrorKey::SettingsRefused => "error-settings-refused",
        ErrorKey::ConfigUnreadable => "error-config-unreadable",
        ErrorKey::SignInTimedOut => "error-sign-in-timed-out",
        ErrorKey::SignInFailed => "error-sign-in-failed",
        ErrorKey::SignInOtherAccount => "error-sign-in-other-account",
        ErrorKey::BrowserUnavailable => "error-browser-unavailable",
        ErrorKey::AccountOffline => "error-account-offline",
        ErrorKey::CredentialsUnavailable => "error-credentials-unavailable",
        ErrorKey::RevocationBacklog => "error-revocation-backlog",
        ErrorKey::TrustStoreUnavailable => "error-trust-store-unavailable",
        ErrorKey::PlanNotEnabled => "error-plan-not-enabled",
        ErrorKey::AccountMisconfigured => "error-account-misconfigured",
        ErrorKey::SignOutNotDurable => "error-sign-out-not-durable",
        ErrorKey::SessionEnded => "error-session-ended",
        ErrorKey::PlanUnavailable => "error-plan-unavailable",
        ErrorKey::UsageLimit => "error-usage-limit",
        ErrorKey::AccountRefused => "error-account-refused",
        ErrorKey::RequestRefused => "error-request-refused",
        ErrorKey::ResponseUnreadable => "error-response-unreadable",
        ErrorKey::AnthropicKeyRejected => "error-anthropic-key-rejected",
        ErrorKey::AnthropicKeyFormat => "error-anthropic-key-format",
        ErrorKey::AnthropicUnreachable => "error-anthropic-unreachable",
        ErrorKey::AnthropicRateLimited => "error-anthropic-rate-limited",
        ErrorKey::AnthropicCredit => "error-anthropic-credit",
        ErrorKey::ClaudeCodeNotFound => "error-claude-code-not-found",
        ErrorKey::ClaudeCodeUnsupported => "error-claude-code-unsupported",
        ErrorKey::ClaudeCodeSignedOut => "error-claude-code-signed-out",
        ErrorKey::ClaudeCodeAuditFailed => "error-claude-code-audit-failed",
        ErrorKey::ClaudeCodePolicy => "error-claude-code-policy",
        ErrorKey::ClaudeCodeInputUnsupported => "error-claude-code-input-unsupported",
        ErrorKey::ClaudeCodeBudget => "error-claude-code-budget",
        ErrorKey::ClaudeCodeCleanupPending => "error-claude-code-cleanup-pending",
    }
}

#[cfg(test)]
mod tests {
    use super::{Strings, Text, error_id};
    use fluent_bundle::{FluentBundle, FluentResource};
    use kumochi_protocol::message::{ErrorArg, ErrorKey, UserError};

    #[test]
    fn every_text_is_in_the_catalog() {
        let strings = Strings::english();
        for text in Text::ALL {
            assert!(
                strings
                    .bundle
                    .get_message(text.id())
                    .and_then(|m| m.value())
                    .is_some(),
                "{text:?} has no value in locales/en/shell.ftl",
            );
        }
        assert_eq!(strings.get(Text::Connected), "Ready");
    }

    #[test]
    fn every_error_is_in_the_catalog() {
        let strings = Strings::english();
        for key in ErrorKey::ALL {
            assert!(
                strings
                    .bundle
                    .get_message(error_id(key))
                    .and_then(|m| m.value())
                    .is_some(),
                "{key:?} has no value in locales/en/shell.ftl",
            );
        }
        assert_eq!(
            strings.error(&UserError::from(ErrorKey::SettingsRefused)),
            "Couldn't save that setting. Try again."
        );
    }

    #[test]
    fn an_error_fills_in_each_argument_by_name() {
        let id = error_id(ErrorKey::SettingsRefused);
        let ftl = format!("{id} = Wait {{ $seconds }} s for {{ $count }} more\n");
        let resource = FluentResource::try_new(ftl).expect("the test message parses");
        let mut bundle = FluentBundle::new(vec!["en".parse().expect("valid")]);
        bundle.set_use_isolating(false);
        bundle.add_resource(resource).expect("one message");
        let strings = Strings { bundle };
        let error = UserError::new(
            ErrorKey::SettingsRefused,
            vec![ErrorArg::Count(3), ErrorArg::Seconds(30)],
            Vec::new(),
        )
        .expect("within the limits");
        assert_eq!(strings.error(&error), "Wait 30 s for 3 more");
    }

    #[test]
    fn names_and_task_context_have_no_isolation_marks() {
        let strings = Strings::english();
        assert_eq!(strings.assistant("Cloud"), "Cloud");
        assert_eq!(strings.back_to_kumo("Cloud"), "Back to Cloud");
        assert_eq!(strings.focus("Birthday"), "You're in: Birthday");
    }

    #[test]
    fn the_frame_rate_shows_its_fraction() {
        let strings = Strings::english();
        assert_eq!(
            strings.frame_rate(72.0),
            "Drawing at 72 frames a second on this display."
        );
        assert_eq!(
            strings.frame_rate(28.8),
            "Drawing at 28.8 frames a second on this display."
        );
        assert_eq!(
            strings.frame_rate(170.0 / 3.0),
            "Drawing at 56.7 frames a second on this display."
        );
    }
}
