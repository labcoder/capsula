//! User-facing text, from `locales/en/`. English only for now.

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use gpui_kit::Global;
use kumochi_protocol::message::{ErrorArg, ErrorKey, UserError};
use unic_langid::LanguageIdentifier;

const ENGLISH: &str = include_str!("../../../locales/en/shell.ftl");
mod claude;
mod issues;

/// Every string the shell shows. A unit test checks each one is in the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    // Shell
    Connecting,
    Connected,
    Reconnecting,
    Failed,
    Retry,
    Stop,
    Quit,
    Chat,

    // Dock
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
    DockError,
    DockBrainError,
    PerchOn,
    PerchUntitled,

    // Turns
    RecoverySignIn,
    DismissNotice,
    ChatLiveCut,
    ChatEmpty,
    ChatLoading,
    ChatHistoryFailed,
    TurnBusy,
    TurnOffline,
    TurnTooLong,

    // Tasks
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
    ThreadNoVisual,
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
    ThreadDeleted,
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

    IssueLoading,
    IssueUnavailable,
    IssueDetailsLoading,
    IssueDetailsUnavailable,
    IssueReadOnly,
    IssueConsentTitle,
    #[cfg(test)]
    IssueConsentOpen,
    IssueConsentConsequence,
    IssueConsentCancel,
    IssueConsentConfirm,
    IssueDuplicate,

    // Settings
    Settings,
    OpenSettings,
    CloseSettings,
    General,
    Accounts,
    GitHubWaiting,
    GitHubSignedOut,
    GitHubRemoving,
    GitHubRemovalFailed,
    GitHubNotDurable,
    GitHubRequesting,
    GitHubAwaiting,
    GitHubConnected,
    GitHubNotInstalled,
    GitHubNeedsSignIn,
    GitHubNotVerified,
    GitHubCheckAgain,
    GitHubConnect,
    GitHubSignInAgain,
    GitHubRefresh,
    GitHubSignOut,
    GitHubTimeout,
    GitHubFailure,
    GitHubOffline,
    GitHubCredentials,
    GitHubRefused,
    GitHubUnavailable,
    GitHubProvider,
    GitHubConfiguration,
    GitHubSavingDefault,
    GitHubDefaultRetained,
    GitHubDefaultRepository,
    GitHubChooseRepository,
    GitHubEmptyRepositories,
    GitHubTruncatedRepositories,
    GitHubVerification,
    GitHubCodeExpired,
    GitHubCanceling,
    GitHubRefreshing,
    Voice,
    SpeechInput,
    VoiceIdlePolicy,
    VoiceKept,
    Voice15Minutes,
    Voice1Hour,
    VoiceIdleHelp,
    VoiceStatusUnknown,
    VoiceNotInstalled,
    VoiceVerifying,
    VoiceDownloading,
    VoiceLoading,
    VoiceReady,
    VoiceUnloaded,
    VoiceRestartRequired,
    VoiceInstalledFailed,
    VoiceNotInstalledFailed,
    VoiceDefaultModel,
    VoiceInstallModel,
    VoiceRetryModel,
    VoiceLoadModel,
    VoiceCancelSetup,
    VoiceCancelHelp,
    VoiceCancelAccessible,
    VoiceRequestPending,
    AssistantName,
    UserName,
    UserNameHelp,
    SettingsSaving,
    SettingsOffline,
    RenameBusy,
    SettingsUnavailable,
    SettingsNotSaved,
    SettingsSaveHelp,
    SettingsVersion,
    SettingsCopyBuild,
    SettingsPositionUnavailable,
    SettingsTaskbar,
    TaskbarAddFailed,
    TaskbarRemoveFailed,
    MaxFrameRate,
    FrameRateAutomatic,

    // Onboarding
    OnboardingClose,
    OnboardingContinue,
    OnboardingWelcomeHelp,

    // ChatGPT accounts
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
    AccountLocal,
    AccountRemote,
    AccountSignOutNeedsRetry,
    AccountCleanupRefusedHelp,
    AccountNotDurableHelp,
    AccountBacklogLead,
    AccountBacklogRetry,
    AccountBacklogExpiry,
    AccountBacklogRecovery,

    // Claude Code
    ClaudeProvider,
    ClaudeReview,
    ClaudeConnect,
    ClaudeDisconnect,
    ClaudeFind,
    ClaudePolicyNotice,
    ClaudeBefore,
    ClaudeDuring,
    ClaudeFallback,
    ClaudeDeadline,
    ClaudeAnalytics,
    ClaudeDisconnectBody,
}

#[cfg(test)]
macro_rules! catalog {
    ($($key:ident),* $(,)?) => {
        const ALL: [Self; [$(stringify!($key)),*].len()] = [$(Self::$key),*];

        const fn listed(self) -> bool {
            match self {
                $(Self::$key)|* => true,
            }
        }
    };
}

impl Text {
    #[cfg(test)]
    catalog! {
        // Shell
        Connecting,
        Connected,
        Reconnecting,
        Failed,
        Retry,
        Stop,
        Quit,
        Chat,

        // Dock
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
        DockError,
        DockBrainError,
        PerchOn,
        PerchUntitled,

        // Turns
        RecoverySignIn,
        DismissNotice,
        ChatLiveCut,
        ChatEmpty,
        ChatLoading,
        ChatHistoryFailed,
        TurnBusy,
        TurnOffline,
        TurnTooLong,

        // Tasks
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
        ThreadNoVisual,
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
        ThreadDeleted,
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

        IssueLoading,
        IssueUnavailable,
        IssueDetailsLoading,
        IssueDetailsUnavailable,
        IssueReadOnly,
        IssueConsentTitle,
        IssueConsentOpen,
        IssueConsentConsequence,
        IssueConsentCancel,
        IssueConsentConfirm,
        IssueDuplicate,

        // Settings
        Settings,
        OpenSettings,
        CloseSettings,
        General,
        Accounts,
        GitHubWaiting,
        GitHubSignedOut,
        GitHubRemoving,
        GitHubRemovalFailed,
        GitHubNotDurable,
        GitHubRequesting,
        GitHubAwaiting,
        GitHubConnected,
        GitHubNotInstalled,
        GitHubNeedsSignIn,
        GitHubNotVerified,
        GitHubCheckAgain,
        GitHubConnect,
        GitHubSignInAgain,
        GitHubRefresh,
        GitHubSignOut,
        GitHubTimeout,
        GitHubFailure,
        GitHubOffline,
        GitHubCredentials,
        GitHubRefused,
        GitHubUnavailable,
        GitHubProvider,
        GitHubConfiguration,
        GitHubSavingDefault,
        GitHubDefaultRetained,
        GitHubDefaultRepository,
        GitHubChooseRepository,
        GitHubEmptyRepositories,
        GitHubTruncatedRepositories,
        GitHubVerification,
        GitHubCodeExpired,
        GitHubCanceling,
        GitHubRefreshing,
        Voice,
        SpeechInput,
        VoiceIdlePolicy,
        VoiceKept,
        Voice15Minutes,
        Voice1Hour,
        VoiceIdleHelp,
        VoiceStatusUnknown,
        VoiceNotInstalled,
        VoiceVerifying,
        VoiceDownloading,
        VoiceLoading,
        VoiceReady,
        VoiceUnloaded,
        VoiceRestartRequired,
        VoiceInstalledFailed,
        VoiceNotInstalledFailed,
        VoiceDefaultModel,
        VoiceInstallModel,
        VoiceRetryModel,
        VoiceLoadModel,
        VoiceCancelSetup,
        VoiceCancelHelp,
        VoiceCancelAccessible,
        VoiceRequestPending,
        AssistantName,
        UserName,
        UserNameHelp,
        SettingsSaving,
        SettingsOffline,
        RenameBusy,
        SettingsUnavailable,
        SettingsNotSaved,
        SettingsSaveHelp,
        SettingsVersion,
        SettingsCopyBuild,
        SettingsPositionUnavailable,
        SettingsTaskbar,
        TaskbarAddFailed,
        TaskbarRemoveFailed,
        MaxFrameRate,
        FrameRateAutomatic,

        // Onboarding
        OnboardingClose,
        OnboardingContinue,
        OnboardingWelcomeHelp,

        // ChatGPT accounts
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
        AccountLocal,
        AccountRemote,
        AccountSignOutNeedsRetry,
        AccountCleanupRefusedHelp,
        AccountNotDurableHelp,
        AccountBacklogLead,
        AccountBacklogRetry,
        AccountBacklogExpiry,
        AccountBacklogRecovery,

        // Claude Code
        ClaudeProvider,
        ClaudeReview,
        ClaudeConnect,
        ClaudeDisconnect,
        ClaudeFind,
        ClaudePolicyNotice,
        ClaudeBefore,
        ClaudeDuring,
        ClaudeFallback,
        ClaudeDeadline,
        ClaudeAnalytics,
        ClaudeDisconnectBody,
    }

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
            // Shell
            Self::Connecting => "status-connecting",
            Self::Connected => "status-connected",
            Self::Reconnecting => "status-reconnecting",
            Self::Failed => "status-failed",
            Self::Retry => "action-retry",
            Self::Stop => "action-stop",
            Self::Quit => "action-quit",
            Self::Chat => "action-chat",

            // Dock
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
            Self::DockError => "dock-error",
            Self::DockBrainError => "dock-brain-error",
            Self::PerchOn => "carry-perch-on",
            Self::PerchUntitled => "carry-perch-untitled",

            // Turns
            Self::RecoverySignIn => "recovery-sign-in",
            Self::DismissNotice => "dismiss-notice",
            Self::ChatLiveCut => "chat-live-cut",
            Self::ChatEmpty => "chat-empty",
            Self::ChatLoading => "chat-loading",
            Self::ChatHistoryFailed => "chat-history-failed",
            Self::TurnBusy => "turn-busy",
            Self::TurnOffline => "turn-offline",
            Self::TurnTooLong => "turn-too-long",

            // Tasks
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
            Self::ThreadNoVisual => "thread-no-visual",
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
            Self::ThreadDeleted => "thread-deleted",
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

            Self::IssueLoading => "issue-loading",
            Self::IssueUnavailable => "issue-unavailable",
            Self::IssueDetailsLoading => "issue-details-loading",
            Self::IssueDetailsUnavailable => "issue-details-unavailable",
            Self::IssueReadOnly => "issue-read-only",
            Self::IssueConsentTitle => "issue-consent-title",
            #[cfg(test)]
            Self::IssueConsentOpen => "issue-consent-open",
            Self::IssueConsentConsequence => "issue-consent-consequence",
            Self::IssueConsentCancel => "issue-consent-cancel",
            Self::IssueConsentConfirm => "issue-consent-confirm",
            Self::IssueDuplicate => "issue-duplicate",

            // Settings
            Self::Settings => "settings-title",
            Self::OpenSettings => "action-settings",
            Self::CloseSettings => "settings-close",
            Self::General => "settings-general",
            Self::Accounts => "settings-accounts",
            Self::GitHubWaiting => "github-waiting",
            Self::GitHubSignedOut => "github-signed-out",
            Self::GitHubRemoving => "github-removing",
            Self::GitHubRemovalFailed => "github-removal-failed",
            Self::GitHubNotDurable => "github-not-durable",
            Self::GitHubRequesting => "github-requesting",
            Self::GitHubAwaiting => "github-awaiting",
            Self::GitHubConnected => "github-connected",
            Self::GitHubNotInstalled => "github-not-installed",
            Self::GitHubNeedsSignIn => "github-needs-sign-in",
            Self::GitHubNotVerified => "github-not-verified",
            Self::GitHubCheckAgain => "github-check-again",
            Self::GitHubConnect => "github-connect",
            Self::GitHubSignInAgain => "github-sign-in-again",
            Self::GitHubRefresh => "github-refresh",
            Self::GitHubSignOut => "github-sign-out",
            Self::GitHubTimeout => "github-timeout",
            Self::GitHubFailure => "github-failure",
            Self::GitHubOffline => "github-offline",
            Self::GitHubCredentials => "github-credentials",
            Self::GitHubRefused => "github-refused",
            Self::GitHubUnavailable => "github-unavailable",
            Self::GitHubProvider => "github-provider",
            Self::GitHubConfiguration => "github-configuration",
            Self::GitHubSavingDefault => "github-saving-default",
            Self::GitHubDefaultRetained => "github-default-retained",
            Self::GitHubDefaultRepository => "github-default-repository",
            Self::GitHubChooseRepository => "github-choose-repository",
            Self::GitHubEmptyRepositories => "github-empty-repositories",
            Self::GitHubTruncatedRepositories => "github-truncated-repositories",
            Self::GitHubVerification => "github-verification",
            Self::GitHubCodeExpired => "github-code-expired",
            Self::GitHubCanceling => "github-canceling",
            Self::GitHubRefreshing => "github-refreshing",
            Self::Voice => "settings-voice",
            Self::SpeechInput => "voice-speech-input",
            Self::VoiceIdlePolicy => "voice-idle-policy",
            Self::VoiceKept => "voice-idle-kept",
            Self::Voice15Minutes => "voice-idle-15m",
            Self::Voice1Hour => "voice-idle-1h",
            Self::VoiceIdleHelp => "voice-idle-help",
            Self::VoiceStatusUnknown => "voice-status-unknown",
            Self::VoiceNotInstalled => "voice-not-installed",
            Self::VoiceVerifying => "voice-verifying",
            Self::VoiceDownloading => "voice-downloading",
            Self::VoiceLoading => "voice-loading",
            Self::VoiceReady => "voice-ready",
            Self::VoiceUnloaded => "voice-unloaded",
            Self::VoiceRestartRequired => "voice-restart-required",
            Self::VoiceInstalledFailed => "voice-installed-failed",
            Self::VoiceNotInstalledFailed => "voice-not-installed-failed",
            Self::VoiceDefaultModel => "voice-default-model",
            Self::VoiceInstallModel => "voice-install-model",
            Self::VoiceRetryModel => "voice-retry-model",
            Self::VoiceLoadModel => "voice-load-model",
            Self::VoiceCancelSetup => "voice-cancel-setup",
            Self::VoiceCancelHelp => "voice-cancel-help",
            Self::VoiceCancelAccessible => "voice-cancel-accessible",
            Self::VoiceRequestPending => "voice-request-pending",
            Self::AssistantName => "settings-assistant-name",
            Self::UserName => "settings-user-name",
            Self::UserNameHelp => "settings-user-name-help",
            Self::SettingsSaving => "settings-saving",
            Self::SettingsOffline => "settings-offline",
            Self::RenameBusy => "rename-busy",
            Self::SettingsUnavailable => "settings-unavailable",
            Self::SettingsNotSaved => "settings-not-saved",
            Self::SettingsSaveHelp => "settings-save-help",
            Self::SettingsVersion => "settings-version",
            Self::SettingsCopyBuild => "settings-copy-build",
            Self::SettingsPositionUnavailable => "settings-position-unavailable",
            Self::SettingsTaskbar => "settings-taskbar",
            Self::TaskbarAddFailed => "taskbar-add-failed",
            Self::TaskbarRemoveFailed => "taskbar-remove-failed",
            Self::MaxFrameRate => "settings-max-frame-rate",
            Self::FrameRateAutomatic => "settings-frame-rate-automatic",

            // Onboarding
            Self::OnboardingClose => "onboarding-close",
            Self::OnboardingContinue => "onboarding-continue",
            Self::OnboardingWelcomeHelp => "onboarding-welcome-help",

            // ChatGPT accounts
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
            Self::AccountLocal => "accounts-local",
            Self::AccountRemote => "accounts-remote",
            Self::AccountSignOutNeedsRetry => "accounts-sign-out-needs-retry",
            Self::AccountCleanupRefusedHelp => "accounts-cleanup-refused-help",
            Self::AccountNotDurableHelp => "accounts-not-durable-help",
            Self::AccountBacklogLead => "accounts-backlog-lead",
            Self::AccountBacklogRetry => "accounts-backlog-retry",
            Self::AccountBacklogExpiry => "accounts-backlog-expiry",
            Self::AccountBacklogRecovery => "accounts-backlog-recovery",

            // Claude Code
            Self::ClaudeProvider => "claude-provider",
            Self::ClaudeReview => "claude-review",
            Self::ClaudeConnect => "claude-connect",
            Self::ClaudeDisconnect => "claude-disconnect",
            Self::ClaudeFind => "claude-find",
            Self::ClaudePolicyNotice => "claude-policy-notice",
            Self::ClaudeBefore => "claude-before",
            Self::ClaudeDuring => "claude-during",
            Self::ClaudeFallback => "claude-fallback",
            Self::ClaudeDeadline => "claude-deadline",
            Self::ClaudeAnalytics => "claude-analytics",
            Self::ClaudeDisconnectBody => "claude-disconnect-body",
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
    pub(crate) fn taskbar(&self) -> String {
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        self.format(Text::SettingsTaskbar.id(), Some(&args))
    }

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

    /// The onboarding window's heading, with the product name.
    pub(crate) fn onboarding_title(&self) -> String {
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        self.format("onboarding-title", Some(&args))
    }

    /// The welcome's words before and after the name input, which may sit anywhere in it. Both
    /// are trimmed, since the row spaces the input itself.
    pub(crate) fn onboarding_welcome(&self) -> (String, String) {
        const SPOT: &str = "\u{E000}";
        let mut args = FluentArgs::new();
        args.set("name", SPOT);
        let text = self.format("onboarding-welcome", Some(&args));
        match text.split_once(SPOT) {
            Some((before, after)) => (before.trim().to_owned(), after.trim().to_owned()),
            None => (text, String::new()),
        }
    }

    /// The question for the user's name and its help line, with the assistant's name.
    pub(crate) fn onboarding_user(&self, assistant: &str) -> [String; 2] {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        ["onboarding-user", "onboarding-user-help"].map(|id| self.format(id, Some(&args)))
    }

    pub(crate) fn issue_title(&self, title: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("title", title);
        self.format("thread-issue-title", Some(&args))
    }

    /// The delete dialog's body, with what Kumo keeps.
    pub(crate) fn delete_body(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        format!(
            "{} {}",
            self.get(Text::DeleteThreadBody),
            self.format("thread-delete-kumo", Some(&args))
        )
    }

    /// The drop label under a carried Kumo: `app` is the window's title, which is screen text.
    pub(crate) fn perch_on(&self, app: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("app", app);
        self.format(Text::PerchOn.id(), Some(&args))
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

/// Each error's catalog id, grouped by feature.
const fn error_id(key: ErrorKey) -> &'static str {
    match key {
        // Settings
        ErrorKey::SettingsRefused => "error-settings-refused",
        ErrorKey::ConfigUnreadable => "error-config-unreadable",

        // ChatGPT and provider errors
        ErrorKey::SignInTimedOut => "error-sign-in-timed-out",
        ErrorKey::SignInFailed => "error-sign-in-failed",
        ErrorKey::SignInOtherAccount => "error-sign-in-other-account",
        ErrorKey::BrowserUnavailable => "error-browser-unavailable",
        ErrorKey::AccountOffline => "error-account-offline",
        ErrorKey::CredentialsUnavailable => "error-credentials-unavailable",
        ErrorKey::RevocationBacklog => Text::AccountBacklogLead.id(),
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
        ErrorKey::SignInNeeded => "error-sign-in-needed",
        ErrorKey::ModelUnavailable => "error-model-unavailable",
        ErrorKey::RenewalUnconfirmed => "error-renewal-unconfirmed",

        // Anthropic
        ErrorKey::AnthropicKeyRejected => "error-anthropic-key-rejected",
        ErrorKey::AnthropicKeyFormat => "error-anthropic-key-format",
        ErrorKey::AnthropicUnreachable => "error-anthropic-unreachable",
        ErrorKey::AnthropicRateLimited => "error-anthropic-rate-limited",
        ErrorKey::AnthropicCredit => "error-anthropic-credit",
        ErrorKey::AnthropicAnswerLimit => "error-anthropic-answer-limit",

        // Claude Code
        ErrorKey::ClaudeCodeNotFound => "error-claude-code-not-found",
        ErrorKey::ClaudeCodeUnsupported => "error-claude-code-unsupported",
        ErrorKey::ClaudeCodeSignedOut => "error-claude-code-signed-out",
        ErrorKey::ClaudeCodeAuditFailed => "error-claude-code-audit-failed",
        ErrorKey::ClaudeCodePolicy => "error-claude-code-policy",
        ErrorKey::ClaudeCodeInputUnsupported => "error-claude-code-input-unsupported",
        ErrorKey::ClaudeCodeBudget => "error-claude-code-budget",
        ErrorKey::ClaudeCodeCleanupPending => "error-claude-code-cleanup-pending",

        // Voice
        ErrorKey::VoiceNoSpace => "error-voice-no-space",
        ErrorKey::VoiceDownload => "error-voice-download",
        ErrorKey::VoiceCorrupt => "error-voice-corrupt",
        ErrorKey::VoiceStorage => "error-voice-storage",
        ErrorKey::VoiceLoad => "error-voice-load",
        ErrorKey::VoiceUnavailable => "error-voice-unavailable",
        ErrorKey::VoiceModelPath => "error-voice-model-path",
        ErrorKey::GitHubUnavailable => "github-unavailable",
        ErrorKey::DrawingReplacementFailed => "error-drawing-replacement-failed",
        ErrorKey::DrawingNotReturned => "error-drawing-not-returned",
    }
}

#[cfg(test)]
mod tests {
    use super::{Strings, Text, error_id};
    use fluent_bundle::{FluentBundle, FluentResource};
    use kumochi_protocol::message::{ErrorArg, ErrorKey, UserError};

    #[test]
    fn model_paths_explain_the_local_folder_requirement_without_a_path() {
        let strings = Strings::english();
        assert_eq!(
            strings.error(&ErrorKey::VoiceModelPath.into()),
            "The speech model folder goes through a link or network location, or has unsafe permissions. Use an ordinary local folder."
        );
    }

    #[test]
    fn the_taskbar_label_uses_the_product_name() {
        assert_eq!(
            Strings::english().taskbar(),
            format!(
                "Also show {} in the taskbar",
                kumochi_protocol::constants::DISPLAY_NAME
            )
        );
    }

    #[test]
    fn onboarding_splits_the_welcome_at_the_name_and_fills_in_the_names() {
        let strings = Strings::english();
        assert_eq!(
            strings.onboarding_welcome(),
            ("Hi, I'm".to_owned(), String::new())
        );
        assert_eq!(
            strings.onboarding_title(),
            format!("Welcome to {}", kumochi_protocol::constants::DISPLAY_NAME)
        );
        assert_eq!(
            strings.onboarding_user("Mochi"),
            [
                "What should Mochi call you?".to_owned(),
                "Optional. It's sent to the model you chat with, so Mochi can use it.".to_owned()
            ]
        );
    }

    #[test]
    fn every_text_is_in_the_catalog() {
        let strings = Strings::english();
        for text in Text::ALL {
            assert!(text.listed());
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
    fn cancellation_scope_is_available_in_the_buttons_accessible_name() {
        let strings = Strings::english();
        assert_eq!(
            strings.error(&ErrorKey::VoiceLoad.into()),
            "Voice couldn't start."
        );
        assert_eq!(
            strings.get(Text::VoiceCancelHelp),
            "Applies to pending setup for all voice features. Some loading or finishing work cannot be stopped. Installed files are kept."
        );
        assert_eq!(
            strings.get(Text::VoiceCancelAccessible),
            "Cancel voice setup. Applies to pending setup for all voice features. Some loading or finishing work cannot be stopped. Installed files are kept."
        );
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
    fn backlog_notices_use_the_short_recovery_lead() {
        assert_eq!(
            Strings::english().error(&UserError::from(ErrorKey::RevocationBacklog)),
            "Still ending earlier ChatGPT sessions. A new sign-in can't start yet."
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
    fn the_deletion_note_never_names_the_task_and_the_dialog_says_names_stay() {
        let strings = Strings::english();
        assert_eq!(strings.get(Text::ThreadDeleted), "You deleted a task.");
        assert_eq!(
            strings.delete_body("Cloud"),
            "This removes the task and its drawings from this PC. It can't remove issues you \
             already filed on GitHub, or what your model provider keeps under its own terms. \
             Cloud keeps a note that a task was deleted. Anything already in Cloud's \
             conversation stays, including names it used."
        );
    }

    #[test]
    fn issue_history_fills_the_title_without_interpreting_it() {
        assert_eq!(
            Strings::english().issue_title("Fix {layout} — 窓"),
            "Issue: Fix {layout} — 窓"
        );
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
