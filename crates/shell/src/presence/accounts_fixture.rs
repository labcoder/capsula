//! Offline capture of the production Settings view, with no presence or transport owner.

use std::time::Duration;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{
    App, AppContext as _, Bounds, Pixels, Size, Styled as _, WeakEntity,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use kumochi_protocol::message::{
    AccountEmail, AccountRecovery, AccountView, Action, AssistantName, Cleanup, ErrorKey,
    MaxFrameRate, PlanState, SettingsView as Snapshot, ThreadId, UserError,
};

use super::{OpenError, settings::SettingsView};
use crate::settings::{Field, Settings, Update};
use crate::strings::{Strings, Text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Appearance {
    Light,
    Dark,
    Large,
    Constrained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    GeneralError,
    ClaudeReview,
    ClaudeSigning,
    ClaudeReady,
    ClaudeCleanup,
    Waiting,
    Offline,
    SignedOut,
    Unconfirmed,
    SigningIn,
    Busy,
    Enabled,
    NoEmail,
    Declined,
    Unavailable,
    Retry,
    SignInAgain,
    ProviderSignIn,
    SigningOut,
    Refused,
    RefusedUnconfirmed,
    NotDurable,
    NotDurableUnconfirmed,
    Confirmation,
    Backlog,
}

impl State {
    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "claude-review" => Self::ClaudeReview,
            "claude-signing" => Self::ClaudeSigning,
            "claude-ready" => Self::ClaudeReady,
            "claude-cleanup" => Self::ClaudeCleanup,
            "general-error" => Self::GeneralError,
            "waiting" => Self::Waiting,
            "offline" => Self::Offline,
            "signed-out" => Self::SignedOut,
            "unconfirmed" => Self::Unconfirmed,
            "signing-in" => Self::SigningIn,
            "busy" => Self::Busy,
            "enabled" => Self::Enabled,
            "no-email" => Self::NoEmail,
            "declined" => Self::Declined,
            "unavailable" => Self::Unavailable,
            "retry" => Self::Retry,
            "sign-in-again" => Self::SignInAgain,
            "provider-sign-in" => Self::ProviderSignIn,
            "signing-out" => Self::SigningOut,
            "refused" => Self::Refused,
            "refused-unconfirmed" => Self::RefusedUnconfirmed,
            "not-durable" => Self::NotDurable,
            "not-durable-unconfirmed" => Self::NotDurableUnconfirmed,
            "confirmation" => Self::Confirmation,
            "backlog" => Self::Backlog,
            _ => return None,
        })
    }

    fn claude_fixture(self, model: &mut Settings) {
        if matches!(
            self,
            Self::ClaudeReview | Self::ClaudeSigning | Self::ClaudeReady | Self::ClaudeCleanup
        ) {
            use kumochi_protocol::message::{ClaudeCodeView, ClaudeVersion};
            let version = ClaudeVersion {
                major: 2,
                minor: 1,
                patch: 306,
            };
            let view = match self {
                Self::ClaudeSigning => ClaudeCodeView::SigningIn,
                Self::ClaudeCleanup => ClaudeCodeView::CleanupPending,
                Self::ClaudeReady => ClaudeCodeView::Ready {
                    version,
                    analytics_on: true,
                },
                _ => ClaudeCodeView::SignedOut { version },
            };
            model.claude.receive(Some(1), 1, view);
            model.claude.open_connect();
        }
    }

    fn model(self) -> Settings {
        let mut model = Settings::default();
        if self == Self::Offline {
            return model;
        }
        assert_eq!(
            model.set_view(
                Snapshot {
                    assistant_name: AssistantName::new(
                        kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME
                    )
                    .expect("default assistant name"),
                    user_name: None,
                    onboarded: true,
                    onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
                    main_thread: ThreadId(1),
                    max_frame_rate: MaxFrameRate::AUTOMATIC,
                    default_model: None,
                    dock_position: kumochi_protocol::message::DockPosition::TopCenter,
                    show_in_taskbar: false,
                    talk_back: false,
                    stt_unload: kumochi_protocol::message::SttUnload::Kept,
                    hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
                    screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
                    show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
                },
                Update::Unsolicited
            ),
            None,
            "the fixture installs an unsolicited settings snapshot"
        );
        if self == Self::GeneralError {
            model.change(Field::Assistant, "");
            assert!(
                !model.save(Field::Assistant, |_| panic!(
                    "the invalid fixture draft must not send"
                )),
                "the empty fixture draft is rejected locally"
            );
            return model;
        }
        let view = match self {
            Self::Waiting | Self::Offline => return model,
            Self::SignedOut | Self::Unconfirmed | Self::Backlog => AccountView::SignedOut {
                revocation_unconfirmed: self == Self::Unconfirmed,
            },
            Self::SigningIn => AccountView::SigningIn,
            Self::Busy => AccountView::Busy,
            Self::SigningOut
            | Self::Refused
            | Self::RefusedUnconfirmed
            | Self::NotDurable
            | Self::NotDurableUnconfirmed => AccountView::SigningOut {
                cleanup: match self {
                    Self::Refused | Self::RefusedUnconfirmed => Cleanup::Refused,
                    Self::NotDurable | Self::NotDurableUnconfirmed => Cleanup::NotDurable,
                    _ => Cleanup::Running,
                },
                revocation_unconfirmed: matches!(
                    self,
                    Self::SigningOut | Self::RefusedUnconfirmed | Self::NotDurableUnconfirmed
                ),
            },
            _ => AccountView::SignedIn {
                email: (self != Self::NoEmail).then(|| {
                    AccountEmail::new("person@example.invalid").expect("fixed display email")
                }),
                plan: match self {
                    Self::Declined => PlanState::Declined,
                    Self::Unavailable => PlanState::Unavailable,
                    _ => PlanState::Enabled,
                },
                recovery: match self {
                    Self::Retry => Some(AccountRecovery::Retry),
                    Self::SignInAgain => Some(AccountRecovery::SignInAgain),
                    _ => None,
                },
            },
        };
        assert!(
            model.accounts.receive(Some(1), 1, view),
            "the fixed fixture generation is current"
        );
        let error = match self {
            Self::Backlog => Some(ErrorKey::RevocationBacklog),
            Self::Retry => Some(ErrorKey::AccountOffline),
            Self::ProviderSignIn => Some(ErrorKey::AccountRefused),
            _ => None,
        };
        if let Some(key) = error {
            let action = if self == Self::ProviderSignIn {
                Action::SignIn
            } else {
                Action::Retry
            };
            model
                .accounts
                .error(&UserError::new(key, Vec::new(), vec![action]).expect("fixed error"));
        }
        self.claude_fixture(&mut model);
        model
    }
}

pub(crate) fn run(state: State, appearance: Option<Appearance>, delay: Duration) {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_global(Strings::english());
            if open(state, appearance, delay, cx).is_err() {
                cx.quit();
            }
        });
}

fn open(
    state: State,
    appearance: Option<Appearance>,
    delay: Duration,
    cx: &mut App,
) -> Result<(), OpenError> {
    if let Some(appearance) = appearance {
        Theme::change(
            if appearance == Appearance::Dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            None,
            cx,
        );
        if matches!(appearance, Appearance::Large | Appearance::Constrained) {
            Theme::update(cx, |theme| theme.font_size = px(24.));
        }
    }
    let model = cx.new(|_| state.model());
    let (events, _receiver) = async_channel::unbounded();
    let rem = f32::from(Theme::global(cx).font_size);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(100.), px(100.)),
            window_size(appearance, rem),
        ))),
        titlebar: None,
        kind: WindowKind::PopUp,
        window_background: WindowBackgroundAppearance::Transparent,
        is_resizable: false,
        ..Default::default()
    };
    let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title(&cx.global::<Strings>().get(Text::Settings));
        window.on_window_should_close(cx, |_, cx| {
            cx.quit();
            true
        });
        cx.new(|cx| {
            let mut view = SettingsView::new(model, WeakEntity::new_invalid(), events, window, cx);
            if state == State::GeneralError {
                view.fixture_general();
            } else {
                view.fixture_accounts();
            }
            view
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    window
        .update(cx, |_, window, cx| {
            gpui_kit::base::Root::update(window, cx, |root, _, cx| {
                root.style().background = Some(gpui_kit::transparent_black().into());
                cx.notify();
            });
        })
        .map_err(|error| OpenError::Gpui(error.into()))?;
    if state == State::Confirmation {
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| view.confirm_sign_out(window, cx));
            })
            .map_err(|error| OpenError::Gpui(error.into()))?;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        cx.update(|cx| cx.quit());
    })
    .detach();
    Ok(())
}

fn window_size(appearance: Option<Appearance>, rem: f32) -> Size<Pixels> {
    if appearance == Some(Appearance::Constrained) {
        return size(px(768.), px(544.));
    }
    let [width, height] = super::threads::PANEL_REMS.map(|value| px(value * rem));
    size(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_window_does_not_grow_with_larger_text() {
        for rem in [16., 24.] {
            assert_eq!(
                window_size(Some(Appearance::Constrained), rem),
                size(px(768.), px(544.))
            );
        }
        assert_eq!(
            window_size(Some(Appearance::Large), 24.),
            size(px(1152.), px(816.))
        );
    }

    #[test]
    fn fixed_states_parse_exactly() {
        for (name, state) in [
            ("general-error", State::GeneralError),
            ("waiting", State::Waiting),
            ("offline", State::Offline),
            ("signed-out", State::SignedOut),
            ("unconfirmed", State::Unconfirmed),
            ("signing-in", State::SigningIn),
            ("busy", State::Busy),
            ("enabled", State::Enabled),
            ("no-email", State::NoEmail),
            ("declined", State::Declined),
            ("unavailable", State::Unavailable),
            ("retry", State::Retry),
            ("sign-in-again", State::SignInAgain),
            ("provider-sign-in", State::ProviderSignIn),
            ("signing-out", State::SigningOut),
            ("refused", State::Refused),
            ("refused-unconfirmed", State::RefusedUnconfirmed),
            ("not-durable", State::NotDurable),
            ("not-durable-unconfirmed", State::NotDurableUnconfirmed),
            ("confirmation", State::Confirmation),
            ("backlog", State::Backlog),
        ] {
            assert_eq!(State::parse(name), Some(state));
        }
    }

    #[test]
    fn general_error_uses_the_real_field_validation_without_an_account() {
        let model = State::GeneralError.model();
        assert_eq!(
            model.field(Field::Assistant).error,
            Some(crate::settings::Error::Validation(
                kumochi_protocol::message::NameError::Empty
            ))
        );
        assert_eq!(model.accounts.view, None);
        for value in ["General-error", "general-erro", "general-error "] {
            assert_eq!(State::parse(value), None);
        }
    }

    #[test]
    fn cleanup_fixtures_keep_local_and_remote_outcomes_separate() {
        for (state, cleanup, revocation_unconfirmed) in [
            (State::SigningOut, Cleanup::Running, true),
            (State::Refused, Cleanup::Refused, false),
            (State::RefusedUnconfirmed, Cleanup::Refused, true),
            (State::NotDurable, Cleanup::NotDurable, false),
            (State::NotDurableUnconfirmed, Cleanup::NotDurable, true),
        ] {
            assert_eq!(
                state.model().accounts.view,
                Some(AccountView::SigningOut {
                    cleanup,
                    revocation_unconfirmed
                })
            );
        }
    }

    #[test]
    fn recovery_fixtures_include_the_authoritative_error() {
        let provider = State::ProviderSignIn.model();
        assert_eq!(
            provider.accounts.view,
            Some(AccountView::SignedIn {
                email: Some(AccountEmail::new("person@example.invalid").expect("fixed email")),
                plan: PlanState::Enabled,
                recovery: None,
            })
        );
        assert_eq!(
            provider.accounts.error,
            Some(
                UserError::new(ErrorKey::AccountRefused, Vec::new(), vec![Action::SignIn])
                    .expect("bounded error")
            )
        );
        for invalid in ["provider-sign-i", "provider-sign-in ", "Provider-sign-in"] {
            assert_eq!(State::parse(invalid), None);
        }
        let backlog = State::Backlog.model();
        assert_eq!(
            backlog.accounts.view,
            Some(AccountView::SignedOut {
                revocation_unconfirmed: false
            })
        );
        assert!(!backlog.accounts.send_failed);
        assert_eq!(
            backlog.accounts.error,
            Some(
                UserError::new(ErrorKey::RevocationBacklog, Vec::new(), vec![Action::Retry])
                    .unwrap()
            )
        );
        let retry = State::Retry.model();
        assert_eq!(
            retry.accounts.error,
            Some(
                UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry]).unwrap()
            )
        );
        assert!(matches!(
            retry.accounts.view,
            Some(AccountView::SignedIn {
                recovery: Some(AccountRecovery::Retry),
                ..
            })
        ));
    }
}
