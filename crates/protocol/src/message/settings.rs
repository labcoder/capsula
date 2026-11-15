//! The user's settings as they travel between shell and brain. Names carry user content, so
//! their `Debug` prints only a length.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ModelChoice, ThreadId};
use crate::voice::VoicePart;

/// The longest name, in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 24;

/// Why a name or a thread title was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Nothing but whitespace.
    #[error("the text is empty")]
    Empty,
    /// More characters than its kind allows: [`MAX_NAME_CHARS`] for a name.
    #[error("the text is too long")]
    TooLong,
    /// A control character or a line break: names and titles go into requests and one-line rows.
    #[error("the text isn't one line")]
    NotOneLine,
}

/// The shared rule: trimmed, 1–`max` scalar values, one line, no control characters.
pub(super) fn checked(text: &str, max: usize) -> Result<String, NameError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(NameError::Empty);
    }
    if text
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(NameError::NotOneLine);
    }
    if text.chars().count() > max {
        return Err(NameError::TooLong);
    }
    Ok(text.to_owned())
}

/// A checked text type: built only through [`checked`] with `$max`, on decode too, and with a
/// `Debug` that prints only its length.
macro_rules! name {
    ($(#[$doc:meta])* $name:ident, $max:expr) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Checks `text` against the name rule, trimming it first.
            ///
            /// # Errors
            /// [`NameError`] says which part of the rule it breaks.
            pub fn new(text: &str) -> Result<Self, NameError> {
                checked(text, $max).map(Self)
            }

            /// The name's text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(text: String) -> Result<Self, NameError> {
                Self::new(&text)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> String {
                name.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(len={})", stringify!($name), self.0.chars().count())
            }
        }
    };
}

name! {
    /// What the assistant is called, in every request and on Kumo's row.
    AssistantName, MAX_NAME_CHARS
}

name! {
    /// What the assistant calls the user. Optional: unset is `None`, never an empty name.
    UserName, MAX_NAME_CHARS
}

/// The dock's alignment within the screen's work area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockPosition {
    /// Top edge, left aligned.
    TopLeft,
    /// Top edge, centered.
    #[default]
    TopCenter,
    /// Top edge, right aligned.
    TopRight,
    /// Middle, left aligned.
    MiddleLeft,
    /// Center of the work area.
    Center,
    /// Middle, right aligned.
    MiddleRight,
    /// Bottom edge, left aligned.
    BottomLeft,
    /// Bottom edge, centered.
    BottomCenter,
    /// Bottom edge, right aligned.
    BottomRight,
}

impl DockPosition {
    /// All positions in the tray menu's row-major order.
    pub const ALL: [Self; 9] = [
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
}

/// First-run onboarding's steps, in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingStep {
    /// The welcome, with the assistant's name.
    #[default]
    Welcome,
    /// What the assistant calls the user.
    YourName,
    /// Connecting accounts.
    Accounts,
    /// The shortcut, which finishes onboarding.
    Shortcut,
}

impl OnboardingStep {
    /// The step after this one; none after the last.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Welcome => Some(Self::YourName),
            Self::YourName => Some(Self::Accounts),
            Self::Accounts => Some(Self::Shortcut),
            Self::Shortcut => None,
        }
    }
}

/// A settings change, shell to brain. Each one is answered by exactly one
/// [`super::ControlToShell::Settings`] with `answer` set, in order, while its connection lasts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingsOp {
    /// Rename the assistant.
    SetAssistantName(AssistantName),
    /// Set or clear the user's name.
    SetUserName(Option<UserName>),
    /// Onboarding is done; it doesn't show again.
    FinishOnboarding,
    /// Cap how often Kumo draws.
    SetMaxFrameRate(MaxFrameRate),
    /// Set the model a thread with no choice runs on. Only the brain sends it today, from the
    /// catalog (design §6).
    SetDefaultModel(ModelChoice),
    /// Move the dock within the work area.
    SetDockPosition(DockPosition),
    /// Also show the app in the taskbar.
    SetTaskbar(bool),
    /// Speak replies aloud.
    SetTalkBack(bool),
    /// When an idle speech-input model is unloaded.
    SetSttUnload(SttUnload),
    /// Download, check and load a voice part. Refused with `VoiceUnavailable` until the
    /// installer lands.
    InstallVoice(VoicePart),
    /// Unload and delete a voice part. Refused like `InstallVoice` until then.
    RemoveVoice(VoicePart),
    /// Stop the install in progress. Refused like `InstallVoice` until then.
    CancelVoiceInstall,
    /// Allow deliberate held speech input.
    SetHoldToTalk(bool),
    /// An onboarding step was completed, so the next launch starts after it. The last step is
    /// completed only by [`Self::FinishOnboarding`]: completing it here changes nothing.
    CompleteOnboardingStep(OnboardingStep),
    /// Let a hold also show the model the window under the pointer.
    SetScreenHolds(bool),
    /// Let a hold also show the model a picture of the screen, taken when it's released.
    SetShowScreen(bool),
}

/// The initial held-speech choice, also used by older settings files without the key.
pub const DEFAULT_HOLD_TO_TALK: bool = true;

/// The initial screen-hold choice, also used by older settings files without the key.
pub const DEFAULT_SCREEN_HOLDS: bool = true;

/// The initial show-screen choice, also used by settings files without the key. A stored
/// screen-hold choice never stands in for it.
pub const DEFAULT_SHOW_SCREEN: bool = false;

/// When an idle speech-input model is unloaded to free its memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttUnload {
    /// Kept loaded.
    #[default]
    Kept,
    /// Unloaded after 15 idle minutes.
    After15Min,
    /// Unloaded after an idle hour.
    After1Hour,
}

/// The settings as the brain holds them: once per connection before `Scenes`, and after
/// every [`SettingsOp`], applied or refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each boolean is an independent persisted preference"
)]
pub struct SettingsView {
    /// The assistant's name.
    pub assistant_name: AssistantName,
    /// The user's name, if set.
    pub user_name: Option<UserName>,
    /// Onboarding isn't owed: `config.toml` says it finished, or it has no `onboarded` key and
    /// the profile was already in use when the brain started. True when the file can't be read.
    pub onboarded: bool,
    /// The main thread, Kumo's, which the shell labels with `assistant_name`.
    pub main_thread: ThreadId,
    /// The most frames a second Kumo draws.
    pub max_frame_rate: MaxFrameRate,
    /// The model a thread with no choice runs on; unset until a catalog has been read.
    pub default_model: Option<ModelChoice>,
    /// The dock's saved work-area alignment.
    pub dock_position: DockPosition,
    /// Whether the app also appears in the taskbar.
    pub show_in_taskbar: bool,
    /// Whether replies are spoken aloud.
    pub talk_back: bool,
    /// When an idle speech-input model is unloaded.
    pub stt_unload: SttUnload,
    /// Whether deliberate held speech input is enabled.
    pub hold_to_talk: bool,
    /// The first onboarding step not yet completed.
    pub onboarding_from: OnboardingStep,
    /// Whether a hold may also show the model the window under the pointer.
    pub screen_holds: bool,
    /// Whether a hold may also show the model a picture of the screen, taken when it's released.
    pub show_screen: bool,
}

/// The most frames a second Kumo draws. The shell paces to the display, so the rate it runs at
/// can be lower (docs/development/frame-rate.md).
/// Automatic (the default) picks 60 or 72, whichever the display's refresh divides more evenly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Option<u16>", into = "Option<u16>")]
pub struct MaxFrameRate(Option<u16>);

/// A frame rate cap that isn't one of [`MaxFrameRate::CAPS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} isn't a frame rate cap")]
pub struct NotACap(pub u16);

impl MaxFrameRate {
    /// The caps a user can pick besides Automatic.
    pub const CAPS: [u16; 5] = [30, 60, 72, 120, 144];

    /// Automatic: 60 or 72 by display.
    pub const AUTOMATIC: Self = Self(None);

    /// A cap in frames a second.
    ///
    /// # Errors
    /// [`NotACap`] unless `fps` is one of [`MaxFrameRate::CAPS`].
    pub fn cap(fps: u16) -> Result<Self, NotACap> {
        if Self::CAPS.contains(&fps) {
            Ok(Self(Some(fps)))
        } else {
            Err(NotACap(fps))
        }
    }

    /// The cap the user picked, or `None` for Automatic.
    #[must_use]
    pub fn fps(self) -> Option<u16> {
        self.0
    }
}

impl TryFrom<Option<u16>> for MaxFrameRate {
    type Error = NotACap;

    fn try_from(fps: Option<u16>) -> Result<Self, NotACap> {
        fps.map_or(Ok(Self::AUTOMATIC), Self::cap)
    }
}

impl From<MaxFrameRate> for Option<u16> {
    fn from(rate: MaxFrameRate) -> Self {
        rate.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AssistantName, DockPosition, MAX_NAME_CHARS, MaxFrameRate, NameError, NotACap, SettingsOp,
        SettingsView, SttUnload, ThreadId, UserName,
    };
    use crate::voice::VoicePart;

    fn view() -> SettingsView {
        SettingsView {
            assistant_name: AssistantName::new("Sentinelname").expect("valid"),
            user_name: Some(UserName::new("Sentineluser").expect("valid")),
            onboarded: true,
            onboarding_from: crate::message::OnboardingStep::Welcome,
            main_thread: ThreadId(1),
            max_frame_rate: MaxFrameRate::AUTOMATIC,
            default_model: None,
            dock_position: DockPosition::TopCenter,
            show_in_taskbar: false,
            talk_back: false,
            stt_unload: SttUnload::Kept,
            hold_to_talk: crate::message::DEFAULT_HOLD_TO_TALK,
            screen_holds: crate::message::DEFAULT_SCREEN_HOLDS,
            show_screen: crate::message::DEFAULT_SHOW_SCREEN,
        }
    }

    #[test]
    fn voice_operations_are_appended_and_round_trip() {
        let ops = [
            (7, SettingsOp::SetTalkBack(true)),
            (7, SettingsOp::SetTalkBack(false)),
            (8, SettingsOp::SetSttUnload(SttUnload::Kept)),
            (8, SettingsOp::SetSttUnload(SttUnload::After15Min)),
            (8, SettingsOp::SetSttUnload(SttUnload::After1Hour)),
            (9, SettingsOp::InstallVoice(VoicePart::SpeechInput)),
            (9, SettingsOp::InstallVoice(VoicePart::SpokenReplies)),
            (10, SettingsOp::RemoveVoice(VoicePart::SpeechInput)),
            (10, SettingsOp::RemoveVoice(VoicePart::SpokenReplies)),
            (11, SettingsOp::CancelVoiceInstall),
            (12, SettingsOp::SetHoldToTalk(false)),
            (12, SettingsOp::SetHoldToTalk(true)),
            (14, SettingsOp::SetScreenHolds(false)),
            (14, SettingsOp::SetScreenHolds(true)),
            (15, SettingsOp::SetShowScreen(false)),
            (15, SettingsOp::SetShowScreen(true)),
        ];
        for (tag, op) in ops {
            let bytes = postcard::to_allocvec(&op).expect("encodes");
            assert_eq!(bytes[0], tag, "{op:?}");
            assert_eq!(
                postcard::from_bytes::<SettingsOp>(&bytes).expect("decodes"),
                op
            );
        }
        assert_eq!(
            postcard::to_allocvec(&SettingsOp::SetTaskbar(true)).expect("encodes"),
            [6, 1]
        );
        assert!(postcard::from_bytes::<SettingsOp>(&[12]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[12, 2]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[13, 4]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[14]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[14, 2]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[15]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[8, 3]).is_err());
        assert!(postcard::from_bytes::<SettingsOp>(&[9, 2]).is_err());
        assert_eq!(SttUnload::default(), SttUnload::Kept);
    }

    #[test]
    fn onboarding_steps_keep_their_wire_positions() {
        use crate::message::OnboardingStep;
        let steps = [
            ([13, 0], OnboardingStep::Welcome),
            ([13, 1], OnboardingStep::YourName),
            ([13, 2], OnboardingStep::Accounts),
            ([13, 3], OnboardingStep::Shortcut),
        ];
        for (bytes, step) in steps {
            let op = SettingsOp::CompleteOnboardingStep(step);
            assert_eq!(postcard::to_allocvec(&op).expect("encodes"), bytes);
            assert_eq!(
                postcard::from_bytes::<SettingsOp>(&bytes).expect("decodes"),
                op
            );
        }
        let view = SettingsView {
            onboarding_from: OnboardingStep::Shortcut,
            ..view()
        };
        let bytes = postcard::to_allocvec(&view).expect("encodes");
        assert_eq!(
            postcard::from_bytes::<SettingsView>(&bytes).expect("decodes"),
            view
        );
    }

    #[test]
    fn the_voice_settings_round_trip_in_the_view() {
        for talk_back in [false, true] {
            for stt_unload in [
                SttUnload::Kept,
                SttUnload::After15Min,
                SttUnload::After1Hour,
            ] {
                let view = SettingsView {
                    talk_back,
                    stt_unload,
                    ..view()
                };
                let bytes = postcard::to_allocvec(&view).expect("encodes");
                assert_eq!(
                    postcard::from_bytes::<SettingsView>(&bytes).expect("decodes"),
                    view
                );
            }
        }
    }

    #[test]
    fn dock_and_taskbar_operations_and_views_round_trip() {
        let positions = [
            DockPosition::TopLeft,
            DockPosition::TopCenter,
            DockPosition::TopRight,
            DockPosition::MiddleLeft,
            DockPosition::Center,
            DockPosition::MiddleRight,
            DockPosition::BottomLeft,
            DockPosition::BottomCenter,
            DockPosition::BottomRight,
        ];
        assert_eq!(DockPosition::ALL, positions);
        assert_eq!(DockPosition::default(), DockPosition::TopCenter);
        for position in positions {
            for shown in [true, false] {
                for op in [
                    SettingsOp::SetDockPosition(position),
                    SettingsOp::SetTaskbar(shown),
                ] {
                    let bytes = postcard::to_allocvec(&op).expect("encodes");
                    assert_eq!(
                        postcard::from_bytes::<SettingsOp>(&bytes).expect("decodes"),
                        op
                    );
                }
                let view = SettingsView {
                    assistant_name: AssistantName::new("Sentinelname").expect("valid"),
                    user_name: Some(UserName::new("Sentineluser").expect("valid")),
                    onboarded: true,
                    onboarding_from: crate::message::OnboardingStep::Welcome,
                    main_thread: ThreadId(1),
                    max_frame_rate: MaxFrameRate::AUTOMATIC,
                    default_model: None,
                    dock_position: position,
                    show_in_taskbar: shown,
                    talk_back: false,
                    stt_unload: SttUnload::Kept,
                    hold_to_talk: crate::message::DEFAULT_HOLD_TO_TALK,
                    screen_holds: crate::message::DEFAULT_SCREEN_HOLDS,
                    show_screen: crate::message::DEFAULT_SHOW_SCREEN,
                };
                let bytes = postcard::to_allocvec(&view).expect("encodes");
                assert_eq!(
                    postcard::from_bytes::<SettingsView>(&bytes).expect("decodes"),
                    view
                );
                let outer = super::super::ControlToShell::Settings { view, answer: true };
                let debug = format!("{outer:?}");
                for marker in ["Sentinelname", "Sentineluser"] {
                    assert!(!debug.contains(marker));
                    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
                }
            }
        }
    }

    #[test]
    fn only_the_listed_caps_decode() {
        let decode = |fps: Option<u16>| {
            let bytes = postcard::to_allocvec(&fps).expect("encodes");
            postcard::from_bytes::<MaxFrameRate>(&bytes).map(MaxFrameRate::fps)
        };
        assert_eq!(decode(None).ok(), Some(None));
        for fps in [30, 60, 72, 120, 144] {
            assert_eq!(decode(Some(fps)).ok(), Some(Some(fps)));
        }
        for fps in [0, 29, 31, 59, 61, 71, 73, 143, 145, u16::MAX] {
            assert!(decode(Some(fps)).is_err(), "{fps}");
            assert_eq!(MaxFrameRate::cap(fps), Err(NotACap(fps)));
        }
        assert_eq!(MaxFrameRate::default(), MaxFrameRate::AUTOMATIC);
    }

    #[test]
    fn names_follow_the_one_rule() {
        let ok = |text: &str| AssistantName::new(text).map(|name| name.as_str().to_owned());
        assert_eq!(ok("K"), Ok("K".to_owned()));
        assert_eq!(ok("  Kumo \t"), Ok("Kumo".to_owned()));
        assert_eq!(ok(&"a".repeat(MAX_NAME_CHARS)), Ok("a".repeat(24)));
        assert_eq!(
            ok(&"🌥".repeat(24)),
            Ok("🌥".repeat(24)),
            "24 four-byte characters"
        );
        assert_eq!(ok(&"a".repeat(25)), Err(NameError::TooLong));
        assert_eq!(ok(""), Err(NameError::Empty));
        assert_eq!(ok("   "), Err(NameError::Empty));
        for bad in [
            "Ku\nmo",
            "Ku\u{7}mo",
            "Ku\u{2028}mo",
            "Ku\u{2029}mo",
            "Ku\rmo",
        ] {
            assert_eq!(ok(bad), Err(NameError::NotOneLine), "{bad:?}");
        }
        assert_eq!(UserName::new("").map(|_| ()), Err(NameError::Empty));
    }

    #[test]
    fn an_invalid_name_fails_to_decode() {
        let bytes = postcard::to_allocvec(&"a".repeat(25)).expect("encodes");
        assert!(postcard::from_bytes::<AssistantName>(&bytes).is_err());
        let bytes = postcard::to_allocvec(&"Kumo").expect("encodes");
        let name: AssistantName = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(name.as_str(), "Kumo");
    }

    #[test]
    fn debug_prints_only_the_length() {
        let name = AssistantName::new("Sentinelname").expect("valid");
        assert_eq!(format!("{name:?}"), "AssistantName(len=12)");
        let user = Some(UserName::new("Oscarsentinel").expect("valid"));
        assert!(!format!("{user:?}").contains("Oscarsentinel"));
    }
}
