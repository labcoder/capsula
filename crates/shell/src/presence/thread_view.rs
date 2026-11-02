//! The two virtualized lists in the threads surface.

use std::time::Duration;

use capsula_protocol::message::{EventId, PageEventKind, ThreadId};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Escape as EscapeKey;
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath, Sizable as _};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ListAlignment, ListState as MessageList, ParentElement as _, Pixels, Render,
    Styled as _, Subscription, Task, WeakEntity, Window, div, list, px,
};

use super::{Presence, PresenceEvent, send};
use crate::strings::{Strings, Text};
use crate::threads::Threads;

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
            let delegate = list.delegate();
            let ix = delegate
                .selected
                .and_then(|id| {
                    delegate
                        .model
                        .read(cx)
                        .rows
                        .iter()
                        .position(|row| row.id == id)
                })
                .map(IndexPath::new);
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
        let row = self.model.read(cx).rows.get(ix.row)?;
        Some(
            ListItem::new(("thread", row.id.0))
                .accessibility_label(row.title.clone())
                .child(div().min_w_0().truncate().child(row.title.clone())),
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
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    events: async_channel::Sender<PresenceEvent>,
    messages: MessageList,
    focus: FocusHandle,
    shown_thread: Option<ThreadId>,
    newest: Option<EventId>,
    count: usize,
    rem: Pixels,
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
            model,
            owner,
            events,
            messages,
            focus: cx.focus_handle().tab_stop(true),
            shown_thread: None,
            newest: None,
            count: 0,
            rem: window.rem_size(),
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
    fn toolbar(&self, cx: &App) -> gpui_kit::AnyElement {
        let new_owner = self.owner.clone();
        let close_events = self.events.clone();
        let strings = cx.global::<Strings>();
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
            let kind = match event.kind {
                PageEventKind::User => Text::ThreadUser,
                PageEventKind::AssistantText => Text::ThreadAssistant,
                PageEventKind::SceneStep => Text::ThreadStep,
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
                        .child(cx.global::<Strings>().get(kind)),
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

    fn detail(&self, cx: &mut Context<'_, Self>) -> gpui_kit::AnyElement {
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
        } else {
            None
        }
        .map(|text| cx.global::<Strings>().get(text));
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
            .children(self.stop.then(|| {
                Button::new("stop")
                    .small()
                    .label(stop_label)
                    .on_click(move |_, _, _| send(&stop_events, PresenceEvent::Escape))
            }))
    }
}
