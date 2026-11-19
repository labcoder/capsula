#[cfg(test)]
use gpui_kit::InteractiveElement as _;
use gpui_kit::base::RadioGroup as PositionGroup;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::Input;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme as _, Disableable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, IntoElement, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window,
    div,
};
use kumochi_protocol::{
    BUILD_ID,
    message::{ConnectionKind, DockPosition, MaxFrameRate, ModelSlug},
};

use super::super::{PresenceEvent, frame_rate, send};
use super::{SettingsView, field_status};
use crate::settings::{Field, Settings, TaskbarChoice};
use crate::strings::{Strings, Text};

impl SettingsView {
    pub(super) fn general(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let quit = self.events.clone();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(
                self.model
                    .read(cx)
                    .view()
                    .is_none()
                    .then(|| div().text_sm().child(strings.get(Text::SettingsOffline))),
            )
            .children(
                [
                    self.dock_notice,
                    self.taskbar_notice,
                    self.model.read(cx).native_taskbar_notice,
                ]
                .into_iter()
                .flatten()
                .map(|notice| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(strings.get(notice))
                }),
            )
            .children(self.model.read(cx).notice().map(|error| {
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.error(error))
            }))
            .child(self.field(Field::Assistant, cx))
            .child(self.field(Field::User, cx))
            .child(self.default_model(cx))
            .child(self.frame_rate(window, cx))
            .child(self.dock_positions(cx))
            .child(self.taskbar(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::SettingsSaveHelp)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child(strings.get(Text::SettingsVersion)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(BUILD_ID),
                            ),
                    )
                    .child(
                        div()
                            .map(|element| {
                                #[cfg(test)]
                                let element =
                                    element.debug_selector(|| "settings-copy-build".into());
                                element
                            })
                            .child(
                                Clipboard::new("settings-copy-build")
                                    .value(BUILD_ID)
                                    .accessibility_label(strings.get(Text::SettingsCopyBuild)),
                            ),
                    ),
            )
            .child(
                Button::new("settings-quit")
                    .outline()
                    .label(strings.get(Text::Quit))
                    .on_click(move |_, _, _| send(&quit, PresenceEvent::Quit)),
            )
    }

    fn dock_positions(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let selected = dock_choice_index(state);
        let label = strings.get(Text::DockPosition);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_sm().child(label.clone()))
            .child(
                PositionGroup::new("settings-dock-position")
                    .aria_label(label)
                    .grid()
                    .grid_cols(3)
                    .gap_2()
                    .children(
                        DockPosition::ALL
                            .into_iter()
                            .zip(Text::POSITIONS)
                            .enumerate()
                            .map(|(ix, (position, label))| {
                                Radio::new(("dock-position", ix))
                                    .label(strings.get(label))
                                    .checked(selected == Some(ix))
                                    .on_change(cx.listener(move |view, _, _, cx| {
                                        let owner = view.owner.clone();
                                        let view = cx.weak_entity();
                                        cx.defer(move |cx| {
                                            let moved = owner
                                                .update(cx, |owner, cx| {
                                                    owner.save_dock_position(position, cx)
                                                })
                                                .is_ok_and(|result| result.is_ok());
                                            let _gone = view.update(cx, |view, cx| {
                                                view.dock_notice = (!moved)
                                                    .then_some(Text::SettingsPositionUnavailable);
                                                cx.notify();
                                            });
                                        });
                                    }))
                                    .map(|radio| {
                                        #[cfg(test)]
                                        let radio = radio
                                            .debug_selector(move || format!("dock-position-{ix}"));
                                        radio
                                    })
                            }),
                    ),
            )
    }

    fn taskbar(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        Checkbox::new("settings-taskbar")
            .label(cx.global::<Strings>().taskbar())
            .checked(state.taskbar_choice() == Some(TaskbarChoice::Shown))
            .disabled(state.view().is_none())
            .on_change(cx.listener(|view, checked, _, cx| {
                let choice = if *checked {
                    TaskbarChoice::Shown
                } else {
                    TaskbarChoice::Hidden
                };
                if view.model.read(cx).taskbar_choice() == Some(choice) {
                    return;
                }
                let sent = view
                    .owner
                    .update(cx, |owner, cx| owner.save_taskbar(choice, cx))
                    .unwrap_or(false);
                view.taskbar_notice = (!sent).then_some(Text::SettingsUnavailable);
                cx.notify();
            }))
            .map(|checkbox| {
                #[cfg(test)]
                let checkbox = checkbox.debug_selector(|| "settings-taskbar".into());
                checkbox
            })
    }

    fn field(&self, field: Field, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let label = strings.get(match field {
            Field::Assistant => Text::AssistantName,
            Field::User => Text::UserName,
        });
        let (saving, error) = field_status(state, field, strings);
        let ix = usize::from(field == Field::User);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(label.clone()))
            .child(
                Input::new(&self.inputs[ix])
                    .aria_label(label)
                    .disabled(state.view().is_none())
                    .readonly(state.field(field).saving),
            )
            .children((field == Field::User).then(|| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::UserNameHelp))
            }))
            .children(saving.map(|saving| div().text_xs().child(saving)))
            .children(error.map(|error| div().text_sm().text_color(cx.theme().danger).child(error)))
    }

    /// One radio per model on offer; a saved default no catalog lists comes first, by its slug.
    fn default_model(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = self.model.read(cx);
        let current = state.default_model_choice();
        let is = |connection: ConnectionKind, slug: &ModelSlug| {
            current
                .is_some_and(|current| current.connection == connection && current.model == *slug)
        };
        let mut choices: Vec<(ConnectionKind, ModelSlug, String)> = state
            .model_choices()
            .map(|(connection, model)| {
                let name = model.display_name.as_str().to_owned();
                (connection, model.slug.clone(), name)
            })
            .collect();
        let mut labelled = labels(&choices, strings);
        if let Some(current) = current
            && !choices
                .iter()
                .any(|(connection, slug, _)| is(*connection, slug))
        {
            let name = current.model.as_str().to_owned();
            labelled.insert(0, label(current.connection, &name, &[], strings));
            choices.insert(0, (current.connection, current.model.clone(), name));
        }
        let selected = choices
            .iter()
            .position(|(connection, slug, _)| is(*connection, slug));
        let radios: Vec<_> = labelled
            .into_iter()
            .enumerate()
            .map(|(ix, label)| {
                let radio = Radio::new(("default-model", ix)).label(label);
                #[cfg(test)]
                let radio = radio.debug_selector(move || format!("default-model-{ix}"));
                radio
            })
            .collect();
        let picks: Vec<_> = choices
            .into_iter()
            .map(|(connection, slug, _)| (connection, slug))
            .collect();
        let owner = self.owner.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(strings.get(Text::DefaultModel)))
            .children(radios.is_empty().then(|| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::ModelsNotListed))
            }))
            .when(!radios.is_empty(), |row| {
                row.child(
                    RadioGroup::vertical("default-model")
                        .children(radios)
                        .selected_index(selected)
                        .disabled(state.view().is_none())
                        .on_click(move |&ix, _, cx| {
                            if let Some((connection, slug)) = picks.get(ix).cloned() {
                                let _gone = owner.update(cx, |owner, cx| {
                                    owner.save_default_model(connection, slug, cx);
                                });
                            }
                        }),
                )
            })
            .children(
                state
                    .model_saving()
                    .then(|| div().text_xs().child(strings.get(Text::SettingsSaving))),
            )
    }

    /// Automatic, then each cap, with the rate a paced animation draws at now.
    fn frame_rate(&self, window: &Window, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let view = self.model.read(cx).view();
        let cap = view.map(|view| view.max_frame_rate).unwrap_or_default();
        let selected = choice_index(self.model.read(cx));
        let label = strings.get(Text::MaxFrameRate);
        let owner = self.owner.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(label))
            .child(
                RadioGroup::horizontal("max-frame-rate")
                    .child(Radio::new("fps-auto").label(strings.get(Text::FrameRateAutomatic)))
                    .children(
                        MaxFrameRate::CAPS.map(|fps| {
                            Radio::new(("fps", usize::from(fps))).label(fps.to_string())
                        }),
                    )
                    .selected_index(Some(selected))
                    .disabled(view.is_none())
                    .on_click(move |&ix, _, cx| {
                        if let Some(rate) = rate_at(ix) {
                            let _gone =
                                owner.update(cx, |owner, cx| owner.save_max_frame_rate(rate, cx));
                        }
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    // The panel opens beside the dock, so it's on the dock's display.
                    .child(
                        strings.frame_rate(frame_rate::effective(cap, window.refresh_interval(cx))),
                    ),
            )
    }
}

/// The radio showing the newest choice, queued or saved: Kit gives a checked radio no click
/// handler, so showing the saved cap while a new one is queued would swallow a quick reversal.
/// Each offered row's label, by [`label`] against the other offered rows.
fn labels(choices: &[(ConnectionKind, ModelSlug, String)], strings: &Strings) -> Vec<String> {
    choices
        .iter()
        .map(|(connection, _, name)| label(*connection, name, choices, strings))
        .collect()
}

/// A key model's row always names the key, since each of its requests is billed; another row names
/// its connection only when another connection in `offered` has a model of the same name.
fn label(
    connection: ConnectionKind,
    name: &str,
    offered: &[(ConnectionKind, ModelSlug, String)],
    strings: &Strings,
) -> String {
    let shared = offered
        .iter()
        .any(|(other, _, other_name)| *other != connection && other_name == name);
    let provider = match connection {
        ConnectionKind::AnthropicKey => Text::AnthropicTitle,
        _ if !shared => return name.to_owned(),
        ConnectionKind::ChatGpt => Text::AccountProvider,
        ConnectionKind::ClaudeCode => Text::ClaudeProvider,
    };
    strings.model_on(name, &strings.get(provider))
}

fn choice_index(model: &Settings) -> usize {
    model
        .max_frame_rate_choice()
        .and_then(MaxFrameRate::fps)
        .and_then(|fps| MaxFrameRate::CAPS.iter().position(|&each| each == fps))
        .map_or(0, |ix| ix + 1)
}

/// The cap behind radio `ix`: Automatic, then each of [`MaxFrameRate::CAPS`].
fn rate_at(ix: usize) -> Option<MaxFrameRate> {
    match ix.checked_sub(1) {
        None => Some(MaxFrameRate::AUTOMATIC),
        Some(ix) => MaxFrameRate::CAPS
            .get(ix)
            .and_then(|&fps| MaxFrameRate::cap(fps).ok()),
    }
}

fn dock_choice_index(state: &Settings) -> Option<usize> {
    let choice = state.dock_position_choice()?;
    DockPosition::ALL
        .iter()
        .position(|position| *position == choice)
}

#[cfg(test)]
pub(super) mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WeakEntity,
        WindowBounds, WindowOptions, px, size,
    };
    use kumochi_protocol::message::{AssistantName, MaxFrameRate, SettingsView, ThreadId};

    use super::{choice_index, dock_choice_index, labels, rate_at};
    use crate::settings::{Settings, Update};
    use crate::strings::Strings;

    #[test]
    fn a_key_row_always_names_the_key_and_another_only_on_a_shared_name() {
        use kumochi_protocol::message::ConnectionKind::{AnthropicKey, ChatGpt, ClaudeCode};
        let row = |connection, slug: &str, name: &str| {
            (
                connection,
                kumochi_protocol::message::ModelSlug::new(slug).expect("slug"),
                name.to_owned(),
            )
        };
        let choices = [
            row(ChatGpt, "m-0", "GPT Luna"),
            row(AnthropicKey, "claude-haiku-5-5", "Claude Haiku 5.5"),
            row(AnthropicKey, "claude-opus-5-5", "Claude Opus 5.5"),
            row(ClaudeCode, "haiku", "Claude Haiku 5.5"),
            row(ClaudeCode, "sonnet", "Claude Sonnet 5.5"),
        ];
        assert_eq!(
            labels(&choices, &Strings::english()),
            [
                "GPT Luna",
                "Claude Haiku 5.5 (Anthropic API key)",
                "Claude Opus 5.5 (Anthropic API key)",
                "Claude Haiku 5.5 (Claude Code)",
                "Claude Sonnet 5.5",
            ]
        );
    }

    #[gpui::test]
    fn the_copy_control_copies_the_running_build(cx: &mut TestAppContext) {
        let (window, _) = open(cx);
        let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
        visual.simulate_resize(size(px(768.), px(1000.)));
        visual.write_to_clipboard(gpui_kit::ClipboardItem::new_string("not the build".into()));
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let point = visual
            .debug_bounds("settings-copy-build")
            .expect("copy control")
            .center();
        visual.simulate_click(point, gpui_kit::Modifiers::default());
        visual.run_until_parked();
        assert_eq!(
            visual.read_from_clipboard().and_then(|item| item.text()),
            Some(kumochi_protocol::BUILD_ID.to_owned())
        );
    }

    #[gpui::test]
    fn dock_and_taskbar_failures_keep_independent_notices(cx: &mut TestAppContext) {
        let (window, view) = open(cx);
        let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
        visual.simulate_resize(size(px(768.), px(1000.)));
        for selectors in [
            ["dock-position-0", "settings-taskbar"],
            ["settings-taskbar", "dock-position-0"],
        ] {
            visual.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.dock_notice = None;
                    view.taskbar_notice = None;
                    cx.notify();
                });
            });
            for selector in selectors {
                visual.run_until_parked();
                visual.update(|window, cx| window.draw(cx).clear(cx));
                let point = visual
                    .debug_bounds(selector)
                    .expect("General control")
                    .center();
                visual.simulate_click(point, gpui_kit::Modifiers::default());
            }
            visual.run_until_parked();
            visual.update(|_, cx| {
                assert_eq!(
                    view.read(cx).dock_notice,
                    Some(crate::strings::Text::SettingsPositionUnavailable)
                );
                assert_eq!(
                    view.read(cx).taskbar_notice,
                    Some(crate::strings::Text::SettingsUnavailable)
                );
            });
        }
    }

    #[test]
    fn the_checked_dock_cell_follows_saved_pending_and_refused_choices() {
        use kumochi_protocol::message::{DockPosition, ErrorKey, UserError};
        for (position, index) in [
            (DockPosition::TopLeft, 0),
            (DockPosition::TopCenter, 1),
            (DockPosition::TopRight, 2),
            (DockPosition::MiddleLeft, 3),
            (DockPosition::Center, 4),
            (DockPosition::MiddleRight, 5),
            (DockPosition::BottomLeft, 6),
            (DockPosition::BottomCenter, 7),
            (DockPosition::BottomRight, 8),
        ] {
            let mut model = Settings::default();
            assert_eq!(dock_choice_index(&model), None);
            let mut snapshot = saved();
            snapshot.dock_position = position;
            model.set_view(snapshot, Update::Unsolicited);
            assert_eq!(dock_choice_index(&model), Some(index));
            model.send_dock_position(DockPosition::BottomLeft, |_| true);
            assert_eq!(dock_choice_index(&model), Some(6));
            model.send_dock_position(DockPosition::Center, |_| true);
            assert_eq!(dock_choice_index(&model), Some(4));
            let mut answer = saved();
            answer.dock_position = DockPosition::BottomLeft;
            model.set_view(answer.clone(), Update::Answer);
            assert_eq!(dock_choice_index(&model), Some(4));
            model.error(UserError::from(ErrorKey::SettingsRefused));
            assert_eq!(dock_choice_index(&model), Some(6));
            model.set_view(answer, Update::Answer);
            assert_eq!(dock_choice_index(&model), Some(6));
        }
    }

    #[gpui::test]
    fn an_enabled_offline_dock_cell_reports_its_missing_owner(cx: &mut TestAppContext) {
        let (window, view) = open(cx);
        cx.update(|cx| {
            view.read(cx).model.clone().update(cx, |model, cx| {
                model.disconnected();
                cx.notify();
            });
        });
        cx.run_until_parked();
        let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let point = visual
            .debug_bounds("dock-position-0")
            .expect("top left cell")
            .center();
        visual.simulate_click(point, gpui_kit::Modifiers::default());
        visual.run_until_parked();
        visual.update(|_, cx| {
            assert_eq!(
                view.read(cx).dock_notice,
                Some(crate::strings::Text::SettingsPositionUnavailable),
                "the offline radio runs its deferred callback and reports the missing owner"
            );
        });
    }

    fn saved() -> SettingsView {
        SettingsView {
            assistant_name: AssistantName::new("Kumo").expect("valid"),
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
        }
    }

    /// The Settings window as the shell opens it, over a saved Automatic cap and no owner.
    pub(in crate::presence::settings) fn open(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<super::SettingsView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| {
                let mut model = Settings::default();
                assert_eq!(model.set_view(saved(), Update::Unsolicited), None);
                model
            });
            let (events, _) = async_channel::unbounded();
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(480.), px(640.)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    super::SettingsView::new(model, WeakEntity::new_invalid(), events, window, cx)
                })
            })
            .expect("a window")
        })
    }

    #[gpui::test]
    fn a_refresh_only_change_redraws_the_effective_rate(cx: &mut TestAppContext) {
        let (window, view) = open(cx);
        let display = cx
            .update_window(window, |_, window, cx| {
                window.display(cx).map(|display| display.id())
            })
            .expect("the window")
            .expect("a display");
        let notified = Rc::new(Cell::new(0));
        let _watch = cx.update(|cx| {
            let notified = Rc::clone(&notified);
            cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
        });
        let hertz_144 = Duration::from_secs(1) / 144;
        cx.simulate_display_refresh_interval_change(display, Some(hertz_144));
        assert_eq!(notified.get(), 1, "the view redraws its effective rate");
        let refresh = cx
            .update_window(window, |_, window, cx| window.refresh_interval(cx))
            .expect("the window");
        assert_eq!(refresh, Some(hertz_144));
    }

    #[gpui::test]
    fn provider_sign_in_errors_render_recovery_while_account_details_are_still_signed_in(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::{AccountView, Action, ErrorKey, PlanState, UserError};
        let (window, view) = open(cx);
        for key in [ErrorKey::AccountRefused, ErrorKey::SignInNeeded] {
            view.update(cx, |view, cx| {
                view.tab = 1;
                view.model.update(cx, |model, cx| {
                    assert!(model.accounts.receive(
                        Some(1),
                        1,
                        AccountView::SignedIn {
                            email: None,
                            plan: PlanState::Enabled,
                            recovery: None,
                        }
                    ));
                    model.accounts.error(
                        &UserError::new(key, Vec::new(), vec![Action::SignIn])
                            .expect("bounded error"),
                    );
                    cx.notify();
                });
                cx.notify();
            });
            cx.run_until_parked();
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            assert!(
                frame.debug_bounds("account-sign-in-recovery").is_some(),
                "{key:?}"
            );
        }
    }

    #[gpui::test]
    fn provider_recovery_button_is_enabled_and_unrelated_sign_in_does_not_hide_enable(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::{
            AccountOp, AccountView, Action, ErrorKey, Incarnation, PlanState, UserError,
        };
        let owner = crate::presence::test_fixture::presence(cx);
        let (window, view) = open(cx);
        view.update(cx, |view, cx| {
            view.owner = owner.downgrade();
            owner.update(cx, |this, _| {
                this.settings = view.model.clone();
                this.session = Some(crate::link::Session::detached(1, Incarnation(1)));
            });
        });
        for (key, waiting, recovery) in [
            (ErrorKey::AccountRefused, false, true),
            (ErrorKey::SignInNeeded, true, true),
            (ErrorKey::PlanUnavailable, false, false),
        ] {
            view.update(cx, |view, cx| {
                view.tab = 1;
                view.model.update(cx, |model, cx| {
                    model.accounts.receive(
                        Some(1),
                        1,
                        AccountView::SignedIn {
                            email: None,
                            plan: PlanState::Declined,
                            recovery: None,
                        },
                    );
                    model.accounts.error(
                        &UserError::new(key, Vec::new(), vec![Action::SignIn]).expect("bounded"),
                    );
                    model.accounts.waiting = waiting.then_some(AccountOp::SignIn);
                    cx.notify();
                });
                cx.notify();
            });
            cx.run_until_parked();
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            assert_eq!(
                frame.debug_bounds("account-sign-in-recovery").is_some(),
                recovery,
                "{key:?}"
            );
            let bounds = frame
                .debug_bounds(if recovery {
                    "account-sign-in-recovery"
                } else {
                    "account-actions"
                })
                .expect("the offered first action");
            frame.simulate_click(
                gpui_kit::point(bounds.left() + px(10.0), bounds.center().y),
                gpui_kit::Modifiers::default(),
            );
            view.read_with(cx, |view, cx| {
                let accounts = &view.model.read(cx).accounts;
                assert_eq!(
                    accounts.send_failed, !waiting,
                    "{key:?}: an enabled action reached the refusing fixture link"
                );
                assert_eq!(accounts.waiting, waiting.then_some(AccountOp::SignIn));
            });
        }
    }

    #[gpui::test]
    fn account_confirmation_closes_on_disconnect_state_loss_or_new_session(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::component::WindowExt as _;
        use kumochi_protocol::message::{AccountView, PlanState};
        let (window, view) = open(cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        let model = cx.update(|cx| view.read(cx).model.clone());
        let signed_in = AccountView::SignedIn {
            email: None,
            plan: PlanState::Enabled,
            recovery: None,
        };
        for outcome in 0..3 {
            cx.update(|cx| {
                model.update(cx, |state, cx| {
                    assert!(state.accounts.receive(Some(1), 1, signed_in.clone()));
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.confirm_sign_out(window, cx));
                assert!(window.has_active_dialog(cx));
            })
            .expect("settings window");
            cx.update(|cx| {
                model.update(cx, |state, cx| {
                    match outcome {
                        0 => state.accounts.disconnected(),
                        1 => {
                            assert!(state.accounts.receive(Some(1), 1, AccountView::Busy));
                        }
                        _ => {
                            assert!(state.accounts.receive(Some(2), 2, signed_in.clone()));
                        }
                    }
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                assert!(!window.has_active_dialog(cx));
                window.draw(cx).clear(cx);
            })
            .expect("settings window");
        }
    }

    #[test]
    fn a_quick_reversal_lands_on_an_unchecked_radio() {
        let mut model = Settings::default();
        assert_eq!(model.set_view(saved(), Update::Unsolicited), None);
        assert_eq!(choice_index(&model), 0);
        let rate = rate_at(5).expect("a radio");
        assert_eq!(rate.fps(), Some(144));
        assert!(model.send_max_frame_rate(rate, |_| true));
        assert_eq!(
            choice_index(&model),
            5,
            "144 shows checked before the answer"
        );
        let automatic = rate_at(0).expect("a radio");
        assert_ne!(
            choice_index(&model),
            0,
            "so Automatic is unchecked and takes the click"
        );
        assert!(model.send_max_frame_rate(automatic, |_| true));
        assert_eq!(choice_index(&model), 0);
        assert_eq!(rate_at(6), None);
    }
}
