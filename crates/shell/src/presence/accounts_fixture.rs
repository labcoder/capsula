//! Offline capture of the production Settings view, with no presence or transport owner.

use std::time::Duration;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{
    App, AppContext as _, Bounds, Styled as _, WeakEntity, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use kumochi_protocol::message::{
    AccountEmail, AccountRecovery, AccountView, Action, AssistantName, Cleanup, ErrorKey,
    MaxFrameRate, PlanState, SettingsView as Snapshot, ThreadId, UserError,
};

use super::{OpenError, settings_view::SettingsView};
use crate::settings::{Settings, Update};
use crate::strings::{Strings, Text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Appearance {
    Light,
    Dark,
    Large,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
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
                    main_thread: ThreadId(1),
                    max_frame_rate: MaxFrameRate::AUTOMATIC,
                    default_model: None,
                },
                Update::Unsolicited
            ),
            None,
            "the fixture installs an unsolicited settings snapshot"
        );
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
            _ => None,
        };
        if let Some(key) = error {
            model
                .accounts
                .error(&UserError::new(key, Vec::new(), vec![Action::Retry]).expect("fixed error"));
        }
        model
    }
}

pub(crate) fn run(state: State, appearance: Option<Appearance>, delay: Duration) {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
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
        if appearance == Appearance::Large {
            Theme::update(cx, |theme| theme.font_size = px(24.));
        }
    }
    let model = cx.new(|_| state.model());
    let (events, _receiver) = async_channel::unbounded();
    let rem = f32::from(Theme::global(cx).font_size);
    let [width, height] = super::threads::PANEL_REMS.map(|value| px(value * rem));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(100.), px(100.)),
            size(width, height),
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
            view.fixture_accounts();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_states_parse_exactly() {
        for (name, state) in [
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
