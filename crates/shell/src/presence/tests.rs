use std::{sync::Arc, time::Instant};

use gpui_kit::TestAppContext;
use kumochi_protocol::message::{
    DataToShell, ErrorKey, Incarnation, Phase, RunId, SceneHeader, SceneId, ThreadId, UserError,
};
use kumochi_scene::{
    Record,
    deferred::{self, Deferred},
    layout::SurfaceGeometry,
    parse_record,
    text::Bounded,
    theme::Theme,
};

use super::{Activity, EscapePanel, Presence, Runs, test_fixture};
use crate::link::{Batch, LinkEvent, Session};
use crate::presenter::{Effect, Presenter};

fn mirrored_presenter() -> Presenter {
    deferred::start().expect("the dropper starts before intake");
    let mut presenter = Presenter::new(
        SurfaceGeometry::Overlay {
            logical_size: [1920.0, 1080.0],
        },
        Arc::new(Theme::default()),
    );
    let now = Instant::now();
    presenter.connected(1);
    presenter
        .described(1, &Bounded::default(), now)
        .expect("empty membership");
    let Record::Begin(begin) =
        parse_record(br#"{"begin":{"v":1,"scene":"unpainted","surface":"overlay","alt":"test"}}"#)
            .expect("header")
    else {
        panic!("begin record");
    };
    presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneBegin {
                    scene: SceneId(1),
                    header: SceneHeader {
                        begin,
                        layout_version: 1,
                    },
                },
                100,
            ),
            100,
            now,
        )
        .expect("begin scene");
    let Record::Step(step) = parse_record(br#"{"step":{"id":"first","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,10,10]}}]}}"#).expect("step") else {
        panic!("step record");
    };
    let effects = presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneStepCommit {
                    run: RunId(1),
                    scene: SceneId(1),
                    index: 0,
                    step,
                },
                100,
            ),
            100,
            now,
        )
        .expect("admit first step");
    let mut admissions = effects.into_iter().filter_map(|effect| match effect {
        Effect::Admit(job) => Some(job),
        _ => None,
    });
    let job = admissions.next().expect("one admission");
    assert!(admissions.next().is_none());
    let effects = presenter
        .completed(job.run(), now)
        .expect("mirror first step");
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Prepare(_)))
            .count(),
        1
    );
    assert!(presenter.holds_scenes());
    presenter
}

#[test]
fn a_first_mirrored_v1_step_without_paint_does_not_hold_global_escape() {
    let presenter = mirrored_presenter();
    assert!(presenter.holds_scenes());
    let runs = Runs::default();
    assert!(!runs.working());
    assert!(!runs.drawing());
    for panel in [EscapePanel::Closed, EscapePanel::Open] {
        let activity = Presence::activity(&runs, panel, &presenter);
        assert!(!activity.drawing);
        assert!(!activity.holds_global_escape());
    }
}

#[test]
fn focused_text_panel_escape_clears_idle_mirrored_scenes_without_a_global_hold() {
    use super::{Escape, escape::local_decision};

    let presenter = mirrored_presenter();
    let activity = Presence::activity(&Runs::default(), EscapePanel::Open, &presenter);
    assert!(!activity.holds_global_escape());
    assert_eq!(
        local_decision(activity, EscapePanel::Open, &presenter),
        Escape::Clear
    );
    assert_eq!(
        local_decision(activity, EscapePanel::SettingsEdit, &presenter),
        Escape::CancelEdit
    );
    assert_eq!(
        local_decision(activity, EscapePanel::Rename, &presenter),
        Escape::CancelEdit
    );
    assert!(!activity.holds_global_escape());
}

#[test]
#[ignore = "opens a real hidden window: cargo test -p kumochi-shell -- --ignored hidden_window"]
fn a_hidden_window_takes_its_requested_size() {
    use gpui_kit::AppContext as _;

    let viewport = std::rc::Rc::new(std::cell::Cell::new(None));
    let seen = std::rc::Rc::clone(&viewport);
    gpui_kit::application().run(move |cx| {
        let window = cx
            .open_window(super::options(), |_, cx| {
                cx.new(|_| super::test_fixture::Blank)
            })
            .expect("a hidden window opens");
        seen.set(Some(
            window
                .update(cx, |_, window, _| window.viewport_size())
                .expect("the window is open"),
        ));
        cx.quit();
    });
    assert_eq!(
        viewport.get(),
        Some(gpui_kit::size(gpui_kit::px(112.0), gpui_kit::px(48.0)))
    );
}

/// What a `RunEnd` frame's size grants back [any nonzero size].
const RUN_END_BYTES: u32 = 24;

fn answer_history(
    owner: &gpui_kit::Entity<Presence>,
    requests: &async_channel::Receiver<kumochi_protocol::message::DataToBrain>,
    events: &[kumochi_protocol::message::PageEvent],
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{DataToBrain, PageOf};
    let mut pages = 0;
    let replies = std::iter::from_fn(|| requests.try_recv().ok())
        .map(|request| {
            let DataToBrain::PageRequest { request, page } = request else {
                panic!("only history requests are sent");
            };
            let message = match page {
                PageOf::Threads { .. } => DataToShell::ThreadList {
                    request,
                    threads: Vec::new(),
                    next: None,
                },
                PageOf::Events { thread, before } => {
                    assert_eq!(thread, ThreadId(1));
                    assert_eq!(before, None);
                    pages += 1;
                    DataToShell::Page {
                        request,
                        thread,
                        events: events.to_vec(),
                        before: None,
                    }
                }
            };
            LinkEvent::Data {
                generation: 1,
                message,
                bytes: 17,
            }
        })
        .collect();
    assert_eq!(pages, 1);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(replies), cx)
            .expect("history replies");
    });
}

#[gpui::test]
fn a_completed_visible_turn_and_its_reopened_first_page_both_replace_the_preview(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{EventId, PageEvent, PageEventKind};
    let owner = test_fixture::presence(cx);
    let (requests, window) = owner.update(cx, |owner, cx| {
        let (link, requests) = crate::link::LinkHandle::recording();
        owner.link = link;
        owner.session = Some(Session::detached(1, Incarnation(1)));
        owner.runs.met(Incarnation(1));
        owner.runs.connected(1);
        owner.runs.settings(ThreadId(1));
        owner
            .runs
            .submit("a submitted turn", |_| true)
            .expect("submitted");
        owner.chat.update(cx, |chat, cx| {
            chat.connected();
            chat.show_for(Some(ThreadId(1)));
            cx.notify();
        });
        let text = owner.text.as_mut().expect("typing fixture");
        text.typing.open();
        let window = text.window;
        owner.request_pages(cx);
        (requests, window)
    });
    answer_history(&owner, &requests, &[], cx);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::RunState {
                        run: RunId(1),
                        thread: ThreadId(1),
                        phase: Phase::Answering,
                        error: None,
                    },
                    LinkEvent::Data {
                        generation: 1,
                        message: DataToShell::AssistantText {
                            run: RunId(1),
                            delta: "joined live answer".into(),
                        },
                        bytes: 23,
                    },
                ]),
                cx,
            )
            .expect("answering and text");
    });
    answer_history(&owner, &requests, &[], cx);
    owner.update(cx, |owner, cx| {
        owner
            .link_events(
                Batch::Plain(vec![
                    LinkEvent::RunState {
                        run: RunId(1),
                        thread: ThreadId(1),
                        phase: Phase::Done,
                        error: None,
                    },
                    LinkEvent::Data {
                        generation: 1,
                        message: DataToShell::RunEnd { run: RunId(1) },
                        bytes: RUN_END_BYTES,
                    },
                ]),
                cx,
            )
            .expect("Done and RunEnd");
    });
    let stored = [2, 1].map(|id| PageEvent {
        id: EventId(id),
        run: Some(RunId(1)),
        kind: PageEventKind::AssistantText,
        text: format!("distinct stored section {id}"),
        cut: false,
    });
    answer_history(&owner, &requests, &stored, cx);
    cx.run_until_parked();
    stored_answer_is_visible(&owner, window, cx);
    owner.update(cx, |owner, cx| {
        owner
            .dismiss_text(cx)
            .expect("hide without a native window");
        assert_eq!(owner.chat.read(cx).proven, None);
        owner.open_text(cx).expect("reopen without native input");
    });
    answer_history(&owner, &requests, &stored, cx);
    cx.run_until_parked();
    stored_answer_is_visible(&owner, window, cx);
}

fn stored_answer_is_visible(
    owner: &gpui_kit::Entity<Presence>,
    window: gpui_kit::AnyWindowHandle,
    cx: &mut TestAppContext,
) {
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.chat.read(cx).proven, Some(RunId(1)));
    });
    let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
    assert!(frame.debug_bounds("message-live-1").is_none());
    assert!(frame.debug_bounds("message-event-1").is_some());
    assert!(frame.debug_bounds("message-event-2").is_some());
}

/// A reconnect failure as the first state naming `run`, then the run's last data frame, as the
/// link delivers them in one batch.
fn reconnect_failure(run: u32) -> Batch {
    Batch::Plain(vec![
        LinkEvent::RunState {
            run: RunId(run.into()),
            thread: ThreadId(1),
            phase: Phase::Failed,
            error: Some(UserError::from(ErrorKey::SessionEnded)),
        },
        LinkEvent::Data {
            generation: 1,
            message: DataToShell::RunEnd {
                run: RunId(run.into()),
            },
            bytes: RUN_END_BYTES,
        },
    ])
}

#[gpui::test]
fn actual_page_batches_route_to_one_history_and_grant_current_session_credit_once(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{DataToBrain, EventId, PageEvent, PageEventKind};
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.session = Some(Session::detached(1, Incarnation(1)));
        let mut ids = Vec::new();
        for (model, thread) in [(&owner.threads, ThreadId(2)), (&owner.chat, ThreadId(3))] {
            let requests = model.update(cx, |state, _| {
                state.connected();
                state.show_for(Some(thread));
                state.requests()
            });
            assert_eq!(requests.len(), 2);
            ids.extend(requests.into_iter().map(|request| {
                let DataToBrain::PageRequest { request, .. } = request else {
                    panic!("page request");
                };
                request
            }));
        }
        let page = |request, thread, id, text: &str, bytes, generation| LinkEvent::Data {
            generation,
            bytes,
            message: DataToShell::Page {
                request,
                thread,
                events: vec![PageEvent {
                    id: EventId(id),
                    run: None,
                    kind: PageEventKind::User,
                    text: text.into(),
                    cut: false,
                }],
                before: None,
            },
        };
        let list = |request, bytes| LinkEvent::Data {
            generation: 1,
            bytes,
            message: DataToShell::ThreadList {
                request,
                threads: Vec::new(),
                next: None,
            },
        };
        owner
            .link_events(
                Batch::Plain(vec![
                    page(ids[3], ThreadId(3), 9, "focused", 11, 1),
                    page(997, ThreadId(3), 99, "unsolicited", 13, 1),
                    page(ids[1], ThreadId(2), 7, "browsed", 17, 1),
                    list(ids[0], 19),
                    list(ids[2], 23),
                    page(ids[3], ThreadId(3), 101, "stale connection", 29, 0),
                ]),
                cx,
            )
            .expect("actual page batch");
        assert_eq!(owner.session.as_ref().map(Session::granted), Some(83));
        for (model, thread, id, text) in [
            (&owner.threads, ThreadId(2), 7, "browsed"),
            (&owner.chat, ThreadId(3), 9, "focused"),
        ] {
            let state = model.read(cx);
            assert_eq!(state.open, Some(thread));
            assert_eq!(
                state
                    .events
                    .iter()
                    .map(|event| (event.id, event.text.as_str()))
                    .collect::<Vec<_>>(),
                [(EventId(id), text)]
            );
            assert!(!state.reading());
            assert!(!state.listing());
        }
    });
}

#[gpui::test]
fn a_reconnect_failure_restores_its_turn_only_into_an_empty_composer(cx: &mut TestAppContext) {
    let presence = test_fixture::presence(cx);
    let (window, input) = presence.update(cx, |this, _| {
        this.session = Some(Session::detached(1, Incarnation(1)));
        this.runs.met(Incarnation(1));
        this.runs.connected(1);
        this.runs.settings(ThreadId(1));
        this.runs.apply_scenes(&[]).expect("no scene to recover");
        let text = this
            .text
            .as_ref()
            .expect("the fixture creates the typing panel");
        (text.window, text.input.clone())
    });
    // The run, the turn typed for it, the composer when it fails, and the composer after.
    for (run, typed, draft, after) in [
        (
            1,
            "Exact draft: café ✓ 日本語",
            "",
            "Exact draft: café ✓ 日本語",
        ),
        (2, "older failed turn", "newer draft", "newer draft"),
        (3, "older failed turn", " ", " "),
    ] {
        presence.update(cx, |this, _| {
            this.runs.submit(typed, |_| true).expect("sent");
        });
        window
            .update(cx, |_, window, cx| {
                input.update(cx, |input, cx| input.set_value(draft, window, cx));
            })
            .expect("live typing window");
        presence.update(cx, |this, cx| {
            this.link_events(reconnect_failure(run), cx)
                .expect("the batch applies");
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.value().to_string()),
            after,
            "run {run}"
        );
        presence.read_with(cx, |this, _| {
            assert!(!this.runs.working(), "run {run} settled with its RunEnd");
            assert_eq!(
                this.activity,
                Activity::default(),
                "run {run} left global Esc alone"
            );
            assert_eq!(
                this.session.as_ref().map(Session::granted),
                Some(run * RUN_END_BYTES),
                "run {run}'s RunEnd took the session's data route"
            );
        });
    }
}
