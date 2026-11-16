//! The retained, virtualized conversation shared by the task and chat surfaces.

use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::{ActiveTheme as _, ThemeStyled as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding,
    ListAlignment, ListOffset, ListState as MessageList, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, list, px,
};
use kumochi_protocol::message::{ActionState, DraftId, EventId, PageEventKind, RunId, ThreadId};
use std::rc::Rc;

pub(super) mod consent;

use super::Presence;
use crate::runs::Shown;
use crate::strings::{Strings, Text};
use crate::threads::Threads;

gpui_kit::actions!(thread_messages, [EarlierMessages, LaterMessages]);

/// The icon under a held turn whose picture was taken; another pick is this one line.
const PICTURED: gpui_kit::assets::IconName = gpui_kit::assets::IconName::Monitor;

pub(super) struct Messages {
    model: Entity<Threads>,
    owner: WeakEntity<Presence>,
    pending_issue: Option<consent::Pending>,
    list: MessageList,
    focus: FocusHandle,
    shown_thread: Option<ThreadId>,
    rows: Rc<[Row]>,
    issue_labels: consent::Labels,
    ids: Vec<(&'static str, u64)>,
    count: usize,
    rem: Pixels,
    live: Option<Shown>,
    preview: Option<Row>,
    live_row: Option<RunId>,
    proven: Option<RunId>,
    reveal_older: Option<(u64, EventId, Option<u64>)>,
    _watch: Subscription,
    github_watch: Option<Subscription>,
    _activation_watch: Subscription,
    _visibility_watch: Subscription,
    _release_watch: Subscription,
}

#[derive(Clone, PartialEq, Eq)]
struct Row {
    id: (&'static str, u64),
    draft: Option<DraftId>,
    run: Option<RunId>,
    assistant: bool,
    label: Option<SharedString>,
    text: SharedString,
    details: Option<SharedString>,
    warning: Option<SharedString>,
    feedback: Option<SharedString>,
    cut: Option<SharedString>,
    /// Under a held turn: why its screen hold's picture wasn't included.
    screen: Option<SharedString>,
    /// Under a held turn whose picture was taken: what its icon says.
    pictured: Option<SharedString>,
}

impl Row {
    fn visible(&self, live: Option<RunId>) -> bool {
        !self.assistant || self.run.is_none() || self.run != live || self.id.0 == "live"
    }
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("id", &self.id)
            .field("text_len", &self.text.len())
            .finish_non_exhaustive()
    }
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
        let shown_thread = state.open;
        let rows = stored_rows(state, cx);
        let count = rows.len();
        let ids = rows.iter().map(|row| row.id).collect();
        let messages = MessageList::new(count, ListAlignment::Bottom, px(0.0));
        messages.set_scroll_handler({
            let model = model.clone();
            let owner = owner.clone();
            move |event, _, cx| {
                if event.visible_range.start == 0 {
                    older(&model, &owner, cx);
                }
            }
        });
        let watch = cx.observe_in(&model, window, |view, model, window, cx| {
            view.dismiss_stale_issue(window, cx);
            let rows = stored_rows(model.read(cx), cx);
            let prefix = view
                .rows
                .iter()
                .zip(rows.iter())
                .take_while(|(before, after)| before.id == after.id)
                .count();
            let head = view.rows.iter().enumerate().zip(rows.iter()).take(prefix);
            let tail = view
                .rows
                .iter()
                .enumerate()
                .skip(prefix)
                .rev()
                .zip(rows.iter().skip(prefix).rev())
                .take_while(|((_, before), after)| before.id == after.id);
            for ((ix, before), after) in head.chain(tail) {
                if before != after {
                    view.list.remeasure_items(ix..ix + 1);
                }
            }
            view.rows = rows;
            view.sync(cx);
        });
        cx.defer_in(window, |view, window, cx| {
            view.github_watch = view.owner.upgrade().map(|owner| {
                let settings = owner.read(cx).settings.clone();
                cx.observe_in(&settings, window, |view, _, window, cx| {
                    view.dismiss_stale_issue(window, cx);
                    view.list.remeasure();
                    cx.notify();
                })
            });
        });
        let activation_watch = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.retire_issue(window, cx);
            }
        });
        let visibility_watch =
            cx.observe_window_visibility(window, |view, visibility, window, cx| {
                if !visibility.is_visible() {
                    view.retire_issue(window, cx);
                }
            });
        let release_window = window.window_handle();
        let release_watch = cx.on_release(move |view, cx| view.release_issue(release_window, cx));
        Self {
            model,
            owner,
            pending_issue: None,
            list: messages,
            focus: cx.focus_handle().tab_stop(true),
            shown_thread,
            rows,
            issue_labels: consent::Labels::new(cx.global::<Strings>()),
            ids,
            count,
            rem: window.rem_size(),
            live: None,
            preview: None,
            live_row: None,
            proven: None,
            reveal_older: None,
            _watch: watch,
            github_watch: None,
            _activation_watch: activation_watch,
            _visibility_watch: visibility_watch,
            _release_watch: release_watch,
        }
    }

    pub(super) fn live(&mut self, answer: Option<&Shown>, cx: &mut Context<'_, Self>) {
        if self.live.as_ref() != answer {
            self.live = answer.cloned();
            self.sync(cx);
        }
    }

    pub(super) fn older(&mut self, cx: &mut Context<'_, Self>) {
        let state = self.model.read(cx);
        if state.online()
            && !state.reading()
            && state.open.is_some()
            && let Some(before) = state.before
        {
            let first = self
                .rows
                .iter()
                .find(|row| row.visible(self.live_row))
                .map(|row| row.id.1);
            self.reveal_older = Some((state.history_revision(), before, first));
            older(&self.model, &self.owner, cx);
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
        self.preview = self
            .live
            .as_ref()
            .filter(|answer| Some(answer.run) == live)
            .map(|answer| {
                row(
                    ("live", answer.run.0),
                    Some(answer.run),
                    PageEventKind::AssistantText,
                    &answer.text,
                    answer.cut.then_some(Text::ChatLiveCut),
                    state,
                    cx,
                )
            });
        let ids: Vec<_> = self
            .rows
            .iter()
            .chain(self.preview.iter())
            .map(|row| row.id)
            .collect();
        let count = ids.len();
        if self.shown_thread == state.open {
            let prefix = self
                .ids
                .iter()
                .zip(&ids)
                .take_while(|(old, new)| old == new)
                .count();
            let suffix = self.ids[prefix..]
                .iter()
                .rev()
                .zip(ids[prefix..].iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            if prefix == 0 && suffix == 0 {
                self.list.reset(count);
            } else {
                self.list
                    .splice(prefix..self.ids.len() - suffix, count - prefix - suffix);
            }
            if self.live_row != live || self.proven != state.proven {
                self.list.remeasure_items(0..count);
            }
        } else {
            self.list.reset(count);
        }
        self.shown_thread = state.open;
        self.proven = state.proven;
        if let Some((revision, before, first)) = self.reveal_older {
            if state.history_revision() != revision || !state.online() || !state.loaded {
                self.reveal_older = None;
            } else if !state.reading() && state.before != Some(before) {
                self.reveal_older = None;
                let earlier = self
                    .rows
                    .iter()
                    .find(|row| row.visible(live))
                    .is_some_and(|row| first.is_none_or(|first| row.id.1 < first));
                if !earlier && let Some(before) = state.before {
                    self.reveal_older = Some((revision, before, first));
                    older(&self.model, &self.owner, cx);
                } else {
                    self.list.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.0),
                    });
                }
            }
        }
        self.ids = ids;
        self.count = count;
        self.live_row = live;
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
        let rows = Rc::clone(&self.rows);
        let issue_labels = self.issue_labels.clone();
        let live = self.preview.clone();
        let live_row = self.live_row;
        let model = self.model.clone();
        let owner = self.owner.clone();
        let view = cx.weak_entity();
        let messages = list(self.list.clone(), move |ix, _, cx| {
            let row = if ix == rows.len() {
                live.as_ref()
            } else {
                rows.get(ix)
            };
            row.filter(|row| row.visible(live_row)).map_or_else(
                || div().into_any_element(),
                |row| {
                    let controls = row.draft.map(|draft| {
                        consent::controls(draft, model.read(cx), &owner, &view, &issue_labels, cx)
                    });
                    message(row, controls, cx)
                },
            )
        })
        .size_full();
        div()
            .id("thread-messages")
            .debug_selector(|| "chat-message-region".into())
            .relative()
            .ml_1()
            .my_1()
            .flex_1()
            .min_h_0()
            .track_focus(&self.focus)
            .when(self.focus.is_focused(window), |this| {
                this.focus_ring_style(window, cx)
            })
            .key_context("ThreadMessages")
            .on_key_down(
                cx.listener(|view, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && view.cancel_waiting_review(window, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
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

fn stored_rows(state: &Threads, cx: &App) -> Rc<[Row]> {
    let mut rows: Vec<Row> = Vec::new();
    let mut current_group = None;
    for event in state.events.iter().rev() {
        let group = match (event.kind, event.run) {
            (PageEventKind::NoVisual { scene }, Some(run))
                if run.0 != 0
                    && event.text.is_empty()
                    && !event.cut
                    && scene.0 != 0
                    && i64::try_from(scene.0).is_ok() =>
            {
                Some((run, scene))
            }
            _ => None,
        };
        if group.is_some() && group == current_group {
            rows.last_mut().expect("a group has a row").id = ("event", event.id.0);
        } else {
            rows.push(row(
                ("event", event.id.0),
                event.run,
                event.kind,
                &event.text,
                event.cut.then_some(Text::ThreadCut),
                state,
                cx,
            ));
        }
        current_group = group;
    }
    if let Some(note) = state
        .screen_note
        .as_ref()
        .filter(|note| Some(note.thread) == state.open)
    {
        let after = note.after.map_or(0, |after| after.0);
        let at = rows
            .iter()
            .rposition(|row| row.id.1 <= after)
            .map_or(0, |ix| ix + 1);
        let mut line = row(
            ("note", after),
            None,
            PageEventKind::TaskDeleted,
            "",
            None,
            state,
            cx,
        );
        line.text = cx
            .global::<Strings>()
            .with_model(note.text, note.model.as_str())
            .into();
        rows.insert(at, line);
    }
    rows.into()
}

#[expect(
    clippy::too_many_lines,
    reason = "an issue card's row and a plain message's row share one builder"
)]
fn row(
    id: (&'static str, u64),
    run: Option<RunId>,
    kind: PageEventKind,
    text: &str,
    cut: Option<Text>,
    state: &Threads,
    cx: &App,
) -> Row {
    let strings = cx.global::<Strings>();
    if let PageEventKind::Issue { draft } = kind {
        let issue = state.issue(draft);
        let current = issue.as_ref().and_then(|entry| entry.state());
        let card = issue.as_ref().and_then(|entry| entry.card());
        let loading = issue.as_ref().is_some_and(|entry| entry.loading());
        let status = current.map_or_else(
            || {
                strings.get(if loading {
                    Text::IssueLoading
                } else {
                    Text::IssueUnavailable
                })
            },
            |state| strings.issue_status(state),
        );
        let details = card.map_or_else(
            || {
                strings.get(if loading {
                    Text::IssueDetailsLoading
                } else {
                    Text::IssueDetailsUnavailable
                })
            },
            |card| strings.issue_details(card),
        );
        let warning = matches!(
            current,
            Some(
                ActionState::Filed {
                    maybe_duplicate: true,
                    ..
                } | ActionState::Unknown {
                    maybe_duplicate: true,
                    ..
                }
            )
        );
        return Row {
            id,
            draft: Some(draft),
            run,
            assistant: false,
            label: Some(status.into()),
            text: strings
                .issue_title(card.map_or(text, |card| card.title.as_str()))
                .into(),
            details: Some(format!("{details}\n\n{}", strings.get(Text::IssueReadOnly)).into()),
            warning: warning.then(|| strings.get(Text::IssueDuplicate).into()),
            feedback: issue.as_ref().and_then(|entry| {
                if entry.pending() {
                    Some(
                        strings
                            .get(if entry.pending_review() {
                                Text::IssueReviewPending
                            } else {
                                Text::IssuePending
                            })
                            .into(),
                    )
                } else {
                    entry
                        .feedback
                        .map(|feedback| strings.issue_feedback(feedback).into())
                }
            }),
            cut: if card.is_none() {
                cut.map(|text| strings.get(text).into())
            } else {
                None
            },
            screen: None,
            pictured: None,
        };
    }
    let label = match kind {
        PageEventKind::User | PageEventKind::HeldTurn { .. } => Some(strings.get(Text::ThreadUser)),
        PageEventKind::AssistantText => Some(strings.assistant(state.settings.as_ref().map_or(
            kumochi_protocol::constants::DEFAULT_ASSISTANT_NAME,
            |settings| settings.assistant_name.as_str(),
        ))),
        PageEventKind::SceneStep | PageEventKind::NoVisual { .. } => {
            Some(strings.get(Text::ThreadNoVisual))
        }
        PageEventKind::Issue { .. } | PageEventKind::TaskDeleted => None,
    };
    let text = match kind {
        PageEventKind::TaskDeleted => strings.get(Text::ThreadDeleted),
        _ => text.to_owned(),
    };
    Row {
        id,
        draft: None,
        run,
        assistant: kind == PageEventKind::AssistantText,
        label: label.map(Into::into),
        text: text.into(),
        details: None,
        warning: None,
        feedback: None,
        cut: cut.map(|text| strings.get(text).into()),
        screen: match kind {
            PageEventKind::HeldTurn { screen } => {
                Text::held_screen(screen).map(|text| strings.get(text).into())
            }
            _ => None,
        },
        pictured: match kind {
            PageEventKind::HeldTurn { screen } => strings.pictured(screen).map(Into::into),
            _ => None,
        },
    }
}

fn message_text(row: &Row, field: &'static str, value: SharedString) -> gpui_kit::Text {
    if row.details.is_some() {
        gpui_kit::Text::new(field.into(), value)
    } else {
        gpui_kit::Text::new_inaccessible(value)
    }
}

fn message(row: &Row, controls: Option<gpui_kit::AnyElement>, cx: &App) -> gpui_kit::AnyElement {
    use gpui_kit::StatefulInteractiveElement as _;
    let id = row.id;
    if id.0 == "note" {
        // A quiet line that isn't a message: centred and muted, with the picture's icon.
        return div()
            .w_full()
            .p_2()
            .flex()
            .justify_center()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(gpui_kit::svg().path(PICTURED.path()).size_4().flex_none())
            .child(row.text.clone())
            .into_any_element();
    }
    div()
        .id(id)
        .debug_selector(move || format!("message-{}-{}", id.0, id.1))
        .w_full()
        .p_3()
        .px_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .children(row.label.clone().map(|label| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(message_text(row, "issue-status", label))
        }))
        .child(
            div()
                .text_sm()
                .child(message_text(row, "issue-title", row.text.clone())),
        )
        .children(row.warning.clone().map(|warning| {
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(message_text(row, "issue-warning", warning))
        }))
        .children(row.details.clone().map(|details| {
            div()
                .text_sm()
                .child(message_text(row, "issue-details", details))
        }))
        .children(row.pictured.clone().map(|tip| {
            // The row around it keeps the icon, and so its tooltip, to the icon's own size.
            div().flex().child(
                div()
                    .id("held-picture")
                    .role(gpui_kit::Role::Image)
                    .aria_label(tip.clone())
                    .debug_selector({
                        let tip = tip.clone();
                        move || format!("held-picture-{} {tip}", id.1)
                    })
                    .child(
                        // shortcut: today's theme has no text colour subtler than muted, so the icon
                        // rests at muted and brightens to the text colour; use the design's subtle
                        // and muted pair once the theme has both.
                        gpui_kit::svg()
                            .path(PICTURED.path())
                            .size_4()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|style| style.text_color(cx.theme().foreground)),
                    )
                    .tooltip(move |window, cx| {
                        let tip = tip.clone();
                        gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
                            div()
                                .max_w(px(304.0))
                                .debug_selector({
                                    let tip = tip.clone();
                                    move || format!("held-picture-tip {tip}")
                                })
                                .child(tip.clone())
                        })
                        .build(window, cx)
                    }),
            )
        }))
        .children(
            row.screen
                .clone()
                .into_iter()
                .chain(row.cut.clone())
                .map(|notice| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(notice)
                }),
        )
        .children(row.feedback.clone().map(|feedback| {
            div()
                .text_sm()
                .child(message_text(row, "issue-feedback", feedback))
        }))
        .children(controls)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    mod issues;
    use gpui_kit::{
        AppContext as _, Bounds, ListOffset, TestAppContext, WindowBounds, WindowOptions, point,
        size,
    };
    use kumochi_protocol::message::{EventId, PageEvent, RunId, SceneId};

    fn event(id: u64) -> PageEvent {
        PageEvent {
            id: EventId(id),
            run: Some(RunId(1)),
            kind: PageEventKind::AssistantText,
            text: format!("message {id}"),
            cut: false,
        }
    }

    fn marker(id: u64, run: Option<RunId>, scene: u64) -> PageEvent {
        PageEvent {
            id: EventId(id),
            run,
            kind: PageEventKind::NoVisual {
                scene: SceneId(scene),
            },
            text: String::new(),
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
        open_with_events(cx, Vec::new())
    }

    fn open_with_events(
        cx: &mut TestAppContext,
        events: Vec<PageEvent>,
    ) -> (Entity<Threads>, Entity<Messages>, gpui_kit::AnyWindowHandle) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Strings::english());
            let model = cx.new(|_| {
                let mut state = Threads::chat_for_tests();
                state.open = Some(ThreadId(1));
                state.events = events;
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
    fn populated_history_starts_grouped_and_older_pages_keep_the_newest_group_key(
        cx: &mut TestAppContext,
    ) {
        let (model, view, _) = open_with_events(
            cx,
            vec![
                event(6),
                marker(5, Some(RunId(1)), 1),
                marker(4, Some(RunId(1)), 1),
            ],
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.count, 2);
            assert_eq!(view.list.item_count(), 2);
            assert_eq!(view.ids, [("event", 5), ("event", 6)]);
            assert!(!view.is_empty());
        });
        model.update(cx, |state, cx| {
            state.events.extend([
                marker(3, Some(RunId(1)), 1),
                marker(2, Some(RunId(1)), 1),
                event(1),
            ]);
            state.before = Some(EventId(1));
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.count, 3);
            assert_eq!(view.list.item_count(), 3);
            assert_eq!(view.ids, [("event", 1), ("event", 5), ("event", 6)]);
        });
        model.read_with(cx, |state, _| {
            assert_eq!(
                state
                    .events
                    .iter()
                    .map(|event| event.id.0)
                    .collect::<Vec<_>>(),
                [6, 5, 4, 3, 2, 1]
            );
            assert_eq!(state.before, Some(EventId(1)));
        });
    }

    #[gpui::test]
    fn a_held_turn_is_a_user_turn_with_an_icon_for_its_picture_or_a_line_saying_why_none(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::{HeldScreen, ScreenClass};
        cx.update(|cx| {
            cx.set_global(Strings::english());
            let mut user = event(1);
            user.kind = PageEventKind::User;
            let mut state = Threads::chat_for_tests();
            state.events = vec![user.clone()];
            let plain = stored_rows(&state, cx);
            assert_eq!(plain.len(), 1);
            assert_eq!(
                plain[0].label.as_deref(),
                Some(cx.global::<Strings>().get(Text::ThreadUser).as_str())
            );
            assert_eq!((&plain[0].screen, &plain[0].pictured), (&None, &None));
            for (screen, line, icon) in [
                (
                    HeldScreen::Attached,
                    None,
                    Some("A snapshot of the window under your pointer was sent in this message. No copy is kept.".to_owned()),
                ),
                (
                    HeldScreen::Screen,
                    None,
                    Some("A snapshot of your screen was sent in this message. No copy is kept.".to_owned()),
                ),
                (
                    HeldScreen::NotIncluded(ScreenClass::Changed),
                    Some("Screen not included: it changed while the picture was being taken"),
                    None,
                ),
                (
                    HeldScreen::NotIncluded(ScreenClass::ModelCantSee),
                    Some("Screen not included: screen sharing isn't available with this model"),
                    None,
                ),
                (
                    HeldScreen::Withdrawn,
                    Some("Screen not included: you turned screen sharing off"),
                    None,
                ),
            ] {
                user.kind = PageEventKind::HeldTurn { screen };
                state.events = vec![user.clone()];
                let held = stored_rows(&state, cx);
                assert_eq!(held.len(), 1);
                assert_eq!(held[0].screen.as_deref(), line, "{screen:?}");
                assert_eq!(held[0].pictured.as_deref(), icon.as_deref(), "{screen:?}");
                // Everything else is the plain user turn.
                let mut same = held[0].clone();
                same.screen = None;
                same.pictured = None;
                assert!(same == plain[0], "{screen:?}");
            }
        });
    }

    #[gpui::test]
    fn a_screen_note_is_a_line_after_the_message_it_followed_in_its_own_conversation_only(
        cx: &mut TestAppContext,
    ) {
        use crate::threads::ScreenNote;
        use kumochi_protocol::message::ModelName;
        cx.update(|cx| {
            cx.set_global(Strings::english());
            let mut state = Threads::chat_for_tests();
            state.open = Some(ThreadId(3));
            let (mut older, mut newer) = (event(5), event(7));
            older.kind = PageEventKind::User;
            newer.kind = PageEventKind::User;
            state.events = vec![newer, older];
            let note = |after, text| ScreenNote {
                thread: ThreadId(3),
                after,
                text,
                model: ModelName::new("GPT-6 Sol").unwrap(),
            };
            let ids = |rows: &[Row]| rows.iter().map(|row| row.id).collect::<Vec<_>>();
            assert_eq!(ids(&stored_rows(&state, cx)), [("event", 5), ("event", 7)]);
            state.screen_note = Some(note(Some(EventId(5)), Text::ScreenNoteCant));
            let rows = stored_rows(&state, cx);
            assert_eq!(ids(&rows), [("event", 5), ("note", 5), ("event", 7)]);
            assert_eq!(
                rows[1].text.as_ref(),
                "GPT-6 Sol doesn't support screenshots, so messages will go without one."
            );
            assert_eq!(rows[1].label, None);
            // Said in an empty conversation, it stays above what came later.
            state.screen_note = Some(note(None, Text::ScreenNoteCan));
            let rows = stored_rows(&state, cx);
            assert_eq!(ids(&rows), [("note", 0), ("event", 5), ("event", 7)]);
            assert_eq!(
                rows[0].text.as_ref(),
                "GPT-6 Sol supports screenshots, so new messages will send one along."
            );
            // Another conversation is open: the line stays with its own.
            state.open = Some(ThreadId(4));
            assert_eq!(ids(&stored_rows(&state, cx)), [("event", 5), ("event", 7)]);
        });
    }

    #[gpui::test]
    fn only_a_pictured_turn_draws_the_icon_and_hovering_it_shows_its_sentence(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::{HeldScreen, ScreenClass};
        let sentences = [
            "A snapshot of your screen was sent in this message. No copy is kept.".to_owned(),
            "A snapshot of the window under your pointer was sent in this message. No copy is kept.".to_owned(),
        ];
        let events = [
            PageEventKind::HeldTurn {
                screen: HeldScreen::Screen,
            },
            PageEventKind::HeldTurn {
                screen: HeldScreen::Attached,
            },
            PageEventKind::HeldTurn {
                screen: HeldScreen::NotIncluded(ScreenClass::Changed),
            },
            PageEventKind::User,
        ]
        .into_iter()
        .zip(1..)
        .map(|(kind, id)| PageEvent { kind, ..event(id) })
        .collect();
        let (_, _, window) = open_with_events(cx, events);
        cx.run_until_parked();
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        // Each turn against each sentence: the screen turn says the first and the window turn
        // the second, and neither the refused turn nor the typed one draws an icon.
        let drawn = (1..=4).map(|id| {
            sentences
                .each_ref()
                .map(|tip| frame.debug_bounds(format!("held-picture-{id} {tip}").leak()))
        });
        let drawn: Vec<_> = drawn.collect();
        assert_eq!(
            drawn
                .iter()
                .map(|icons| icons.map(|icon| icon.is_some()))
                .collect::<Vec<_>>(),
            [[true, false], [false, true], [false, false], [false, false]]
        );
        let icon = drawn[0][0].expect("the screen turn's icon");
        // The icon is 16 pixels square, not the row's width, so only it answers a hover.
        assert_eq!(icon.size, size(px(16.0), px(16.0)));
        let tip: &str = format!("held-picture-tip {}", sentences[0]).leak();
        assert!(frame.debug_bounds(tip).is_none());
        frame.simulate_mouse_move(icon.center(), None, gpui_kit::Modifiers::default());
        frame
            .executor()
            .advance_clock(std::time::Duration::from_millis(500));
        frame.run_until_parked();
        frame.update(|window, cx| window.draw(cx).clear(cx));
        assert!(frame.debug_bounds(tip).is_some());
    }

    #[gpui::test]
    fn unavailable_rows_only_group_adjacent_empty_trusted_markers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Strings::english());
            let sentinel = "private drawing text {unchanged} — 窓";
            let run = Some(RunId(1));
            let other = Some(RunId(2));
            let max_scene = (1_u64 << 63) - 1;
            let mut text = marker(7, other, 2);
            text.text = sentinel.into();
            let mut cut = marker(9, other, 2);
            cut.cut = true;
            let mut legacy = marker(13, other, 2);
            legacy.kind = PageEventKind::SceneStep;
            legacy.text = sentinel.into();
            legacy.cut = true;
            let mut empty_legacy = marker(14, other, 2);
            empty_legacy.kind = PageEventKind::SceneStep;
            let mut user = event(21);
            user.kind = PageEventKind::User;
            user.text = sentinel.into();
            let mut state = Threads::chat_for_tests();
            state.events = vec![
                marker(1, run, 1),
                marker(2, run, 1),
                marker(3, other, 1),
                marker(4, other, 1),
                marker(5, other, 2),
                marker(6, other, 2),
                text,
                marker(8, other, 2),
                cut,
                marker(10, other, 2),
                marker(11, None, 2),
                marker(12, None, 2),
                legacy,
                empty_legacy,
                marker(15, other, 2),
                marker(16, other, 0),
                marker(17, other, 0),
                marker(18, other, 2),
                marker(19, other, u64::MAX),
                marker(20, other, u64::MAX),
                user,
                marker(22, other, max_scene),
                marker(23, other, max_scene),
                marker(24, other, max_scene + 1),
                marker(25, other, max_scene + 1),
                marker(26, Some(RunId(0)), 2),
                marker(27, Some(RunId(0)), 2),
            ];
            state.events.reverse();
            let raw = state.events.clone();
            let rows = stored_rows(&state, cx);
            assert_eq!(
                rows.iter().map(|row| row.id.1).collect::<Vec<_>>(),
                [
                    2, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 23, 24, 25,
                    26, 27
                ]
            );
            for row in rows.iter().filter(|row| row.id.1 != 21) {
                assert_eq!(row.label.as_deref(), Some("Drawing unavailable in history"));
                assert_eq!(
                    row.text.as_ref(),
                    if [7, 13].contains(&row.id.1) {
                        sentinel
                    } else {
                        ""
                    }
                );
                assert_eq!(
                    row.cut.as_deref(),
                    [9, 13]
                        .contains(&row.id.1)
                        .then_some("This message was shortened.")
                );
            }
            assert_eq!(rows[17].label.as_deref(), Some("You"));
            assert_eq!(rows[17].text.as_ref(), sentinel);
            let debug = format!("{rows:?}");
            assert!(!debug.contains(sentinel));
            assert!(!debug.contains(&format!("{:?}", sentinel.as_bytes())));
            assert_eq!(state.events, raw);
        });
    }

    #[gpui::test]
    fn a_task_deleted_row_renders_the_exact_localized_note(cx: &mut TestAppContext) {
        let (model, view, window) = open(cx);
        model.update(cx, |state, cx| {
            state.events = vec![PageEvent {
                id: EventId(7),
                run: None,
                kind: PageEventKind::TaskDeleted,
                text: String::new(),
                cut: false,
            }];
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.rows.len(), 1);
            assert_eq!(view.rows[0].id, ("event", 7));
            assert_eq!(view.rows[0].label, None);
            assert_eq!(view.rows[0].text.as_ref(), "You deleted a task.");
        });
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        assert!(frame.debug_bounds("message-event-7").is_some());
    }

    #[gpui::test]
    fn cached_row_updates_keep_exact_text_and_redact_the_snapshot(cx: &mut TestAppContext) {
        let (model, view, _) = open(cx);
        let sentinel = "private-row-sentinel-83c2";
        model.update(cx, |state, cx| {
            let mut stored = event(1);
            stored.text = sentinel.into();
            state.events = vec![stored];
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.rows[0].text.as_ref(), sentinel);
            let debug = format!("{:?}", view.rows);
            assert!(!debug.contains(sentinel));
            assert!(!debug.contains(&format!("{:?}", sentinel.as_bytes())));
        });
        model.update(cx, |state, cx| {
            state.events[0].text = "A repaired row with the same event id".into();
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            assert_eq!(
                view.rows[0].text.as_ref(),
                "A repaired row with the same event id"
            );
            view.live(
                Some(&Shown {
                    run: RunId(7),
                    thread: Some(ThreadId(1)),
                    text: sentinel.into(),
                    cut: true,
                    done: false,
                }),
                cx,
            );
            assert_eq!(
                view.preview.as_ref().and_then(|row| row.cut.as_deref()),
                Some("Earlier streamed text isn't shown here.")
            );
        });
    }

    #[gpui::test]
    fn live_insertion_and_stored_proof_preserve_a_reader_above_the_tail(cx: &mut TestAppContext) {
        let (model, view, _) = open(cx);
        model.update(cx, |state, cx| {
            state.events = (1..=50).rev().map(event).collect();
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.list.scroll_to(ListOffset {
                item_ix: 10,
                offset_in_item: px(0.0),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let anchor = view.update(cx, |view, _| view.list.logical_scroll_top());
        view.update(cx, |view, cx| {
            view.live(
                Some(&Shown {
                    run: RunId(7),
                    thread: Some(ThreadId(1)),
                    text: "A new live answer".into(),
                    cut: false,
                    done: false,
                }),
                cx,
            );
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            let actual = view.list.logical_scroll_top();
            assert_eq!(actual.item_ix, anchor.item_ix);
            assert_eq!(actual.offset_in_item, anchor.offset_in_item);
        });
        model.update(cx, |state, cx| {
            let mut stored = event(51);
            stored.run = Some(RunId(7));
            state.events.insert(0, stored);
            state.proven = Some(RunId(7));
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            let actual = view.list.logical_scroll_top();
            assert_eq!(actual.item_ix, anchor.item_ix);
            assert_eq!(actual.offset_in_item, anchor.offset_in_item);
            assert_eq!(view.live_row, None);
            assert_eq!(view.list.item_count(), 51);
        });
    }

    #[gpui::test]
    fn a_repair_with_no_shared_rows_resets_a_scrolled_reader_to_the_latest_tail(
        cx: &mut TestAppContext,
    ) {
        let (model, view, _) = open(cx);
        model.update(cx, |state, cx| {
            state.events = (1..=50).rev().map(event).collect();
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.list.scroll_to(ListOffset {
                item_ix: 10,
                offset_in_item: px(7.0),
            });
        });
        model.update(cx, |state, cx| {
            state.events = (101..=150).rev().map(event).collect();
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.list.item_count(), 50);
            assert_eq!(view.list.logical_scroll_top().item_ix, 50);
            assert_eq!(view.ids.first(), Some(&("event", 101)));
            assert_eq!(view.ids.last(), Some(&("event", 150)));
        });
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
    fn pending_older_navigation_is_retired_by_reconnect_selection_or_failure(
        cx: &mut TestAppContext,
    ) {
        use kumochi_protocol::message::DataToShell;

        for change in ["reconnect", "selection", "failure"] {
            let (model, view, _) = open_with_events(cx, Vec::new());
            model.update(cx, |state, cx| {
                state.connected();
                state.show();
                assert_eq!(state.requests().len(), 2);
                state.apply_for_tests(DataToShell::ThreadList {
                    request: 1,
                    threads: Vec::new(),
                    next: None,
                });
                state.apply_for_tests(DataToShell::Page {
                    request: 2,
                    thread: ThreadId(1),
                    events: (11..=30).rev().map(event).collect(),
                    before: Some(EventId(11)),
                });
                cx.notify();
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                view.older(cx);
                assert_eq!(
                    view.reveal_older,
                    Some((model.read(cx).history_revision(), EventId(11), Some(11)))
                );
            });
            model.update(cx, |state, cx| {
                state.older_events();
                assert_eq!(state.requests().len(), 1);
                match change {
                    "reconnect" => {
                        state.disconnected();
                        state.connected();
                        assert_eq!(state.requests().len(), 2);
                        state.apply_for_tests(DataToShell::Page {
                            request: 5,
                            thread: ThreadId(1),
                            events: (31..=50).rev().map(event).collect(),
                            before: Some(EventId(31)),
                        });
                    }
                    "selection" => state.select(Some(ThreadId(2))),
                    "failure" => state.apply_for_tests(DataToShell::PageFailed { request: 3 }),
                    _ => unreachable!("three terminal changes"),
                }
                state.apply_for_tests(DataToShell::Page {
                    request: 3,
                    thread: ThreadId(1),
                    events: vec![event(1)],
                    before: None,
                });
                cx.notify();
            });
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.reveal_older, None);
                assert_eq!(view.list.logical_scroll_top().item_ix, view.count);
            });
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
            assert_eq!(view.ids, []);
        });
    }
}
