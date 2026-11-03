//! The two virtualized lists in the threads surface.

use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputEvent, InputState};
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath, Sizable as _};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ListAlignment, ListState as MessageList, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Task, WeakEntity, Window, div, list, px,
};
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, EventId, NameError, PageEventKind, SettingsOp, ThreadId,
    ThreadKind, ThreadOp, ThreadRow, ThreadTitle,
};

use super::{Presence, PresenceEvent, send};
use crate::runs::Target;
use crate::strings::{Strings, Text};
use crate::threads::{Threads, row_label};

/// Search settles for this long before reading a page [guess].
const SEARCH_DELAY: Duration = Duration::from_millis(150);

gpui_kit::actions!(threads_panel, [EarlierMessages, LaterMessages]);

pub(super) struct ThreadsDelegate {
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    selected: Option<ThreadId>,
    _watch: Subscription,
}

impl ThreadsDelegate {
    pub(super) fn state(
        model: Entity<Threads>,
        owner: WeakEntity<Presence>,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> ListState<Self> {
        let watch = cx.observe_in(&model, window, |list, _, window, cx| {
            let delegate = list.delegate_mut();
            let state = delegate.model.read(cx);
            delegate.selected = state.open;
            let ix = state.selected_index().map(IndexPath::new);
            list.set_selected_index(ix, window, cx);
            cx.notify();
        });
        ListState::new(
            Self {
                model,
                owner,
                selected: None,
                _watch: watch,
            },
            window,
            cx,
        )
        .searchable(true)
    }
}

impl ListDelegate for ThreadsDelegate {
    type Item = ListItem;

    fn items_count(&self, _: usize, cx: &App) -> usize {
        self.model.read(cx).rows.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let state = self.model.read(cx);
        let row = state.rows.get(ix.row)?;
        let deleting = state.deleting.contains(&row.id);
        let label = deleting.then(|| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(cx.global::<Strings>().get(Text::ThreadDeleting))
        });
        let title = row_label(row, state.settings.as_ref());
        Some(
            ListItem::new(("thread", row.id.0))
                .accessibility_label(title.clone())
                .disabled(deleting)
                .child(div().min_w_0().truncate().child(title))
                .children(label),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix
            .and_then(|ix| self.model.read(cx).rows.get(ix.row))
            .map(|row| row.id);
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<'_, ListState<Self>>) {
        if let Some(thread) = self.selected {
            let owner = self.owner.clone();
            cx.defer(move |cx| {
                let _gone = owner.update(cx, |owner, cx| owner.select_thread(Some(thread), cx));
            });
        }
    }

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Task<()> {
        let query = query.to_owned();
        let revision = self.model.update(cx, |state, cx| {
            let revision = state.defer_search(&query);
            cx.notify();
            revision
        });
        let owner = self.owner.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let _gone = owner.update(cx, |owner, cx| {
                owner.threads.update(cx, |state, cx| {
                    state.finish_search(revision);
                    cx.notify();
                });
                owner.request_pages(cx);
            });
        })
    }

    fn loading(&self, cx: &App) -> bool {
        let state = self.model.read(cx);
        !state.listed && state.listing()
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        let state = self.model.read(cx);
        let text = if state.search_error.is_some() {
            Text::ThreadSearchTooLong
        } else if state.failed {
            Text::ThreadsFailed
        } else if state.searching() {
            Text::EmptyThreadSearch
        } else {
            Text::EmptyThreads
        };
        div()
            .p_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(cx.global::<Strings>().get(text))
    }

    fn has_more(&self, cx: &App) -> bool {
        let state = self.model.read(cx);
        state.next.is_some() && !state.listing()
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<'_, ListState<Self>>) {
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            let _gone = owner.update(cx, |owner, cx| {
                owner.threads.update(cx, |state, cx| {
                    state.more_threads();
                    cx.notify();
                });
                owner.request_pages(cx);
            });
        });
    }
}

pub(super) struct ThreadsView {
    pub(super) list: Entity<ListState<ThreadsDelegate>>,
    pub(super) stop: bool,
    pub(super) target: Target,
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    messages: MessageList,
    focus: FocusHandle,
    shown_thread: Option<ThreadId>,
    newest: Option<EventId>,
    count: usize,
    rem: Pixels,
    rename: Option<(ThreadId, ThreadKind)>,
    rename_error: Option<Text>,
    rename_input: Entity<InputState>,
    _rename_events: Subscription,
    _watch: Subscription,
}

impl ThreadsView {
    pub(super) fn new(
        model: Entity<Threads>,
        owner: WeakEntity<Presence>,
        events: async_channel::Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("escape", EscapeKey, Some("ThreadsPanel")),
            KeyBinding::new("pageup", EarlierMessages, Some("ThreadMessages")),
            KeyBinding::new("pagedown", LaterMessages, Some("ThreadMessages")),
        ]);
        let list = cx.new(|cx| ThreadsDelegate::state(model.clone(), owner.clone(), window, cx));
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let rename_events =
            cx.subscribe_in(
                &rename_input,
                window,
                |view, _, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => view.save_rename(window, cx),
                    InputEvent::Change => {
                        view.rename_error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            );
        let messages = MessageList::new(0, ListAlignment::Bottom, px(0.0));
        messages.set_scroll_handler({
            let owner = owner.clone();
            move |event, _, cx| {
                if event.visible_range.start == 0 {
                    older(&owner, cx);
                }
            }
        });
        let watch = cx.observe(&model, |view, _, cx| {
            let state = view.model.read(cx);
            if view
                .rename
                .is_some_and(|(thread, _)| state.open != Some(thread))
            {
                view.rename = None;
                view.rename_error = None;
            }
            let count = state.events.len();
            let newest = state.events.first().map(|event| event.id);
            if view.shown_thread == state.open && view.newest == newest && count >= view.count {
                view.messages.splice(0..0, count - view.count);
            } else {
                view.messages.reset(count);
            }
            view.shown_thread = state.open;
            view.newest = newest;
            view.count = count;
            cx.notify();
        });
        Self {
            list,
            stop: false,
            target: Target::Kumo,
            model,
            owner,
            events,
            messages,
            focus: cx.focus_handle().tab_stop(true),
            shown_thread: None,
            newest: None,
            count: 0,
            rem: window.rem_size(),
            rename: None,
            rename_error: None,
            rename_input,
            _rename_events: rename_events,
            _watch: watch,
        }
    }
}

fn older(owner: &WeakEntity<Presence>, cx: &mut App) {
    let owner = owner.clone();
    cx.defer(move |cx| {
        let _gone = owner.update(cx, |owner, cx| {
            owner.threads.update(cx, |state, cx| {
                state.older_events();
                cx.notify();
            });
            owner.request_pages(cx);
        });
    });
}

impl ThreadsView {
    pub(super) fn editing(&self) -> bool {
        self.rename.is_some()
    }

    pub(super) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.rename = None;
        self.rename_error = None;
        self.list.update(cx, |list, cx| list.focus(window, cx));
        cx.notify();
    }

    fn save_rename(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((thread, kind)) = self.rename else {
            return;
        };
        if !self.model.read(cx).can_rename(thread) {
            return;
        }
        let message = match rename_message(thread, kind, &self.rename_input.read(cx).value()) {
            Ok(message) => message,
            Err(error) => {
                self.rename_error = Some(error);
                cx.notify();
                return;
            }
        };
        let sent = self
            .owner
            .update(cx, |owner, cx| {
                let sent = match message {
                    ControlToBrain::Settings(SettingsOp::SetAssistantName(name)) => {
                        owner.rename_assistant(name, cx)
                    }
                    message => owner
                        .session
                        .as_ref()
                        .is_some_and(|session| owner.link.send_control(session, message)),
                };
                if sent && kind == ThreadKind::Thread {
                    owner.threads.update(cx, |state, cx| {
                        state.renamed(thread);
                        cx.notify();
                    });
                    owner.request_pages(cx);
                }
                sent
            })
            .unwrap_or(false);
        if sent {
            self.cancel_rename(window, cx);
        } else {
            self.rename_error = Some(Text::TurnOffline);
            cx.notify();
        }
    }

    fn toolbar(&self, cx: &App) -> gpui_kit::AnyElement {
        let new_owner = self.owner.clone();
        let close_events = self.events.clone();
        let strings = cx.global::<Strings>();
        let back_owner = self.owner.clone();
        let back = self.target != Target::Kumo;
        let back_label = strings.back_to_kumo(self.model.read(cx).settings.as_ref().map_or(
            kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
            |settings| settings.assistant_name.as_str(),
        ));
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(strings.get(Text::Threads)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(back.then(|| {
                        Button::new("back-to-kumo")
                            .small()
                            .ghost()
                            .label(back_label)
                            .on_click(move |_, _, cx| {
                                let _gone = back_owner.update(cx, Presence::back_to_kumo);
                            })
                    }))
                    .child(
                        Button::new("new-thread")
                            .small()
                            .label(strings.get(Text::NewThread))
                            .on_click(move |_, _, cx| {
                                let owner = new_owner.clone();
                                cx.defer(move |cx| {
                                    if let Ok(Err(error)) = owner.update(cx, Presence::new_thread) {
                                        tracing::error!(%error, "can't open a new thread");
                                    }
                                });
                            }),
                    )
                    .child(
                        Button::new("close-threads")
                            .small()
                            .ghost()
                            .label(strings.get(Text::CloseThreads))
                            .on_click(move |_, _, _| {
                                send(&close_events, PresenceEvent::DismissPanel);
                            }),
                    ),
            )
            .into_any_element()
    }

    fn message_region(&self, cx: &mut Context<'_, Self>) -> gpui_kit::AnyElement {
        let model = self.model.clone();
        let messages = list(self.messages.clone(), move |ix, _, cx| {
            let state = model.read(cx);
            let Some(event) = state.events.iter().rev().nth(ix) else {
                return div().into_any_element();
            };
            let strings = cx.global::<Strings>();
            let label = match event.kind {
                PageEventKind::User => strings.get(Text::ThreadUser),
                PageEventKind::AssistantText => strings.assistant(state.settings.as_ref().map_or(
                    kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
                    |settings| settings.assistant_name.as_str(),
                )),
                PageEventKind::SceneStep => strings.get(Text::ThreadStep),
            };
            div()
                .id(("event", event.id.0))
                .w_full()
                .p_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(div().text_sm().child(event.text.clone()))
                .children(event.cut.then(|| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(cx.global::<Strings>().get(Text::ThreadCut))
                }))
                .into_any_element()
        })
        .size_full();
        div()
            .id("thread-messages")
            .relative()
            .flex_1()
            .min_h_0()
            .track_focus(&self.focus)
            .key_context("ThreadMessages")
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    view.focus.focus(window, cx);
                }),
            )
            .on_action(cx.listener(|view, _: &EarlierMessages, _, cx| {
                view.messages
                    .scroll_by(-view.messages.viewport_bounds().size.height * 0.9);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &LaterMessages, _, cx| {
                view.messages
                    .scroll_by(view.messages.viewport_bounds().size.height * 0.9);
                cx.notify();
            }))
            .child(messages)
            .child(Scrollbar::vertical(&self.messages))
            .into_any_element()
    }

    /// The open thread's Delete, which asks first, or why it can't be deleted now.
    fn delete_bar(&self, cx: &App) -> Option<gpui_kit::AnyElement> {
        let state = self.model.read(cx);
        let thread = state.open?;
        if state.row(thread)?.kind == ThreadKind::Main {
            return None;
        }
        let strings = cx.global::<Strings>();
        let note = if state.deleting.contains(&thread) {
            Some(Text::ThreadDeleting)
        } else if state.deletes_full {
            Some(Text::ThreadDeletesFull)
        } else {
            None
        };
        let owner = self.owner.clone();
        let button = Button::new("delete-thread")
            .small()
            .danger()
            .disabled(note.is_some())
            .label(strings.get(Text::DeleteThread))
            .on_click(move |_, window, cx| {
                let owner = owner.clone();
                window.open_alert_dialog(cx, move |alert, _, cx| {
                    let strings = cx.global::<Strings>();
                    let owner = owner.clone();
                    alert
                        .title(strings.get(Text::DeleteThreadTitle))
                        .description(strings.get(Text::DeleteThreadBody))
                        .show_cancel(true)
                        .ok_text(strings.get(Text::DeleteThread))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(strings.get(Text::DeleteThreadCancel))
                        .on_ok(move |_, _, cx| {
                            let owner = owner.clone();
                            cx.defer(move |cx| {
                                let _gone = owner.update(cx, |owner, cx| {
                                    owner.delete_thread(thread, cx);
                                });
                            });
                            true
                        })
                });
            });
        Some(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap_2()
                .p_2()
                .children(note.map(|note| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(strings.get(note))
                }))
                .child(button)
                .into_any_element(),
        )
    }

    fn rename_editor(&self, thread: ThreadId, cx: &mut Context<'_, Self>) -> gpui_kit::AnyElement {
        let state = self.model.read(cx);
        let disabled = !state.can_rename(thread);
        let strings = cx.global::<Strings>();
        let save_label = strings.get(Text::SaveRename);
        let cancel_label = strings.get(Text::CancelRename);
        let error = self.rename_error.map(|error| strings.get(error));
        div()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(Input::new(&self.rename_input))
            .children(error.map(|error| div().text_xs().text_color(cx.theme().danger).child(error)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("save-rename")
                            .small()
                            .disabled(disabled)
                            .label(save_label)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.save_rename(window, cx);
                            })),
                    )
                    .child(
                        Button::new("cancel-rename")
                            .small()
                            .ghost()
                            .label(cancel_label)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.cancel_rename(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn detail_header(&self, cx: &mut Context<'_, Self>) -> Option<gpui_kit::AnyElement> {
        let state = self.model.read(cx);
        let row = state.row(state.open?)?.clone();
        let title = row_label(&row, state.settings.as_ref());
        let disabled = !state.can_rename(row.id);
        if self.rename.is_some() {
            return Some(self.rename_editor(row.id, cx));
        }
        let strings = cx.global::<Strings>();
        let rename_label = strings.get(Text::RenameThread);
        let switch_label = strings.get(Text::SwitchThread);
        let switch = !row_is_target(&row, self.target);
        let switch_owner = self.owner.clone();
        let thread = row.id;
        let kind = row.kind;
        let edit_title = title.clone();
        Some(
            div()
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .children(switch.then(|| {
                            Button::new("switch-task")
                                .small()
                                .disabled(disabled)
                                .label(switch_label)
                                .on_click(move |_, _, cx| {
                                    let _gone = switch_owner
                                        .update(cx, |owner, cx| owner.switch_thread(&row, cx));
                                })
                        }))
                        .child(
                            Button::new("rename-task")
                                .small()
                                .ghost()
                                .disabled(disabled)
                                .label(rename_label)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.rename = Some((thread, kind));
                                    view.rename_error = None;
                                    view.rename_input.update(cx, |input, cx| {
                                        input.set_value(edit_title.clone(), window, cx);
                                        input.focus(window, cx);
                                    });
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn detail(&self, cx: &mut Context<'_, Self>) -> gpui_kit::AnyElement {
        let header = self.detail_header(cx);
        let delete_bar = self.delete_bar(cx);
        let state = self.model.read(cx);
        let empty = if state.open.is_none() {
            Some(Text::SelectThread)
        } else if state.loaded && state.events.is_empty() {
            Some(Text::ThreadGone)
        } else {
            None
        }
        .map(|text| cx.global::<Strings>().get(text));
        let more = state.before.is_some();
        let waiting = state.open.is_some() && state.reading();
        let older_label = cx.global::<Strings>().get(Text::OlderMessages);
        let owner = self.owner.clone();
        let messages = self.message_region(cx);
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .flex()
            .flex_col()
            .children(header)
            .children(delete_bar)
            .children(more.then(|| {
                Button::new("older-messages")
                    .small()
                    .ghost()
                    .disabled(waiting)
                    .label(older_label)
                    .on_click(move |_, _, cx| older(&owner, cx))
            }))
            .children(waiting.then(Spinner::new))
            .children(empty.map(|text| {
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(text)
            }))
            .child(messages)
            .into_any_element()
    }
}

impl Render for ThreadsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.rem != window.rem_size() {
            self.rem = window.rem_size();
            self.messages.splice(0..self.count, self.count);
        }
        let escape_events = self.events.clone();
        let cancel_events = self.events.clone();
        let stop_events = self.events.clone();
        let state = self.model.read(cx);
        let notice = if !state.online() {
            Some(Text::ThreadsOffline)
        } else if state.failed {
            Some(Text::ThreadsFailed)
        } else if state.delete_failed {
            Some(Text::ThreadDeleteFailed)
        } else {
            None
        }
        .map(|text| cx.global::<Strings>().get(text));
        let error = self
            .owner
            .upgrade()
            .and_then(|owner| owner.read(cx).error.clone());
        let stop_label = cx.global::<Strings>().get(Text::Stop);
        let search = cx.global::<Strings>().get(Text::SearchThreads);
        let toolbar = self.toolbar(cx);
        let detail = self.detail(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .p_3()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .key_context("ThreadsPanel")
            .on_action(move |_: &EscapeKey, _, _| send(&escape_events, PresenceEvent::Escape))
            .on_action(move |_: &gpui_kit::base::actions::Cancel, _, _| {
                send(&cancel_events, PresenceEvent::Escape);
            })
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_stretch()
                    .child(
                        div()
                            .w_64()
                            .min_w_0()
                            .h_full()
                            .child(List::new(&self.list).small().search_placeholder(search)),
                    )
                    .child(detail),
            )
            .children(notice.map(|text| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(text)
            }))
            .children(error.map(|error| div().text_sm().text_color(cx.theme().danger).child(error)))
            .children(self.stop.then(|| {
                Button::new("stop")
                    .small()
                    .label(stop_label)
                    .on_click(move |_, _, _| send(&stop_events, PresenceEvent::Escape))
            }))
    }
}

fn row_is_target(row: &ThreadRow, target: Target) -> bool {
    match row.kind {
        ThreadKind::Main => target == Target::Kumo,
        ThreadKind::Thread => target == Target::Task(row.id),
    }
}

fn rename_message(thread: ThreadId, kind: ThreadKind, text: &str) -> Result<ControlToBrain, Text> {
    let result = match kind {
        ThreadKind::Main => AssistantName::new(text)
            .map(|name| ControlToBrain::Settings(SettingsOp::SetAssistantName(name))),
        ThreadKind::Thread => ThreadTitle::new(text)
            .map(|title| ControlToBrain::ThreadOp(ThreadOp::Rename { thread, title })),
    };
    result.map_err(|error| match error {
        NameError::Empty => Text::RenameEmpty,
        NameError::TooLong if kind == ThreadKind::Main => Text::RenameNameTooLong,
        NameError::TooLong => Text::RenameTooLong,
        NameError::NotOneLine => Text::RenameOneLine,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_uses_the_kind_and_validates_before_sending() {
        assert_eq!(
            rename_message(ThreadId(4), ThreadKind::Main, " Cloud "),
            Ok(ControlToBrain::Settings(SettingsOp::SetAssistantName(
                AssistantName::new("Cloud").expect("valid name")
            )))
        );
        assert_eq!(
            rename_message(ThreadId(5), ThreadKind::Thread, " Birthday "),
            Ok(ControlToBrain::ThreadOp(ThreadOp::Rename {
                thread: ThreadId(5),
                title: ThreadTitle::new("Birthday").expect("valid title")
            }))
        );
        assert_eq!(
            rename_message(ThreadId(4), ThreadKind::Main, ""),
            Err(Text::RenameEmpty)
        );
        assert_eq!(
            rename_message(ThreadId(4), ThreadKind::Main, &"a".repeat(25)),
            Err(Text::RenameNameTooLong)
        );
        assert_eq!(
            rename_message(ThreadId(5), ThreadKind::Thread, &"a".repeat(81)),
            Err(Text::RenameTooLong)
        );
        assert_eq!(
            rename_message(ThreadId(5), ThreadKind::Thread, "two\nlines"),
            Err(Text::RenameOneLine)
        );
    }

    #[test]
    fn assistant_names_and_task_titles_have_different_character_limits() {
        let name = "🌥".repeat(24);
        assert_eq!(
            rename_message(ThreadId(4), ThreadKind::Main, &name),
            Ok(ControlToBrain::Settings(SettingsOp::SetAssistantName(
                AssistantName::new(&name).expect("24 characters")
            ))),
        );
        let title = "🌥".repeat(80);
        assert_eq!(
            rename_message(ThreadId(5), ThreadKind::Thread, &title),
            Ok(ControlToBrain::ThreadOp(ThreadOp::Rename {
                thread: ThreadId(5),
                title: ThreadTitle::new(&title).expect("80 characters")
            })),
        );
        let title = "a".repeat(25);
        assert_eq!(
            rename_message(ThreadId(5), ThreadKind::Thread, &title),
            Ok(ControlToBrain::ThreadOp(ThreadOp::Rename {
                thread: ThreadId(5),
                title: ThreadTitle::new(&title).expect("25 characters")
            })),
        );
        assert_eq!(
            rename_message(ThreadId(4), ThreadKind::Main, "Cloud\u{7}"),
            Err(Text::RenameOneLine)
        );
    }

    #[test]
    fn switch_is_hidden_only_for_the_actual_target() {
        let main = ThreadRow {
            id: ThreadId(4),
            title: String::new(),
            kind: ThreadKind::Main,
            updated_ms: 0,
            choice: None,
        };
        let task = ThreadRow {
            id: ThreadId(5),
            title: "Birthday".into(),
            kind: ThreadKind::Thread,
            updated_ms: 0,
            choice: None,
        };
        assert!(row_is_target(&main, Target::Kumo));
        assert!(!row_is_target(&main, Target::Task(ThreadId(5))));
        assert!(row_is_target(&task, Target::Task(ThreadId(5))));
        assert!(!row_is_target(&task, Target::Task(ThreadId(6))));
        assert!(!row_is_target(&task, Target::New));
    }
}
