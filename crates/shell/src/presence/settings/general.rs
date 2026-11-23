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
use kumochi_protocol::BUILD_ID;

use super::super::{PresenceEvent, frame_rate, send};
use super::SettingsView;
use crate::settings::{Field, Save, Settings, TaskbarChoice};
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
        let selected = self.model.read(cx).dock_position_index();
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
                    .children(Settings::dock_position_options().enumerate().map(
                        |(ix, (position, label))| {
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
                                    let radio =
                                        radio.debug_selector(move || format!("dock-position-{ix}"));
                                    radio
                                })
                        },
                    )),
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
                let result = view
                    .owner
                    .update(cx, |owner, cx| {
                        owner.request_save(Save::Taskbar(choice), cx).result
                    })
                    .unwrap_or_else(|_| {
                        view.model.update(cx, |state, _| {
                            state.apply_save(Save::Taskbar(choice), None).result
                        })
                    });
                if result == Ok(false) {
                    return;
                }
                view.taskbar_notice = result.err();
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
        let (saving, error) = state.field_status(field, strings);
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
        let (choices, selected) = state.default_model_options(strings);
        let (picks, radios): (Vec<_>, Vec<_>) = choices
            .into_iter()
            .enumerate()
            .map(|(ix, (connection, slug, label))| {
                let radio = Radio::new(("default-model", ix)).label(label);
                #[cfg(test)]
                let radio = radio.debug_selector(move || format!("default-model-{ix}"));
                ((connection, slug), radio)
            })
            .unzip();
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
        let state = self.model.read(cx);
        let view = state.view();
        let cap = view.map(|view| view.max_frame_rate).unwrap_or_default();
        let selected = state.frame_rate_index();
        let label = strings.get(Text::MaxFrameRate);
        let owner = self.owner.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().child(label))
            .child(
                RadioGroup::horizontal("max-frame-rate")
                    .children(Settings::frame_rate_options().map(|rate| match rate.fps() {
                        Some(fps) => Radio::new(("fps", usize::from(fps))).label(fps.to_string()),
                        None => Radio::new("fps-auto").label(strings.get(Text::FrameRateAutomatic)),
                    }))
                    .selected_index(Some(selected))
                    .disabled(view.is_none())
                    .on_click(move |&ix, _, cx| {
                        if let Some(rate) = Settings::frame_rate_at(ix) {
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

    use super::super::SettingsTab;
    use crate::settings::{Settings, Update};
    use crate::strings::Strings;

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

    pub(in crate::presence::settings) fn saved() -> SettingsView {
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
            shortcut: kumochi_protocol::message::ShortcutView::Default,
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
                view.tab = SettingsTab::Accounts;
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
                view.tab = SettingsTab::Accounts;
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
}
