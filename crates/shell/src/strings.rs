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
    Listening,
    Retry,
    Stop,
    Quit,
    DockOffline,
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
}

impl Text {
    #[cfg(test)]
    const ALL: [Self; 46] = [
        Self::Connecting,
        Self::Connected,
        Self::Reconnecting,
        Self::Failed,
        Self::Listening,
        Self::Retry,
        Self::Stop,
        Self::Quit,
        Self::DockOffline,
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

    const fn id(self) -> &'static str {
        match self {
            Self::Connecting => "status-connecting",
            Self::Connected => "status-connected",
            Self::Reconnecting => "status-reconnecting",
            Self::Failed => "status-failed",
            Self::Listening => "status-listening",
            Self::Retry => "action-retry",
            Self::Stop => "action-stop",
            Self::Quit => "action-quit",
            Self::DockOffline => "dock-offline",
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
        bundle
            .add_resource(resource)
            .expect("the bundled catalog has no duplicate ids; strings::tests checks it");
        Self { bundle }
    }

    /// The text for `text`. A missing entry shows its id, which the unit test rules out.
    pub(crate) fn get(&self, text: Text) -> String {
        self.format(text.id(), None)
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
    }
}

#[cfg(test)]
mod tests {
    use super::{Strings, Text, error_id};
    use kumochi_protocol::message::{ErrorKey, UserError};

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
}
