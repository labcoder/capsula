//! The retained, virtualized conversation shared by the task and chat surfaces.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding,
    ListAlignment, ListState as MessageList, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, WeakEntity, Window, div, list, px,
};
use kumochi_protocol::message::{EventId, PageEventKind, ThreadId};

use super::Presence;
use crate::strings::{Strings, Text};
use crate::threads::Threads;

gpui_kit::actions!(thread_messages, [EarlierMessages, LaterMessages]);

pub(super) struct Messages {
    model: Entity<Threads>,
    list: MessageList,
    focus: FocusHandle,
    shown_thread: Option<ThreadId>,
    newest: Option<EventId>,
    count: usize,
    rem: Pixels,
    _watch: Subscription,
}

impl Messages {
    pub(super) fn new(
        model: Entity<Threads>,
        owner: WeakEntity<Presence>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("pageup", EarlierMessages, Some("ThreadMessages")),
            KeyBinding::new("pagedown", LaterMessages, Some("ThreadMessages")),
        ]);
        let state = model.read(cx);
        let count = state.events.len();
        let shown_thread = state.open;
        let newest = state.events.first().map(|event| event.id);
        let messages = MessageList::new(count, ListAlignment::Bottom, px(0.0));
        messages.set_scroll_handler({
            let model = model.clone();
            move |event, _, cx| {
                if event.visible_range.start == 0 {
                    older(&model, &owner, cx);
                }
            }
        });
        let watch = cx.observe(&model, |view, _, cx| {
            let state = view.model.read(cx);
            let count = state.events.len();
            let newest = state.events.first().map(|event| event.id);
            if view.shown_thread == state.open && view.newest == newest && count >= view.count {
                view.list.splice(0..0, count - view.count);
            } else {
                view.list.reset(count);
            }
            view.shown_thread = state.open;
            view.newest = newest;
            view.count = count;
            cx.notify();
        });
        Self {
            model,
            list: messages,
            focus: cx.focus_handle().tab_stop(true),
            shown_thread,
            newest,
            count,
            rem: window.rem_size(),
            _watch: watch,
        }
    }
}

pub(super) fn older(model: &Entity<Threads>, owner: &WeakEntity<Presence>, cx: &mut App) {
    let model = model.clone();
    let owner = owner.clone();
    cx.defer(move |cx| {
        let _gone = owner.update(cx, |owner, cx| {
            model.update(cx, |state, cx| {
                state.older_events();
                cx.notify();
            });
            owner.request_pages(cx);
        });
    });
}

impl Render for Messages {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.rem != window.rem_size() {
            self.rem = window.rem_size();
            self.list.splice(0..self.count, self.count);
        }
        let model = self.model.clone();
        let messages = list(self.list.clone(), move |ix, _, cx| {
            let state = model.read(cx);
            let Some(event) = state.events.iter().rev().nth(ix) else {
                return div().into_any_element();
            };
            let strings = cx.global::<Strings>();
            let (label, text) = match event.kind {
                PageEventKind::User => (Some(strings.get(Text::ThreadUser)), event.text.clone()),
                PageEventKind::AssistantText => (
                    Some(strings.assistant(state.settings.as_ref().map_or(
                        kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
                        |settings| settings.assistant_name.as_str(),
                    ))),
                    event.text.clone(),
                ),
                PageEventKind::SceneStep => {
                    (Some(strings.get(Text::ThreadStep)), event.text.clone())
                }
                PageEventKind::Issue { .. } => (None, strings.issue_draft(&event.text)),
                PageEventKind::TaskDeleted => (None, strings.get(Text::ThreadDeleted)),
            };
            div()
                .id(("event", event.id.0))
                .w_full()
                .p_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .children(label.map(|label| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label)
                }))
                .child(div().text_sm().child(text))
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
                view.list
                    .scroll_by(-view.list.viewport_bounds().size.height * 0.9);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &LaterMessages, _, cx| {
                view.list
                    .scroll_by(view.list.viewport_bounds().size.height * 0.9);
                cx.notify();
            }))
            .child(messages)
            .child(Scrollbar::vertical(&self.list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{
        AppContext as _, Bounds, ListOffset, TestAppContext, WindowBounds, WindowOptions, point,
        size,
    };
    use kumochi_protocol::message::{PageEvent, RunId};

    fn event(id: u64) -> PageEvent {
        PageEvent {
            id: EventId(id),
            run: Some(RunId(1)),
            kind: PageEventKind::AssistantText,
            text: format!("message {id}"),
            cut: false,
        }
    }

    struct Frame(Entity<Messages>);

    impl Render for Frame {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.0.clone())
        }
    }

    #[gpui::test]
    fn older_messages_preserve_the_anchor_and_selection_resets_the_list(cx: &mut TestAppContext) {
        let (model, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| {
                let mut state = Threads::default();
                state.open = Some(ThreadId(1));
                state.events = (3..=12).rev().map(event).collect();
                state
            });
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(300.0), px(200.0)),
                ))),
                ..WindowOptions::default()
            };
            let mut view = None;
            let _window = gpui_kit::open_window(options, cx, |window, cx| {
                let messages = cx
                    .new(|cx| Messages::new(model.clone(), WeakEntity::new_invalid(), window, cx));
                view = Some(messages.clone());
                cx.new(|_| Frame(messages))
            })
            .expect("a window");
            (model, view.expect("messages created with the window"))
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.list.item_count(), 10);
            view.list.scroll_to(ListOffset {
                item_ix: 5,
                offset_in_item: px(0.0),
            });
        });
        model.update(cx, |state, cx| {
            state.events.extend([event(2), event(1)]);
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.list.item_count(), 12);
            assert_eq!(view.list.logical_scroll_top().item_ix, 7);
            assert_eq!(view.shown_thread, Some(ThreadId(1)));
        });
        model.update(cx, |state, cx| {
            state.select(Some(ThreadId(2)));
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.list.item_count(), 0);
            assert_eq!(view.shown_thread, Some(ThreadId(2)));
            assert_eq!(view.newest, None);
        });
    }
}
