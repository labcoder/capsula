//! The retained, virtualized conversation shared by the task and chat surfaces.

use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::{ActiveTheme as _, ThemeStyled as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding,
    ListAlignment, ListState as MessageList, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, WeakEntity, Window, div, list, px,
};
use kumochi_protocol::message::{EventId, PageEventKind, RunId, ThreadId};

use super::Presence;
use crate::runs::Shown;
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
    live: Option<Shown>,
    live_row: Option<RunId>,
    proven: Option<RunId>,
    _watch: Subscription,
}

impl Messages {
    pub(super) fn is_empty(&self) -> bool {
        self.count == 0
    }
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
        let watch = cx.observe(&model, |view, _, cx| view.sync(cx));
        Self {
            model,
            list: messages,
            focus: cx.focus_handle().tab_stop(true),
            shown_thread,
            newest,
            count,
            rem: window.rem_size(),
            live: None,
            live_row: None,
            proven: None,
            _watch: watch,
        }
    }

    pub(super) fn live(&mut self, answer: Option<&Shown>, cx: &mut Context<'_, Self>) {
        if self.live.as_ref() != answer {
            self.live = answer.cloned();
            self.sync(cx);
        }
    }

    fn sync(&mut self, cx: &mut Context<'_, Self>) {
        let state = self.model.read(cx);
        let live = self
            .live
            .as_ref()
            .filter(|live| {
                live.thread.is_some()
                    && live.thread == state.open
                    && Some(live.run) != state.proven
                    && !live.text.is_empty()
            })
            .map(|live| live.run);
        let count = state.events.len() + usize::from(live.is_some());
        let newest = state.events.first().map(|event| event.id);
        if self.shown_thread == state.open
            && self.newest == newest
            && self.proven == state.proven
            && self.live_row == live
            && count >= self.count
        {
            self.list.splice(0..0, count - self.count);
        } else {
            self.list.reset(count);
        }
        self.shown_thread = state.open;
        self.newest = newest;
        self.count = count;
        self.live_row = live;
        self.proven = state.proven;
        cx.notify();
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
            self.list.remeasure();
        }
        let model = self.model.clone();
        let live = self.live.clone();
        let live_row = self.live_row;
        let messages = list(self.list.clone(), move |ix, _, cx| {
            let state = model.read(cx);
            message(
                ix,
                state,
                live.as_ref().filter(|live| Some(live.run) == live_row),
                cx,
            )
        })
        .size_full();
        div()
            .id("thread-messages")
            .debug_selector(|| "chat-message-region".into())
            .relative()
            .mx_1()
            .my_1()
            .flex_1()
            .min_h_0()
            .track_focus(&self.focus)
            .when(self.focus.is_focused(window), |this| {
                this.focus_ring_style(window, cx)
            })
            .key_context("ThreadMessages")
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    view.focus.focus(window, cx);
                }),
            )
            .on_action(cx.listener(|view, _: &EarlierMessages, _, cx| {
                let height = view.list.viewport_bounds().size.height;
                let bottom = view.list.logical_scroll_top().item_ix == view.list.item_count();
                let end_anchor = if bottom { height } else { px(0.0) };
                view.list.scroll_by(-height * 0.9 - end_anchor);
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

fn message(ix: usize, state: &Threads, live: Option<&Shown>, cx: &App) -> gpui_kit::AnyElement {
    let (id, kind, text, cut) = if ix == state.events.len() {
        let Some(live) = live else {
            return div().into_any_element();
        };
        (
            ("live", live.run.0),
            PageEventKind::AssistantText,
            live.text.clone(),
            live.cut,
        )
    } else {
        let Some(event) = state.events.iter().rev().nth(ix) else {
            return div().into_any_element();
        };
        if event.kind == PageEventKind::AssistantText
            && event.run.is_some()
            && event.run == live.map(|live| live.run)
        {
            return div().into_any_element();
        }
        (
            ("event", event.id.0),
            event.kind,
            event.text.clone(),
            event.cut,
        )
    };
    let strings = cx.global::<Strings>();
    let label = match kind {
        PageEventKind::User => Some(strings.get(Text::ThreadUser)),
        PageEventKind::AssistantText => Some(strings.assistant(state.settings.as_ref().map_or(
            kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
            |settings| settings.assistant_name.as_str(),
        ))),
        PageEventKind::SceneStep => Some(strings.get(Text::ThreadStep)),
        PageEventKind::Issue { .. } => None,
    };
    let text = if label.is_some() {
        text
    } else {
        strings.issue_draft(&text)
    };
    div()
        .id(id)
        .debug_selector(move || format!("message-{}-{}", id.0, id.1))
        .w_full()
        .p_3()
        .px_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .children(label.map(|label| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label)
        }))
        .child(div().text_sm().child(text))
        .children(cut.then(|| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(cx.global::<Strings>().get(if id.0 == "live" {
                    Text::ChatLiveCut
                } else {
                    Text::ThreadCut
                }))
        }))
        .into_any_element()
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

    fn open(
        cx: &mut TestAppContext,
    ) -> (Entity<Threads>, Entity<Messages>, gpui_kit::AnyWindowHandle) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| {
                let mut state = Threads::chat();
                state.open = Some(ThreadId(1));
                state
            });
            let mut view = None;
            let (window, _) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(0.0), px(0.0)),
                        size(px(600.0), px(800.0)),
                    ))),
                    ..WindowOptions::default()
                },
                cx,
                |window, cx| {
                    let messages = cx.new(|cx| {
                        Messages::new(model.clone(), WeakEntity::new_invalid(), window, cx)
                    });
                    view = Some(messages.clone());
                    cx.new(|_| Frame(messages))
                },
            )
            .expect("window");
            (model, view.expect("messages"), window)
        })
    }

    #[gpui::test]
    fn page_keys_scroll_the_focused_message_region(cx: &mut TestAppContext) {
        let (model, view, window) = open(cx);
        model.update(cx, |state, cx| {
            state.events = (1..=50).rev().map(event).collect();
            cx.notify();
        });
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            assert!(frame.debug_bounds("message-event-50").is_some());
        }
        let before = view.update(cx, |view, _| view.list.logical_scroll_top());
        window
            .update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.focus.focus(window, cx);
                    assert!(view.focus.is_focused(window));
                });
                let _arena = window.draw(cx);
                assert!(window.is_action_available(&EarlierMessages, cx));
                assert!(window.dispatch_keystroke(
                    gpui_kit::Keystroke::parse("pageup").expect("page up"),
                    cx
                ));
                view.read_with(cx, |view, _| {
                    assert!(
                        view.list.logical_scroll_top().item_ix < before.item_ix,
                        "immediate offset: {:?}",
                        view.list.logical_scroll_top()
                    );
                });
            })
            .expect("message window");
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            assert!(frame.debug_bounds("chat-message-region").is_some());
        }
        view.update(cx, |view, _| {
            assert!(
                view.list.logical_scroll_top().item_ix < before.item_ix,
                "before={before:?} after={:?} viewport={:?}",
                view.list.logical_scroll_top(),
                view.list.viewport_bounds()
            );
        });
        window
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(
                    gpui_kit::Keystroke::parse("pagedown").expect("page down"),
                    cx,
                );
            })
            .expect("message window");
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.list.logical_scroll_top().item_ix, before.item_ix);
        });
    }

    #[gpui::test]
    fn live_preview_survives_partial_history_then_yields_to_every_distinct_stored_row(
        cx: &mut TestAppContext,
    ) {
        let (model, view, window) = open(cx);
        let mut user = event(1);
        user.kind = PageEventKind::User;
        let mut step = event(3);
        step.kind = PageEventKind::SceneStep;
        model.update(cx, |state, cx| {
            state.events = vec![step.clone(), event(2), user.clone()];
            cx.notify();
        });
        let mut answer = Shown {
            run: RunId(1),
            thread: Some(ThreadId(1)),
            text: "before after".into(),
            cut: true,
            done: false,
        };
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            for (id, visible) in [
                ("message-event-1", true),
                ("message-event-2", false),
                ("message-event-3", true),
                ("message-live-1", true),
            ] {
                assert_eq!(frame.debug_bounds(id).is_some(), visible, "{id}");
            }
        }
        model.update(cx, |state, cx| {
            state.events = vec![event(4), step, event(2), user];
            state.proven = Some(RunId(1));
            cx.notify();
        });
        answer.text.push_str(" late frame");
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        for id in [
            "message-event-1",
            "message-event-2",
            "message-event-3",
            "message-event-4",
        ] {
            assert!(frame.debug_bounds(id).is_some());
        }
        assert!(frame.debug_bounds("message-live-1").is_none());
        view.update(cx, |view, _| assert_eq!(view.list.item_count(), 4));
    }

    #[gpui::test]
    fn empty_repairs_keep_live_text_and_its_row_remeasures_with_the_font(cx: &mut TestAppContext) {
        let (model, view, window) = open(cx);
        let mut answer = Shown {
            run: RunId(7),
            thread: Some(ThreadId(1)),
            text: "Long wrapping answer. ".repeat(15),
            cut: false,
            done: false,
        };
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        model.update(cx, |state, cx| {
            state.loaded = true;
            cx.notify();
        });
        cx.run_until_parked();
        let before = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("live row")
                .size
                .height
        };
        answer
            .text
            .push_str(&" More streaming text that changes the row height.".repeat(20));
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        let grown = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("grown live row")
                .size
                .height
        };
        assert!(grown > before);
        cx.update(|cx| {
            gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(24.0));
        });
        cx.run_until_parked();
        let after = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("remeasured row")
                .size
                .height
        };
        view.update(cx, |view, _| {
            assert_eq!(view.list.item_count(), 1);
            assert_eq!(view.rem, px(24.0));
            assert!(after > grown);
        });
        view.update(cx, |view, cx| {
            view.live(
                Some(&Shown {
                    thread: Some(ThreadId(2)),
                    ..answer
                }),
                cx,
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _| assert_eq!(view.list.item_count(), 0));
        window
            .update(cx, |_, _, _| {})
            .expect("window remains open");
    }

    #[gpui::test]
    fn offscreen_live_updates_keep_the_history_anchor_and_remeasure_on_return(
        cx: &mut TestAppContext,
    ) {
        let (model, view, window) = open(cx);
        model.update(cx, |state, cx| {
            state.events = (1..=50).rev().map(event).collect();
            cx.notify();
        });
        let mut answer = Shown {
            run: RunId(7),
            thread: Some(ThreadId(1)),
            text: "Short live answer.".into(),
            cut: false,
            done: false,
        };
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        let before = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("initial live row")
                .size
                .height
        };
        view.update(cx, |view, cx| {
            view.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let anchor = view.update(cx, |view, _| view.list.logical_scroll_top());
        answer.text = "A growing answer while the user reads earlier history. ".repeat(200);
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            assert!(frame.debug_bounds("message-live-7").is_none());
        }
        view.update(cx, |view, cx| {
            let after = view.list.logical_scroll_top();
            assert_eq!(after.item_ix, anchor.item_ix);
            assert_eq!(after.offset_in_item, anchor.offset_in_item);
            view.list.scroll_to_end();
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("updated live row")
        };
        assert!(bounds.size.height > before);
        view.update(cx, |view, _| {
            assert!((bounds.bottom() - view.list.viewport_bounds().bottom()).abs() < px(1.0));
        });
    }

    #[gpui::test]
    fn font_changes_preserve_the_reading_position_inside_a_long_answer(cx: &mut TestAppContext) {
        let (_, view, window) = open(cx);
        let answer = Shown {
            run: RunId(7),
            thread: Some(ThreadId(1)),
            text: "A long answer with wrapping words. ".repeat(500),
            cut: false,
            done: false,
        };
        view.update(cx, |view, cx| view.live(Some(&answer), cx));
        cx.run_until_parked();
        let height = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("long row")
                .size
                .height
        };
        view.update(cx, |view, cx| {
            assert!(height > view.list.viewport_bounds().size.height * 3.0);
            view.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: height * 0.4,
            });
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            gpui_kit::component::Theme::update(cx, |theme| theme.font_size = px(24.0));
        });
        cx.run_until_parked();
        let resized_height = {
            let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
            frame
                .debug_bounds("message-live-7")
                .expect("resized row")
                .size
                .height
        };
        assert!(resized_height > height);
        view.update(cx, |view, _| {
            let offset = view.list.logical_scroll_top();
            assert_eq!(offset.item_ix, 0);
            assert!((offset.offset_in_item - resized_height * 0.4).abs() < px(1.0));
        });
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
