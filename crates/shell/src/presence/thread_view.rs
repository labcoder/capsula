//! The two virtualized lists in the threads surface.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape as EscapeKey, Input, InputEvent, InputState};
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath, Sizable as _};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window,
    div,
};
use kumochi_protocol::message::{
    ControlToBrain, SettingsOp, ThreadId, ThreadKind, ThreadOp, ThreadRow,
};

use super::messages::Messages;
use super::thread_selection::Selection;
use super::{Presence, PresenceEvent, send};
use crate::runs::Target;
use crate::strings::{Strings, Text};
use crate::threads::{Rename, Threads, row_label};

/// Search settles for this long before reading a page [guess].
const SEARCH_DELAY: Duration = Duration::from_millis(150);

pub(super) struct ThreadsDelegate {
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    selection: Selection,
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
            let ix = delegate.selection.observe(state).map(IndexPath::new);
            list.set_selected_index(ix, window, cx);
            cx.notify();
        });
        ListState::new(
            Self {
                model,
                owner,
                selection: Selection::default(),
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
        self.selection
            .select(ix.map(|ix| ix.row), self.model.read(cx));
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<'_, ListState<Self>>) {
        if let Some(thread) = self.selection.selected {
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
    /// Pushed by presence, never read from it: the first frame renders while presence is updating.
    pub(super) error: Option<SharedString>,
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    messages: Entity<Messages>,
    pub(super) rename: super::escape::RenameEdit,
    rename_error: Option<Text>,
    rename_input: Entity<InputState>,
    _rename_events: Subscription,
    _watch: Subscription,
}

impl ThreadsView {
    pub(super) fn new(
        model: Entity<Threads>,
        owner: WeakEntity<Presence>,
        error: Option<SharedString>,
        events: async_channel::Sender<PresenceEvent>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([KeyBinding::new("escape", EscapeKey, Some("ThreadsPanel"))]);
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
        let messages = cx.new(|cx| Messages::new(model.clone(), owner.clone(), window, cx));
        let watch = cx.observe(&model, |view, _, cx| {
            let state = view.model.read(cx);
            if view
                .rename
                .get()
                .is_some_and(|thread| state.open != Some(thread))
            {
                view.rename.set(None);
                view.rename_error = None;
            }
            cx.notify();
        });
        Self {
            list,
            stop: false,
            target: Target::Kumo,
            error,
            model,
            owner,
            events,
            messages,
            rename: Rc::default(),
            rename_error: None,
            rename_input,
            _rename_events: rename_events,
            _watch: watch,
        }
    }
}

impl ThreadsView {
    pub(super) fn editing(&self) -> bool {
        self.rename.get().is_some()
    }

    pub(super) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.rename.set(None);
        self.rename_error = None;
        self.list.update(cx, |list, cx| list.focus(window, cx));
        cx.notify();
    }

    fn save_rename(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(thread) = self.rename.get() else {
            return;
        };
        let message = match self
            .model
            .read(cx)
            .prepare_rename(thread, &self.rename_input.read(cx).value())
        {
            Ok(Rename::Send(message)) => message,
            Ok(Rename::Blocked) => return,
            Ok(Rename::Close) => {
                self.cancel_rename(window, cx);
                return;
            }
            Err(error) => {
                self.rename_error = Some(error);
                cx.notify();
                return;
            }
        };
        let renamed = match &message {
            ControlToBrain::ThreadOp(ThreadOp::Rename { title, .. }) => {
                Some(title.as_str().to_owned())
            }
            _ => None,
        };
        let sent = self
            .owner
            .update(cx, |owner, cx| {
                let sent = match message {
                    ControlToBrain::Settings(SettingsOp::SetAssistantName(name)) => {
                        owner.rename_assistant(name, cx)
                    }
                    message => match owner.session.as_ref() {
                        Some(session) => owner
                            .link
                            .send_control(session, message)
                            .then_some(())
                            .ok_or(Text::SettingsUnavailable),
                        None => Err(Text::TurnOffline),
                    },
                };
                if sent.is_ok()
                    && let Some(title) = renamed.as_deref()
                {
                    for model in [&owner.threads, &owner.chat] {
                        model.update(cx, |state, cx| {
                            state.renamed(thread, title);
                            cx.notify();
                        });
                    }
                    owner.request_pages(cx);
                }
                sent
            })
            .unwrap_or(Err(Text::TurnOffline));
        match sent {
            Ok(()) => self.cancel_rename(window, cx),
            Err(error) => {
                self.rename_error = Some(error);
                cx.notify();
            }
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
        let assistant = state.settings.as_ref().map_or(
            kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME.to_owned(),
            |settings| settings.assistant_name.as_str().to_owned(),
        );
        let button = Button::new("delete-thread")
            .small()
            .danger()
            .disabled(note.is_some())
            .label(strings.get(Text::DeleteThread))
            .on_click(move |_, window, cx| {
                let owner = owner.clone();
                let strings = cx.global::<Strings>();
                let title = strings.get(Text::DeleteThreadTitle).into();
                let body = strings.delete_body(&assistant).into();
                let cancel = strings.get(Text::DeleteThreadCancel).into();
                let confirm = strings.get(Text::DeleteThread).into();
                let _focus = crate::confirmation::open(
                    window,
                    cx,
                    title,
                    body,
                    cancel,
                    confirm,
                    move |_, cx| {
                        cx.defer(move |cx| {
                            let _gone = owner.update(cx, |owner, cx| {
                                owner.delete_thread(thread, cx);
                            });
                        });
                    },
                );
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
        if self.editing() {
            return Some(self.rename_editor(row.id, cx));
        }
        let strings = cx.global::<Strings>();
        let rename_label = strings.get(Text::RenameThread);
        let switch_label = strings.get(Text::SwitchThread);
        let switch = !row_is_target(&row, self.target);
        let switch_owner = self.owner.clone();
        let thread = row.id;
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
                                .debug_selector(|| "rename-task".into())
                                .small()
                                .ghost()
                                .disabled(disabled)
                                .label(rename_label)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.rename.set(Some(thread));
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
        let older_messages = self.messages.clone();
        let messages = self.messages.clone();
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
                    .on_click(move |_, _, cx| {
                        older_messages.update(cx, Messages::older);
                    })
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let escape_events = self.events.clone();
        let cancel_events = self.events.clone();
        let stop_events = self.events.clone();
        let escape_owner = self.owner.clone();
        let cancel_owner = self.owner.clone();
        let stop_owner = self.owner.clone();
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
        let error = self.error.clone();
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
            .on_action(move |_: &EscapeKey, _, cx| {
                super::send_escape(&escape_events, Some(&escape_owner), cx);
            })
            .on_action(move |_: &gpui_kit::base::actions::Cancel, _, cx| {
                super::send_escape(&cancel_events, Some(&cancel_owner), cx);
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
            .children(error.map(|error| {
                div()
                    .debug_selector(|| format!("threads-error {error}"))
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error)
            }))
            .children(self.stop.then(|| {
                Button::new("stop")
                    .small()
                    .label(stop_label)
                    .on_click(move |_, _, cx| {
                        super::send_escape(&stop_events, Some(&stop_owner), cx);
                    })
            }))
    }
}

fn row_is_target(row: &ThreadRow, target: Target) -> bool {
    match row.kind {
        ThreadKind::Main => target == Target::Kumo,
        ThreadKind::Thread => target == Target::Task(row.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AnyWindowHandle, TestAppContext, VisualTestContext, WindowOptions};

    const UNSAVED: &str = "threads-error Couldn't save that setting. Try again.";
    const OFFLINE: &str = "threads-error Kumo can't reach the brain.";

    /// Which of the two errors the window's latest frame drew.
    fn drawn(cx: &mut TestAppContext, window: AnyWindowHandle) -> [bool; 2] {
        cx.run_until_parked();
        let mut frame = VisualTestContext::from_window(window, cx);
        [UNSAVED, OFFLINE].map(|error| frame.debug_bounds(error).is_some())
    }

    #[gpui::test]
    fn the_threads_error_draws_from_its_pushed_copy(cx: &mut TestAppContext) {
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| Threads::default());
            let (events, _) = async_channel::unbounded();
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let error = Some("Couldn't save that setting. Try again.".into());
                cx.new(|cx| {
                    ThreadsView::new(model, WeakEntity::new_invalid(), error, events, window, cx)
                })
            })
            .expect("a window")
        });
        assert_eq!(drawn(cx, window), [true, false]);
        view.update(cx, |view, cx| {
            view.error = Some("Kumo can't reach the brain.".into());
            cx.notify();
        });
        assert_eq!(drawn(cx, window), [false, true]);
        view.update(cx, |view, cx| {
            view.error = None;
            cx.notify();
        });
        assert_eq!(drawn(cx, window), [false, false]);
    }

    #[gpui::test]
    fn a_stale_rename_closes_without_sending_but_the_current_editor_sends(cx: &mut TestAppContext) {
        use async_channel::TryRecvError;
        use kumochi_protocol::message::{Incarnation, ThreadTitle};

        use crate::link::{LinkHandle, Session};
        use crate::presence::{test_fixture, threads::ThreadsPanel};

        test_fixture::without_desktop(cx);
        let owner = test_fixture::presence(cx);
        let first = ThreadId(1);
        let second = ThreadId(2);
        let (window, view, controls, _requests) = owner.update(cx, |owner, cx| {
            let (mut link, requests) = LinkHandle::recording();
            let controls = link.record_controls();
            owner.link = link;
            owner.session = Some(Session::detached(1, Incarnation(1)));
            owner.threads.update(cx, |state, cx| {
                state.connected();
                state.rows = [first, second]
                    .map(|id| ThreadRow {
                        id,
                        title: format!("Task {}", id.0),
                        updated_ms: 0,
                        kind: ThreadKind::Thread,
                        choice: None,
                        task: None,
                    })
                    .to_vec();
                state.show_for(Some(first));
                cx.notify();
            });
            let panel = ThreadsPanel::detached(owner.threads.clone(), cx);
            (panel.window, panel.view, controls, requests)
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("test window");
        let open_rename = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            let mut frame = VisualTestContext::from_window(window, cx);
            let button = frame.debug_bounds("rename-task").expect("Rename task");
            frame.simulate_click(button.center(), gpui_kit::Modifiers::default());
            frame.run_until_parked();
        };
        open_rename(cx);
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.rename_input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    view.save_rename(window, cx);
                    assert_eq!(view.rename_error, Some(Text::RenameEmpty));
                    view.rename_input.update(cx, |input, cx| {
                        input.set_value("Stale edit", window, cx);
                    });
                    owner.update(cx, |owner, cx| owner.select_thread(Some(second), cx));
                    let state = view.model.read(cx);
                    assert_eq!(state.open, Some(second));
                    assert!(state.rows.iter().any(|row| row.id == first));
                    assert_eq!(
                        view.rename.get(),
                        Some(first),
                        "observer hasn't refreshed yet"
                    );
                    view.save_rename(window, cx);
                    assert!(!view.editing(), "the save closes the stale editor");
                    assert_eq!(view.rename_error, None);
                    assert_eq!(view.model.read(cx).open, Some(second));
                    assert_eq!(controls.try_recv(), Err(TryRecvError::Empty));
                });
            })
            .expect("test window");

        open_rename(cx);
        window
            .update(cx, |_, window, cx| {
                let state = view.read(cx);
                assert_eq!(state.rename.get(), Some(second));
                state.rename_input.clone().update(cx, |input, cx| {
                    input.set_value("Current edit", window, cx);
                });
            })
            .expect("test window");
        cx.simulate_keystrokes(window, "enter");
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::ThreadOp(ThreadOp::Rename {
                thread: second,
                title: ThreadTitle::new("Current edit").expect("valid title"),
            }))
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Empty));
        assert!(!view.read_with(cx, |view, _| view.editing()));
        window
            .update(cx, |_, window, _| window.remove_window())
            .expect("close the test window");
        drop(view);
        drop(owner);
        cx.run_until_parked();
    }

    #[test]
    fn switch_is_hidden_only_for_the_actual_target() {
        let main = ThreadRow {
            id: ThreadId(4),
            title: String::new(),
            kind: ThreadKind::Main,
            updated_ms: 0,
            choice: None,
            task: None,
        };
        let task = ThreadRow {
            id: ThreadId(5),
            title: "Birthday".into(),
            kind: ThreadKind::Thread,
            updated_ms: 0,
            choice: None,
            task: None,
        };
        assert!(row_is_target(&main, Target::Kumo));
        assert!(!row_is_target(&main, Target::Task(ThreadId(5))));
        assert!(row_is_target(&task, Target::Task(ThreadId(5))));
        assert!(!row_is_target(&task, Target::Task(ThreadId(6))));
        assert!(!row_is_target(&task, Target::New));
    }
}
