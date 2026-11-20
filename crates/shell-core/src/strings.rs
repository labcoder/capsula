//! User-facing text, from `locales/en/`. English only for now.

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use kumochi_protocol::message::{ErrorArg, ErrorKey, HeldScreen, ScreenClass, UserError};
use unic_langid::LanguageIdentifier;

const ENGLISH: &str = include_str!("../../../locales/en/shell.ftl");
mod claude;
mod issues;

/// Declares [`Text`] from one table. Each line names a string and its ID in
/// `locales/en/shell.ftl`. The enum, the ID lookup and the test's list of every string all come
/// from it, so a new string is one line here and one message in the catalog.
macro_rules! texts {
    ($($key:ident => $id:literal),* $(,)?) => {
        /// Every string the shell shows. A unit test checks each one is in the catalog.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Text {
            $($key),*
        }

        impl Text {
            #[cfg(test)]
            const ALL: [Self; [$($id),*].len()] = [$(Self::$key),*];

            const fn id(self) -> &'static str {
                match self {
                    $(Self::$key => $id),*
                }
            }
        }
    };
}

texts! {
    // Shell
    Connecting => "status-connecting",
    Connected => "status-connected",
    Reconnecting => "status-reconnecting",
    Failed => "status-failed",
    Retry => "action-retry",
    Stop => "action-stop",
    Quit => "action-quit",
    Chat => "action-chat",

    // Dock
    DockPosition => "dock-position",
    TopLeft => "position-top-left",
    TopCenter => "position-top-center",
    TopRight => "position-top-right",
    MiddleLeft => "position-middle-left",
    Center => "position-center",
    MiddleRight => "position-middle-right",
    BottomLeft => "position-bottom-left",
    BottomCenter => "position-bottom-center",
    BottomRight => "position-bottom-right",
    DockError => "dock-error",
    DockBrainError => "dock-brain-error",
    PerchOn => "carry-perch-on",
    PerchUntitled => "carry-perch-untitled",

    // Turns
    RecoverySignIn => "recovery-sign-in",
    DismissNotice => "dismiss-notice",
    ChatLiveCut => "chat-live-cut",
    ChatEmpty => "chat-empty",
    ChatLoading => "chat-loading",
    ChatHistoryFailed => "chat-history-failed",
    TurnBusy => "turn-busy",
    TurnOffline => "turn-offline",
    TurnTooLong => "turn-too-long",

    // Tasks
    Threads => "threads-title",
    OpenThreads => "action-threads",
    NewThread => "action-new-thread",
    SearchThreads => "threads-search",
    EmptyThreads => "threads-empty",
    EmptyThreadSearch => "threads-search-empty",
    ThreadGone => "thread-gone",
    SelectThread => "thread-select",
    ThreadCut => "thread-cut",
    OlderMessages => "thread-older",
    CloseThreads => "threads-close",
    ThreadUser => "thread-user",
    ThreadAssistant => "thread-assistant",
    ThreadNoVisual => "thread-no-visual",
    ThreadsOffline => "threads-offline",
    ThreadSearchTooLong => "threads-search-too-long",
    ThreadsFailed => "threads-failed",
    DeleteThread => "thread-delete",
    DeleteThreadTitle => "thread-delete-title",
    DeleteThreadBody => "thread-delete-body",
    DeleteThreadCancel => "thread-delete-cancel",
    ThreadDeleting => "thread-deleting",
    ThreadDeleteFailed => "thread-delete-failed",
    ThreadDeletesFull => "thread-deletes-full",
    ThreadDeleted => "thread-deleted",
    BackToKumo => "action-back-to-kumo",
    SwitchThread => "action-switch-task",
    RenameThread => "action-rename-task",
    SaveRename => "action-save-rename",
    CancelRename => "action-cancel-rename",
    RenameEmpty => "rename-empty",
    RenameTooLong => "rename-task-too-long",
    RenameNameTooLong => "rename-name-too-long",
    RenameOneLine => "rename-one-line",
    TaskFallback => "task-fallback",

    IssueLoading => "issue-loading",
    IssueUnavailable => "issue-unavailable",
    IssueDetailsLoading => "issue-details-loading",
    IssueDetailsUnavailable => "issue-details-unavailable",
    IssueReadOnly => "issue-read-only",
    IssueConsentTitle => "issue-consent-title",
    IssueConsentOpen => "issue-consent-open",
    IssueDismiss => "issue-dismiss",
    IssueCheckAgain => "issue-check-again",
    IssueReplacement => "issue-replacement",
    IssuePending => "issue-pending",
    IssueQueueFailed => "issue-queue-failed",
    IssueActionUnavailable => "issue-action-unavailable",
    IssueReviewPending => "issue-review-pending",
    IssueReviewNotSaved => "issue-review-not-saved",
    IssueReviewCapacity => "issue-review-capacity",
    IssueAttemptNotSaved => "issue-attempt-not-saved",
    IssueConsentConsequence => "issue-consent-consequence",
    IssueConsentCancel => "issue-consent-cancel",
    IssueConsentConfirm => "issue-consent-confirm",
    IssueDuplicate => "issue-duplicate",

    // Settings
    Settings => "settings-title",
    OpenSettings => "action-settings",
    CloseSettings => "settings-close",
    General => "settings-general",
    Accounts => "settings-accounts",
    GitHubWaiting => "github-waiting",
    GitHubSignedOut => "github-signed-out",
    GitHubRemoving => "github-removing",
    GitHubRemovalFailed => "github-removal-failed",
    GitHubNotDurable => "github-not-durable",
    GitHubRequesting => "github-requesting",
    GitHubAwaiting => "github-awaiting",
    GitHubConnected => "github-connected",
    GitHubNotInstalled => "github-not-installed",
    GitHubNeedsSignIn => "github-needs-sign-in",
    GitHubNotVerified => "github-not-verified",
    GitHubCheckAgain => "github-check-again",
    GitHubConnect => "github-connect",
    GitHubSignInAgain => "github-sign-in-again",
    GitHubRefresh => "github-refresh",
    GitHubSignOut => "github-sign-out",
    GitHubTimeout => "github-timeout",
    GitHubFailure => "github-failure",
    GitHubOffline => "github-offline",
    GitHubCredentials => "github-credentials",
    GitHubRefused => "github-refused",
    GitHubUnavailable => "github-unavailable",
    GitHubProvider => "github-provider",
    GitHubConfiguration => "github-configuration",
    GitHubSavingDefault => "github-saving-default",
    GitHubDefaultRetained => "github-default-retained",
    GitHubDefaultRepository => "github-default-repository",
    GitHubChooseRepository => "github-choose-repository",
    GitHubEmptyRepositories => "github-empty-repositories",
    GitHubTruncatedRepositories => "github-truncated-repositories",
    GitHubVerification => "github-verification",
    GitHubCodeExpired => "github-code-expired",
    GitHubCanceling => "github-canceling",
    GitHubRefreshing => "github-refreshing",
    Voice => "settings-voice",
    Privacy => "settings-privacy",
    SpeechInput => "voice-speech-input",
    VoiceIdlePolicy => "voice-idle-policy",
    VoiceKept => "voice-idle-kept",
    Voice15Minutes => "voice-idle-15m",
    Voice1Hour => "voice-idle-1h",
    VoiceIdleHelp => "voice-idle-help",
    VoiceStatusUnknown => "voice-status-unknown",
    VoiceNotInstalled => "voice-not-installed",
    VoiceVerifying => "voice-verifying",
    VoiceDownloading => "voice-downloading",
    VoiceLoading => "voice-loading",
    VoiceReady => "voice-ready",
    VoiceUnloaded => "voice-unloaded",
    HoldPreparing => "hold-preparing",
    HoldToTalk => "hold-to-talk",
    HoldToTalkHelp => "hold-to-talk-help",
    HoldDisabled => "hold-disabled",
    HoldListening => "hold-listening",
    HoldNearLimit => "hold-near-limit",
    HoldStopping => "hold-stopping",
    HoldTranscribing => "hold-transcribing",
    HoldThinking => "hold-thinking",
    HoldEmpty => "hold-empty",
    HoldCancelled => "hold-cancelled",
    HoldUnavailable => "hold-unavailable",
    HoldMicrophoneUnavailable => "hold-microphone-unavailable",
    HoldUnconfirmed => "hold-unconfirmed",
    HoldFailed => "hold-failed",
    HoldLost => "hold-lost",
    HoldLostRestored => "hold-lost-restored",
    HoldLostPictured => "hold-lost-pictured",
    HoldLostRestoredPictured => "hold-lost-restored-pictured",
    ScreenHoldWillSee => "screen-hold-will-see",
    ScreenNotIncludedModel => "screen-not-included-model",
    ScreenNotIncludedNotReady => "screen-not-included-not-ready",
    ScreenNotIncludedNothingThere => "screen-not-included-nothing-there",
    ScreenNotIncludedCantShare => "screen-not-included-cant-share",
    ScreenNotIncludedUnreadable => "screen-not-included-unreadable",
    ScreenNotIncludedTooLarge => "screen-not-included-too-large",
    ScreenNotIncludedChanged => "screen-not-included-changed",
    ScreenNotIncludedTurnedOff => "screen-not-included-turned-off",
    ScreenHistoryAttached => "screen-history-attached",
    ShowScreenHistory => "show-screen-history",
    ShowScreen => "show-screen",
    ShowScreenHelp => "show-screen-help",
    ShowScreenChanged => "show-screen-changed",
    ScreenNotIncludedPictureTooLarge => "screen-not-included-picture-too-large",
    ScreenHistoryChanged => "screen-history-changed",
    ScreenNoteCant => "screen-note-cant",
    ScreenNoteCan => "screen-note-can",
    MarksSkipped => "marks-skipped",
    VoiceRestartRequired => "voice-restart-required",
    VoiceInstalledFailed => "voice-installed-failed",
    VoiceNotInstalledFailed => "voice-not-installed-failed",
    VoiceDefaultModel => "voice-default-model",
    VoiceInstallModel => "voice-install-model",
    VoiceRetryModel => "voice-retry-model",
    VoiceLoadModel => "voice-load-model",
    VoiceCancelSetup => "voice-cancel-setup",
    VoiceCancelHelp => "voice-cancel-help",
    VoiceCancelAccessible => "voice-cancel-accessible",
    VoiceRequestPending => "voice-request-pending",
    AssistantName => "settings-assistant-name",
    UserName => "settings-user-name",
    UserNameHelp => "settings-user-name-help",
    SettingsSaving => "settings-saving",
    SettingsOffline => "settings-offline",
    RenameBusy => "rename-busy",
    SettingsUnavailable => "settings-unavailable",
    SettingsNotSaved => "settings-not-saved",
    SettingsSaveHelp => "settings-save-help",
    SettingsVersion => "settings-version",
    SettingsCopyBuild => "settings-copy-build",
    SettingsPositionUnavailable => "settings-position-unavailable",
    SettingsTaskbar => "settings-taskbar",
    TaskbarAddFailed => "taskbar-add-failed",
    TaskbarRemoveFailed => "taskbar-remove-failed",
    MaxFrameRate => "settings-max-frame-rate",
    FrameRateAutomatic => "settings-frame-rate-automatic",
    DefaultModel => "settings-default-model",
    ModelOnConnection => "settings-model-on-connection",
    ModelsNotListed => "settings-models-not-listed",

    // Onboarding
    OnboardingClose => "onboarding-close",
    OnboardingContinue => "onboarding-continue",
    OnboardingFinish => "onboarding-finish",
    OnboardingNotNow => "onboarding-not-now",
    OnboardingPlan => "onboarding-plan",
    OnboardingWelcomeHelp => "onboarding-welcome-help",
    ShortcutKeys => "shortcut-keys",

    // ChatGPT accounts
    AccountProvider => "accounts-provider",
    AccountWaiting => "accounts-waiting",
    AccountOffline => "accounts-offline",
    AccountUsePlan => "accounts-use-plan",
    AccountContinue => "accounts-continue",
    AccountSignedOut => "accounts-signed-out",
    AccountSigningIn => "accounts-signing-in",
    AccountReplaceHelp => "accounts-replace-help",
    AccountCancel => "accounts-cancel",
    AccountOpenAgain => "accounts-open-again",
    AccountBusy => "accounts-busy",
    AccountBusyHelp => "accounts-busy-help",
    AccountTryAgain => "accounts-try-again",
    AccountIdentity => "accounts-identity",
    AccountUsingPlan => "accounts-using-plan",
    AccountPlanOff => "accounts-plan-off",
    AccountEnable => "accounts-enable",
    AccountUnavailable => "accounts-unavailable",
    AccountAttention => "accounts-attention",
    AccountRetryHelp => "accounts-retry-help",
    AccountSignInAgain => "accounts-sign-in-again",
    AccountSessionEnded => "accounts-session-ended",
    AccountManageUsage => "accounts-manage-usage",
    AccountSignOut => "accounts-sign-out",
    AccountSignOutTitle => "accounts-sign-out-title",
    AccountSignOutBody => "accounts-sign-out-body",
    AccountSignOutRegistration => "accounts-sign-out-registration",
    AccountSignOutConfirm => "accounts-sign-out-confirm",
    AccountDialogCancel => "accounts-dialog-cancel",
    AccountSigningOut => "accounts-signing-out",
    AccountStopped => "accounts-stopped",
    AccountCleanupRefused => "accounts-cleanup-refused",

    AccountNotDurable => "accounts-not-durable",
    AccountRevocationPending => "accounts-revocation-pending",
    AccountRevocationUnconfirmed => "accounts-revocation-unconfirmed",
    AccountSendFailed => "accounts-send-failed",
    AccountLocal => "accounts-local",
    AccountRemote => "accounts-remote",
    AccountSignOutNeedsRetry => "accounts-sign-out-needs-retry",
    AccountCleanupRefusedHelp => "accounts-cleanup-refused-help",
    AccountNotDurableHelp => "accounts-not-durable-help",
    AccountBacklogLead => "accounts-backlog-lead",
    AccountBacklogRetry => "accounts-backlog-retry",
    AccountBacklogExpiry => "accounts-backlog-expiry",
    AccountBacklogRecovery => "accounts-backlog-recovery",

    // Anthropic API key
    AnthropicTitle => "anthropic-title",
    AnthropicIntro => "anthropic-intro",
    AnthropicDisclosure => "anthropic-check-disclosure",
    AnthropicBilling => "anthropic-billing",
    AnthropicPaste => "anthropic-paste",
    AnthropicReplaceNote => "anthropic-replace-note",
    AnthropicMissing => "anthropic-missing",
    AnthropicSaving => "anthropic-saving",
    AnthropicChecking => "anthropic-checking",
    AnthropicSaved => "anthropic-saved",
    AnthropicReady => "anthropic-ready",
    AnthropicRejected => "anthropic-rejected",
    AnthropicUnconfirmed => "anthropic-unconfirmed",
    AnthropicCheck => "anthropic-check",
    AnthropicRetry => "anthropic-retry",
    AnthropicClipboardInvalid => "anthropic-clipboard-invalid",
    AnthropicClipboardTooLong => "anthropic-clipboard-too-long",
    AnthropicSaveFailed => "anthropic-save-failed",
    AnthropicSaveUnconfirmed => "anthropic-save-unconfirmed",
    AnthropicRemoving => "anthropic-removing",
    AnthropicRemovePending => "anthropic-remove-pending",
    AnthropicRemoveChanged => "anthropic-remove-changed",
    AnthropicRemoveOpen => "anthropic-remove-open",
    AnthropicRemove => "anthropic-remove",
    AnthropicRemoveTitle => "anthropic-remove-title",
    AnthropicRemoveBody => "anthropic-remove-body",
    AnthropicFormat => "error-anthropic-key-format",
    AnthropicSubscriptionToken => "error-anthropic-key-subscription-token",
    AnthropicAdminKey => "error-anthropic-key-admin-key",
    AnthropicWorkspaceRequired => "error-anthropic-key-workspace-required",
    AnthropicRefused => "error-anthropic-key-rejected",

    // Claude Code
    ClaudeProvider => "claude-provider",
    ClaudeReview => "claude-review",
    ClaudeConnect => "claude-connect",
    ClaudeDisconnect => "claude-disconnect",
    ClaudeFind => "claude-find",
    ClaudePolicyNotice => "claude-policy-notice",
    ClaudeBefore => "claude-before",
    ClaudeDuring => "claude-during",
    ClaudeFallback => "claude-fallback",
    ClaudeDeadline => "claude-deadline",
    ClaudeAnalytics => "claude-analytics",
    ClaudeDisconnectBody => "claude-disconnect-body",
}

impl Text {
    pub const POSITIONS: [Self; 9] = [
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

    /// The line saying why a screen hold's window wasn't included.
    pub const fn screen_not_included(class: ScreenClass) -> Self {
        match class {
            ScreenClass::NotReady => Self::ScreenNotIncludedNotReady,
            ScreenClass::NothingThere => Self::ScreenNotIncludedNothingThere,
            ScreenClass::CantShare => Self::ScreenNotIncludedCantShare,
            ScreenClass::Unreadable => Self::ScreenNotIncludedUnreadable,
            ScreenClass::TooLarge => Self::ScreenNotIncludedTooLarge,
            ScreenClass::Changed => Self::ScreenNotIncludedChanged,
            ScreenClass::ModelCantSee => Self::ScreenNotIncludedModel,
        }
    }

    /// The history line under a held turn saying why its screen hold's picture wasn't included.
    /// None for a turn whose picture was taken, which [`Strings::pictured`] speaks for.
    pub const fn held_screen(screen: HeldScreen) -> Option<Self> {
        Some(match screen {
            // A stored turn doesn't say which flow it came from, so these two read true for both.
            HeldScreen::NotIncluded(ScreenClass::TooLarge) => {
                Self::ScreenNotIncludedPictureTooLarge
            }
            HeldScreen::NotIncluded(ScreenClass::Changed) => Self::ScreenHistoryChanged,
            HeldScreen::NotIncluded(class) => Self::screen_not_included(class),
            HeldScreen::Withdrawn => Self::ScreenNotIncludedTurnedOff,
            HeldScreen::Attached | HeldScreen::Screen => return None,
        })
    }
}

/// The loaded catalog, kept as a GPUI global on the UI thread.
pub struct Strings {
    bundle: FluentBundle<FluentResource>,
}

impl std::fmt::Debug for Strings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Strings").finish_non_exhaustive()
    }
}

impl Strings {
    pub fn taskbar(&self) -> String {
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        self.format(Text::SettingsTaskbar.id(), Some(&args))
    }

    /// What the icon under a held turn says: which picture was sent with the turn. None for a
    /// turn with no picture. The brain clears a picture no request carried, so this over-reports
    /// only for a request that failed before its upload began, or after a crash before the
    /// clear. The catalog may name the app.
    pub fn pictured(&self, screen: HeldScreen) -> Option<String> {
        let text = match screen {
            HeldScreen::Attached => Text::ScreenHistoryAttached,
            HeldScreen::Screen => Text::ShowScreenHistory,
            HeldScreen::NotIncluded(_) | HeldScreen::Withdrawn => return None,
        };
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        Some(self.format(text.id(), Some(&args)))
    }

    /// The Privacy tab's setting, named for the assistant.
    pub fn show_screen(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        self.format(Text::ShowScreen.id(), Some(&args))
    }

    /// What that setting sends and where, with the product name.
    pub fn show_screen_help(&self) -> String {
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        self.format(Text::ShowScreenHelp.id(), Some(&args))
    }

    /// Loads the English catalog compiled into the binary.
    pub fn english() -> Self {
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
    pub fn get(&self, text: Text) -> String {
        self.format(text.id(), None)
    }

    pub fn assistant(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        self.format(Text::ThreadAssistant.id(), Some(&args))
    }

    /// A model row whose name another connection's model shares, with its connection's name.
    pub fn model_on(&self, model: &str, connection: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("model", model);
        args.set("connection", connection);
        self.format(Text::ModelOnConnection.id(), Some(&args))
    }

    /// The onboarding window's heading, with the product name.
    pub fn onboarding_title(&self) -> String {
        let mut args = FluentArgs::new();
        args.set("app", kumochi_protocol::constants::DISPLAY_NAME);
        self.format("onboarding-title", Some(&args))
    }

    /// The welcome's words before and after the name input, which may sit anywhere in it. Both
    /// are trimmed, since the row spaces the input itself.
    pub fn onboarding_welcome(&self) -> (String, String) {
        let mut args = FluentArgs::new();
        args.set("name", SPOT);
        around_spot(self.format("onboarding-welcome", Some(&args)))
    }

    /// The shortcut step's words before and after the keys, which may sit anywhere in it, then
    /// its help line, with the assistant's name.
    pub fn onboarding_shortcut(&self, assistant: &str) -> [String; 3] {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        args.set("keys", SPOT);
        let (before, after) = around_spot(self.format("onboarding-shortcut", Some(&args)));
        [
            before,
            after,
            self.format("onboarding-shortcut-help", Some(&args)),
        ]
    }

    /// The question for the user's name and its help line, with the assistant's name.
    pub fn onboarding_user(&self, assistant: &str) -> [String; 2] {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        ["onboarding-user", "onboarding-user-help"].map(|id| self.format(id, Some(&args)))
    }

    /// The account step's heading and help line, with the assistant's name.
    pub fn onboarding_accounts(&self, assistant: &str) -> [String; 2] {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        ["onboarding-accounts", "onboarding-accounts-help"].map(|id| self.format(id, Some(&args)))
    }

    pub fn issue_title(&self, title: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("title", title);
        self.format("thread-issue-title", Some(&args))
    }

    /// The delete dialog's body, with what Kumo keeps.
    pub fn delete_body(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        format!(
            "{} {}",
            self.get(Text::DeleteThreadBody),
            self.format("thread-delete-kumo", Some(&args))
        )
    }

    /// The drop label under a carried Kumo: `app` is the window's title, which is screen text.
    pub fn perch_on(&self, app: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("app", app);
        self.format(Text::PerchOn.id(), Some(&args))
    }

    pub fn back_to_kumo(&self, assistant: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("assistant", assistant);
        self.format(Text::BackToKumo.id(), Some(&args))
    }

    /// A screen-hold line that names the model the window goes to, as the picker shows it.
    pub fn with_model(&self, text: Text, model: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("model", model);
        self.format(text.id(), Some(&args))
    }

    pub fn frame_rate(&self, fps: f64) -> String {
        let mut args = FluentArgs::new();
        args.set("fps", (fps * 10.0).round() / 10.0);
        self.format("settings-frame-rate-now", Some(&args))
    }

    pub fn focus(&self, task: &str) -> String {
        let mut args = FluentArgs::new();
        args.set("task", task);
        self.format("task-focus", Some(&args))
    }

    /// The text for an error the brain sent, with its typed arguments.
    pub fn error(&self, error: &UserError) -> String {
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

/// Stands in for an element placed inside a sentence, so the words around it can be split off.
const SPOT: &str = "\u{E000}";

/// The words before and after [`SPOT`], trimmed, since the row spaces the element itself.
fn around_spot(text: String) -> (String, String) {
    match text.split_once(SPOT) {
        Some((before, after)) => (before.trim().to_owned(), after.trim().to_owned()),
        None => (text, String::new()),
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
        ErrorKey::ClaudeCodeUnavailable => "error-claude-code-unavailable",
        ErrorKey::ClaudeCodeBusy => "error-claude-code-busy",

        // Voice
        ErrorKey::VoiceNoSpace => "error-voice-no-space",
        ErrorKey::VoiceDownload => "error-voice-download",
        ErrorKey::VoiceCorrupt => "error-voice-corrupt",
        ErrorKey::VoiceStorage => "error-voice-storage",
        ErrorKey::VoiceLoad => "error-voice-load",
        ErrorKey::VoiceUnavailable => "error-voice-unavailable",
        ErrorKey::VoiceModelPath => "error-voice-model-path",
        ErrorKey::HoldToTalkOff => "hold-disabled",
        ErrorKey::GitHubUnavailable => "github-unavailable",
        ErrorKey::DrawingReplacementFailed => "error-drawing-replacement-failed",
        ErrorKey::DrawingNotReturned => "error-drawing-not-returned",
        ErrorKey::ScreenUnavailable => "error-screen-unavailable",
        ErrorKey::ScreenPictureStopped => "error-screen-picture-stopped",
        ErrorKey::ConnectionNeeded => "error-connection-needed",
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
    fn onboarding_splits_the_shortcut_at_its_keys() {
        assert_eq!(
            Strings::english().onboarding_shortcut("Kumo"),
            [
                "Double-tap".to_owned(),
                "to chat with Kumo".to_owned(),
                "Try it now: two quick presses of both keys open a typing box under the dock."
                    .to_owned()
            ]
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
    fn the_screen_lines_say_what_happened_without_claiming_more() {
        let strings = Strings::english();
        let text = |key: ErrorKey| strings.error(&key.into());
        assert_eq!(
            text(ErrorKey::ScreenUnavailable),
            "The screen is no longer available, so this turn was stopped."
        );
        assert_eq!(
            text(ErrorKey::ScreenPictureStopped),
            "This model stopped accepting screenshots during this answer, so the answer was stopped. The screenshot may already have reached the model."
        );
    }

    #[test]
    fn each_screen_class_has_its_line_and_the_model_lines_name_the_model() {
        use kumochi_protocol::message::{HeldScreen, ScreenClass};
        let strings = Strings::english();
        for (class, reason) in [
            (ScreenClass::NotReady, "the screen wasn't ready"),
            (ScreenClass::NothingThere, "nothing under the pointer"),
            (ScreenClass::CantShare, "that window can't be shared"),
            (
                ScreenClass::Unreadable,
                "something is drawn over the window under your pointer, so I couldn't tell which one you meant",
            ),
            (ScreenClass::TooLarge, "the window image was too large"),
            (ScreenClass::Changed, "the window changed"),
            (
                ScreenClass::ModelCantSee,
                "screen sharing isn't available with this model",
            ),
        ] {
            assert_eq!(
                strings.get(Text::screen_not_included(class)),
                format!("Screen not included: {reason}"),
            );
        }
        for (text, line) in [
            (
                Text::ScreenNoteCant,
                "GPT-5.5 doesn't support screenshots, so messages will go without one.",
            ),
            (
                Text::ScreenNoteCan,
                "GPT-5.5 supports screenshots, so holding to talk will send one along.",
            ),
            (Text::ScreenHoldWillSee, "GPT-5.5 will see this window"),
        ] {
            assert_eq!(strings.with_model(text, "GPT-5.5"), line);
        }
        // A hold that sends the screen at release: its own word for a display that changed, and
        // the Privacy tab's row.
        assert_eq!(
            strings.get(Text::ShowScreenChanged),
            "Screen not included: the display changed"
        );
        assert_eq!(strings.get(Text::Privacy), "Privacy");
        assert_eq!(strings.show_screen("Cloud"), "Cloud can see what you see");
        let app = kumochi_protocol::constants::DISPLAY_NAME;
        assert_eq!(
            strings.show_screen_help(),
            format!(
                "When you let go of the shortcut, {app} sends one screenshot of the screen your pointer is on, as it is, with your words to the model you're using. It sends it nowhere else and keeps no copy once the answer is done."
            )
        );
        // A held turn's history says one thing: why no picture was included, as a line, or
        // which picture was taken, on its icon.
        let app = kumochi_protocol::constants::DISPLAY_NAME;
        for (screen, line, icon) in [
            (
                HeldScreen::Attached,
                None,
                Some(format!(
                    "A screenshot of the window under your pointer was sent in this message. {app} keeps no copy."
                )),
            ),
            (
                HeldScreen::Screen,
                None,
                Some(format!(
                    "A screenshot of your screen was sent in this message. {app} keeps no copy."
                )),
            ),
            // A stored turn doesn't say which flow it came from, so these two read true of both.
            (
                HeldScreen::NotIncluded(ScreenClass::Changed),
                Some("Screen not included: it changed while the screenshot was being taken"),
                None,
            ),
            (
                HeldScreen::NotIncluded(ScreenClass::TooLarge),
                Some("Screen not included: the screenshot was too large"),
                None,
            ),
            (
                HeldScreen::NotIncluded(ScreenClass::NotReady),
                Some("Screen not included: the screen wasn't ready"),
                None,
            ),
            (
                HeldScreen::Withdrawn,
                Some("Screen not included: you turned screen sharing off"),
                None,
            ),
        ] {
            let shown = Text::held_screen(screen).map(|text| strings.get(text));
            assert_eq!(shown.as_deref(), line, "{screen:?}");
            assert_eq!(strings.pictured(screen), icon, "{screen:?}");
        }
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
