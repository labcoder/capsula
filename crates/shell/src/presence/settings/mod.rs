//! Settings window state and shared frame, with one module per tab.

pub(super) mod accounts;
mod anthropic;
pub(super) mod claude;
mod general;
mod github;
mod privacy;
mod voice;
mod window;

pub(super) use window::SettingsPanel;

use std::rc::Rc;

use super::{Presence, PresenceEvent};
use crate::settings::{Field, Settings};
use crate::strings::{Strings, Text};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, InputEvent, InputState};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, scroll::ScrollableElement as _};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, Render, ScrollHandle, Styled as _, Subscription, Task,
    WeakEntity, Window, div,
};

gpui_kit::actions!(settings, [NextTab, PreviousTab]);

const SHOW_ANTHROPIC_KEY_ROW: bool = true;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    General,
    Accounts,
    Voice,
    Privacy,
}

impl SettingsTab {
    const ALL: [Self; 4] = [Self::General, Self::Accounts, Self::Voice, Self::Privacy];

    const fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Accounts => 1,
            Self::Voice => 2,
            Self::Privacy => 3,
        }
    }

    const fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    const fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

pub(super) struct SettingsView {
    pub(super) model: Entity<Settings>,
    pub(super) owner: WeakEntity<Presence>,
    pub(super) confirmation: Option<(
        u64,
        kumochi_protocol::message::AccountOp,
        crate::confirmation::Handle,
    )>,
    #[cfg(any(test, debug_assertions))]
    pub(super) fixture: bool,
    #[cfg(test)]
    github_configured: Option<bool>,
    events: async_channel::Sender<PresenceEvent>,
    inputs: [Entity<InputState>; 2],
    github_repos: Entity<SelectState<Vec<github::RepoChoice>>>,
    github_expiry: Option<Task<()>>,
    github_interactive: Option<u64>,
    focus: FocusHandle,
    voice_scroll: ScrollHandle,
    voice_focus: [FocusHandle; 5],
    pub(super) editing: super::escape::NameEdit,
    tab: SettingsTab,
    dock_notice: Option<Text>,
    taskbar_notice: Option<Text>,
    hold_notice: Option<Text>,
    pub(super) stop: bool,
    _events: Vec<Subscription>,
    _watch: Subscription,
    _focus_lost: Subscription,
    /// Redraws the effective rate when the window's display or its refresh changes.
    _display: Subscription,
}

impl SettingsView {
    pub(super) fn new(
        model: Entity<Settings>,
        owner: WeakEntity<Presence>,
        events: async_channel::Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("escape", Escape, Some("SettingsPanel")),
            KeyBinding::new("ctrl-tab", NextTab, Some("SettingsPanel")),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, Some("SettingsPanel")),
        ]);
        let inputs = [Field::Assistant, Field::User].map(|field| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                let value = model.read(cx).value(field).to_owned();
                input.set_value(value, window, cx);
                input
            })
        });
        let github_repos =
            cx.new(|cx| SelectState::new(Vec::<github::RepoChoice>::new(), None, window, cx));
        let mut subscriptions: Vec<_> = [Field::Assistant, Field::User]
            .into_iter()
            .zip(&inputs)
            .map(|(field, input)| {
                cx.subscribe_in(input, window, move |view, input, event, window, cx| {
                    view.name_input(field, input, event, window, cx);
                })
            })
            .collect();
        subscriptions.push(
            cx.subscribe_in(&github_repos, window, |view, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(choice)) = event {
                    view.choose_github_repo(choice, window, cx);
                }
            }),
        );
        let watch = cx.observe_in(&model, window, |view, _, window, cx| {
            view.sync_github(window, cx);
            view.schedule_github_on_open(cx);
            let account = &view.model.read(cx).accounts;
            if view
                .confirmation
                .as_ref()
                .is_some_and(|(generation, op, _)| {
                    account.generation != Some(*generation)
                        || !accounts::offers(account.view.as_ref(), *op)
                })
                && let Some((_, _, dialog)) = view.confirmation.take()
            {
                dialog.dismiss_owned(window, cx);
            }
            for (field, input) in [Field::Assistant, Field::User]
                .into_iter()
                .zip(&view.inputs)
            {
                let value = view.model.read(cx).value(field);
                if input.read(cx).value().as_ref() != value {
                    let value = value.to_owned();
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                }
            }
            let _gone = view.owner.update(cx, |owner, cx| owner.refresh_dock(cx));
            if view.tab == SettingsTab::Voice && view.voice_focus[4].contains_focused(window, cx) {
                view.voice_scroll.scroll_to_item(4);
            }
            cx.notify();
        });
        let view = cx.entity_id();
        let display = window.observe_window_display(move |_, cx| cx.notify(view));
        let focus_lost = cx.on_focus_lost(window, |view, window, cx| {
            restore_lost_focus(&view.focus, window, cx);
            cx.notify();
        });
        let voice_focus = Self::observe_voice_focus(window, cx, &mut subscriptions);
        let mut view = Self {
            model,
            owner,
            confirmation: None,
            #[cfg(any(test, debug_assertions))]
            fixture: false,
            #[cfg(test)]
            github_configured: None,
            events,
            inputs,
            github_repos,
            github_expiry: None,
            github_interactive: None,
            focus: cx.focus_handle().tab_stop(true),
            voice_scroll: ScrollHandle::new(),
            voice_focus,
            editing: Rc::default(),
            tab: SettingsTab::General,
            dock_notice: None,
            taskbar_notice: None,
            hold_notice: None,
            stop: false,
            _events: subscriptions,
            _watch: watch,
            _focus_lost: focus_lost,
            _display: display,
        };
        view.sync_github(window, cx);
        view
    }

    fn observe_voice_focus(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        subscriptions: &mut Vec<Subscription>,
    ) -> [FocusHandle; 5] {
        let voice_focus = std::array::from_fn(|_| cx.focus_handle());
        for (row, focus) in voice_focus.iter().enumerate() {
            subscriptions.push(cx.on_focus_in(focus, window, move |view, _, cx| {
                view.voice_scroll.scroll_to_item(row);
                cx.notify();
            }));
        }
        voice_focus
    }

    pub(super) fn editing(&self) -> Option<Field> {
        self.editing.get()
    }

    fn name_input(
        &mut self,
        field: Field,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        name_input_event(
            field,
            input,
            event,
            &self.editing,
            &self.focus,
            &self.model,
            &self.owner,
            window,
            cx,
        );
        cx.notify();
    }

    fn select_tab(
        &mut self,
        tab: SettingsTab,
        generation: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.switch_tab(tab, generation, cx);
        self.finish_edit(window, cx);
    }

    fn switch_tab(
        &mut self,
        tab: SettingsTab,
        generation: Option<u64>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.tab != tab {
            self.tab = tab;
            self.accounts_opened(generation, cx);
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn fixture_general(&mut self) {
        self.fixture = true;
    }

    #[cfg(debug_assertions)]
    pub(super) fn fixture_accounts(&mut self) {
        self.tab = SettingsTab::Accounts;
        self.fixture = true;
    }

    pub(super) fn focus(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.focus(&self.focus, cx);
    }

    pub(super) fn finish_edit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.dock_notice = None;
        self.taskbar_notice = None;
        self.hold_notice = None;
        self.confirmation = None;
        crate::confirmation::retire_in(window, cx);
        window.close_all_dialogs(cx);
        self.editing.set(None);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(super) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(field) = self.editing.get() {
            self.editing.set(None);
            self.model.update(cx, |state, cx| {
                state.cancel(field);
                cx.notify();
            });
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }
}

/// A name input's events, for every view that edits a name: Focus marks the field edited, Change
/// drafts it, and Enter or Blur saves it; Enter then focuses `after_enter`.
#[expect(
    clippy::too_many_arguments,
    reason = "the editing view's own state, passed through"
)]
pub(in crate::presence) fn name_input_event(
    field: Field,
    input: &Entity<InputState>,
    event: &InputEvent,
    editing: &super::escape::EditSlot<Field>,
    after_enter: &FocusHandle,
    model: &Entity<Settings>,
    owner: &WeakEntity<Presence>,
    window: &mut Window,
    cx: &mut App,
) {
    match event {
        InputEvent::Focus => editing.set(Some(field)),
        InputEvent::Change => {
            let value = input.read(cx).value();
            model.update(cx, |state, cx| {
                state.change(field, &value);
                cx.notify();
            });
        }
        InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur => {
            if editing.get() == Some(field) {
                editing.set(None);
            }
            let _gone = owner.update(cx, |owner, cx| owner.save_setting(field, cx));
            if matches!(event, InputEvent::PressEnter { .. }) {
                window.focus(after_enter, cx);
            }
        }
        InputEvent::PressEnter { .. } => {}
    }
}

impl SettingsView {
    /// The four tabs. Choosing one drops the notices and dialogs of the tab it leaves.
    fn tabs(&self, cx: &Context<'_, Self>) -> TabBar {
        let strings = cx.global::<Strings>();
        TabBar::new("settings-tabs")
            .selected_index(self.tab.index())
            .children(SettingsTab::ALL.map(|tab| {
                let item = Tab::new().label(strings.get(match tab {
                    SettingsTab::General => Text::General,
                    SettingsTab::Accounts => Text::Accounts,
                    SettingsTab::Voice => Text::Voice,
                    SettingsTab::Privacy => Text::Privacy,
                }));
                #[cfg(test)]
                let item = match tab {
                    SettingsTab::Voice => item.debug_selector(|| "settings-voice-tab".into()),
                    SettingsTab::Privacy => item.debug_selector(|| "settings-privacy-tab".into()),
                    _ => item,
                };
                item
            }))
            .on_click(cx.listener(|view, index, window, cx| {
                let Some(&tab) = SettingsTab::ALL.get(*index) else {
                    return;
                };
                view.dock_notice = None;
                view.taskbar_notice = None;
                view.hold_notice = None;
                view.confirmation = None;
                crate::confirmation::retire_in(window, cx);
                window.close_all_dialogs(cx);
                view.switch_tab(tab, view.github_generation(cx), cx);
                cx.notify();
            }))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let events = self.events.clone();
        let close = self.owner.clone();
        let stop_events = self.events.clone();
        let escape_owner = self.owner.clone();
        let stop_owner = self.owner.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(if self.focus.is_focused(window) {
                cx.theme().ring
            } else {
                cx.theme().border
            })
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .key_context("SettingsPanel")
            .track_focus(&self.focus)
            .on_action(move |_: &Escape, _, cx| {
                super::send_escape(&events, Some(&escape_owner), cx);
            })
            .on_action(cx.listener(|view, _: &NextTab, window, cx| {
                view.select_tab(view.tab.next(), view.github_generation(cx), window, cx);
            }))
            .on_action(cx.listener(|view, _: &PreviousTab, window, cx| {
                view.select_tab(view.tab.previous(), view.github_generation(cx), window, cx);
            }))
            .child(
                div()
                    .p_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().child(strings.get(Text::Settings)))
                    .child(
                        Button::new("close-settings")
                            .debug_selector(|| "close-settings".into())
                            .ghost()
                            .label(strings.get(Text::CloseSettings))
                            .on_click(move |_, _, cx| {
                                let owner = close.clone();
                                cx.defer(move |cx| {
                                    if let Ok(Err(error)) =
                                        owner.update(cx, Presence::dismiss_settings)
                                    {
                                        tracing::error!(%error, "can't dismiss settings");
                                    }
                                });
                            }),
                    ),
            )
            .child(div().px_4().child(self.tabs(cx)))
            .child(if self.tab == SettingsTab::Voice {
                self.voice(window, cx).into_any_element()
            } else {
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .p_4()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .children(
                                (self.tab == SettingsTab::General)
                                    .then(|| self.general(window, cx)),
                            )
                            .children(
                                (self.tab == SettingsTab::Accounts).then(|| self.accounts(cx)),
                            )
                            .children((self.tab == SettingsTab::Accounts).then(|| self.claude(cx)))
                            .children(
                                (self.tab == SettingsTab::Accounts && SHOW_ANTHROPIC_KEY_ROW)
                                    .then(|| self.anthropic(cx)),
                            )
                            .children((self.tab == SettingsTab::Accounts).then(|| self.github(cx)))
                            .children((self.tab == SettingsTab::Privacy).then(|| self.privacy(cx))),
                    )
                    .into_any_element()
            })
            .children(self.stop.then(|| {
                div()
                    .p_4()
                    .child(Button::new("stop").label(strings.get(Text::Stop)).on_click(
                        move |_, _, cx| {
                            super::send_escape(&stop_events, Some(&stop_owner), cx);
                        },
                    ))
            }))
    }
}

/// A view that shows the account sections: a focused button its view replaced took the view's
/// key context, and Esc, with it. Focus goes where GPUI would restore it, or else to the view.
pub(in crate::presence) fn restore_lost_focus(
    focus: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .focus_lost_restore_target(cx)
        .unwrap_or_else(|| focus.clone())
        .focus(window, cx);
}
