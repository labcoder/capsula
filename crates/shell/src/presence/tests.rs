use std::{sync::Arc, time::Instant};

use gpui_kit::TestAppContext;
use kumochi_protocol::message::{
    DataToShell, ErrorKey, Incarnation, Phase, RunId, RunOrigin, SceneHeader, SceneId, ThreadId,
    UserError,
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

#[gpui::test]
fn exhausted_page_ids_close_the_actual_link_not_only_the_view_models(cx: &mut TestAppContext) {
    use crate::link::fixture::{attended, connected, next, patient};
    let link = attended("page-exhaustion");
    let events = patient(link.events);
    let session = connected(next(&events));
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        owner.link = link.handle;
        owner.session = Some(session);
        owner.threads.update(cx, |state, _| {
            state.connected();
            state.show();
            state.exhaust_for_tests();
        });
        owner.request_pages(cx);
    });
    let end = Instant::now() + std::time::Duration::from_secs(5);
    while !link.link.is_finished() && Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        link.link.is_finished(),
        "exhaustion must close the live session"
    );
    assert!(link.link.join().unwrap());
}

fn mirrored_presenter() -> Presenter {
    deferred::start().expect("the dropper starts before intake");
    let mut presenter = Presenter::new(
        SurfaceGeometry::Overlay {
            default_canvas: [1920.0, 1080.0],
        },
        Arc::new(Theme::default()),
        Box::new(super::paint_cache::Stager(size_of::<
            super::paint_cache::Stager,
        >())),
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
                        layout_version: 2,
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
                    speech: None,
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
        assert!(activity.working);
        assert!(!activity.holds_global_escape());
    }
}

#[test]
fn focused_text_panel_escape_clears_idle_mirrored_scenes_without_a_global_hold() {
    use super::{Escape, escape::local_decision};

    let mut presenter = mirrored_presenter();
    drop(
        presenter
            .stop(RunId(1), Instant::now())
            .expect("idle stopped scene"),
    );
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
fn pending_mirrored_work_selects_stop_but_a_live_pull_keeps_precedence() {
    use super::{Escape, escape::local_decision};
    let presenter = mirrored_presenter();
    let activity = Presence::activity(&Runs::default(), EscapePanel::Open, &presenter);
    assert!(activity.working);
    for panel in [
        EscapePanel::Open,
        EscapePanel::SettingsEdit,
        EscapePanel::Rename,
    ] {
        assert_eq!(local_decision(activity, panel, &presenter), Escape::Stop);
    }
    assert_eq!(
        local_decision(
            Activity {
                pulling: true,
                ..activity
            },
            EscapePanel::Open,
            &presenter
        ),
        Escape::CancelPull
    );
    assert!(!activity.holds_global_escape());
}

#[gpui::test]
fn provider_done_keeps_the_presenters_stop_token_when_runs_stop_returns_none(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::ControlToBrain;
    let owner = test_fixture::presence(cx);
    let presenter = mirrored_presenter();
    let controls = owner.update(cx, |this, cx| {
        this.presenter = presenter;
        this.session = Some(Session::detached(1, Incarnation(1)));
        this.runs.connected(1);
        this.runs
            .apply_state(RunId(1), ThreadId(1), Phase::Answering, None);
        this.runs
            .apply_state(RunId(1), ThreadId(1), Phase::Done, None);
        this.runs
            .apply_data(DataToShell::RunEnd { run: RunId(1) }, 24);
        assert!(!this.runs.working());
        assert_eq!(this.runs.stop(), None);
        let controls = this.link.record_controls();
        this.activity = Presence::activity(&this.runs, EscapePanel::Closed, &this.presenter);
        assert!(this.activity.working);
        this.escape(cx).expect("Stop after provider Done");
        assert!(!this.presenter.live_work(Instant::now()));
        this.text.as_mut().expect("typing panel").typing.open();
        this.escape(cx)
            .expect("second clear before cut acknowledgement");
        assert!(!this.presenter.holds_scenes());
        controls
    });
    let ControlToBrain::StopScenes { run, cuts } = controls.try_recv().expect("covering Stop")
    else {
        panic!("StopScenes");
    };
    assert_eq!(run, RunId(1));
    assert_eq!(cuts.len(), 1);
    assert_eq!(cuts[0].scene, SceneId(1));
    assert_eq!(cuts[0].cut.activated, 0);
    assert_eq!(&*cuts[0].cut.revealed, &[]);
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::ClearDrawing {
            scene_ids: vec![SceneId(1)]
        })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
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

#[test]
fn a_reconnected_presenter_never_reuses_a_previous_sessions_stop_token() {
    let mut presenter = mirrored_presenter();
    assert_eq!(presenter.stoppable_run(), Some(RunId(1)));
    presenter.disconnected();
    presenter.connected(2);
    presenter
        .described(2, &Bounded::default(), Instant::now())
        .expect("fresh empty membership");
    assert_eq!(presenter.stoppable_run(), None);
    assert!(!presenter.live_work(Instant::now()));
    assert!(
        !Presence::activity(&Runs::default(), EscapePanel::Closed, &presenter)
            .holds_global_escape()
    );
}

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
                        generation: 1,
                        origin: Some(RunOrigin::Typed { turn: 1 }),
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
                        generation: 1,
                        origin: None,
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
fn reconnect_failure(run: u32, turn: u32) -> Batch {
    Batch::Plain(vec![
        LinkEvent::RunState {
            generation: 1,
            origin: Some(RunOrigin::Typed { turn }),
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
                    page(
                        ids[3],
                        ThreadId(3),
                        103,
                        "old session before terminal",
                        31,
                        0,
                    ),
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
fn unavailable_history_drops_stale_pages_without_crediting_a_replacement_session(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{DataToBrain, EventId, PageEvent, PageEventKind, PageOf};

    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, cx| {
        let (link, requests) = crate::link::LinkHandle::recording();
        owner.link = link;
        owner.session = Some(Session::detached(2, Incarnation(1)));
        owner.chat.update(cx, |state, _| {
            state.connected();
            state.show_for(Some(ThreadId(1)));
            assert_eq!(
                state.requests(),
                vec![
                    DataToBrain::PageRequest {
                        request: 1,
                        page: PageOf::Threads {
                            search: None,
                            after: None,
                        },
                    },
                    DataToBrain::PageRequest {
                        request: 2,
                        page: PageOf::Events {
                            thread: ThreadId(1),
                            before: None,
                        },
                    },
                ]
            );
            state.select(Some(ThreadId(2)));
        });
        let marker = |id| PageEvent {
            id: EventId(id),
            run: Some(RunId(7)),
            kind: PageEventKind::NoVisual { scene: SceneId(1) },
            text: String::new(),
            cut: false,
        };
        let page = |request, thread, generation, bytes, events| LinkEvent::Data {
            generation,
            bytes,
            message: DataToShell::Page {
                request,
                thread,
                events,
                before: Some(EventId(8)),
            },
        };
        owner
            .link_events(
                Batch::Plain(vec![
                    page(2, ThreadId(1), 1, 29, vec![marker(99)]),
                    page(2, ThreadId(1), 2, 11, vec![marker(99)]),
                    page(997, ThreadId(2), 2, 13, vec![marker(99)]),
                ]),
                cx,
            )
            .expect("stale history replies");
        assert_eq!(owner.session.as_ref().map(Session::granted), Some(24));
        let state = owner.chat.read(cx);
        assert_eq!(state.open, Some(ThreadId(2)));
        assert_eq!(state.events, []);
        assert_eq!(state.before, None);
        assert!(state.reading());
        assert_eq!(
            requests.try_recv(),
            Ok(DataToBrain::PageRequest {
                request: 3,
                page: PageOf::Events {
                    thread: ThreadId(2),
                    before: None,
                },
            })
        );
        assert_eq!(requests.try_recv(), Err(async_channel::TryRecvError::Empty));
        owner
            .link_events(
                Batch::Plain(vec![
                    page(3, ThreadId(2), 1, 31, vec![marker(99)]),
                    page(3, ThreadId(2), 2, 17, vec![marker(9), marker(8)]),
                    page(2, ThreadId(2), 2, 19, vec![marker(99)]),
                ]),
                cx,
            )
            .expect("current history reply");
        assert_eq!(owner.session.as_ref().map(Session::granted), Some(60));
        let state = owner.chat.read(cx);
        assert_eq!(state.open, Some(ThreadId(2)));
        assert_eq!(state.events, [marker(9), marker(8)]);
        assert_eq!(state.before, Some(EventId(8)));
        assert!(!state.reading());
        assert_eq!(owner.threads.read(cx).events, []);
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
    for (run, turn, typed, draft, after) in [
        (
            1,
            1,
            "Exact draft: café ✓ 日本語",
            "",
            "Exact draft: café ✓ 日本語",
        ),
        (2, 2, "older failed turn", "newer draft", "newer draft"),
        (3, 3, "older failed turn", " ", " "),
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
            this.link_events(reconnect_failure(run, turn), cx)
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

/// A delete that fails before hiding rereads its thread's open page, since the brain sends no
/// events marker for a thread while it is deleting; another thread's failed delete reads nothing.
#[gpui::test]
fn a_delete_that_fails_before_hiding_rereads_only_its_open_thread(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{DataToBrain, Deleted, PageOf};
    for (deleted, reread) in [(ThreadId(1), true), (ThreadId(2), false)] {
        let owner = test_fixture::presence(cx);
        let requests = owner.update(cx, |owner, cx| {
            let (link, requests) = crate::link::LinkHandle::recording();
            owner.link = link;
            owner.session = Some(Session::detached(1, Incarnation(1)));
            owner.chat.update(cx, |state, _| {
                state.connected();
                state.show_for(Some(ThreadId(1)));
                assert_eq!(state.requests().len(), 2);
                state.receive(
                    DataToShell::Page {
                        request: 2,
                        thread: ThreadId(1),
                        events: Vec::new(),
                        before: None,
                    },
                    0,
                );
                assert!(!state.reading());
            });
            assert!(owner.runs.confirm_delete(deleted));
            owner.sync_deletes(cx);
            requests
        });
        owner.update(cx, |owner, cx| {
            owner.thread_deleted(deleted, Deleted::NotHidden, None, cx);
        });
        let sent: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        let expected = reread.then_some(DataToBrain::PageRequest {
            request: 3,
            page: PageOf::Events {
                thread: ThreadId(1),
                before: None,
            },
        });
        assert_eq!(sent, Vec::from_iter(expected), "deleted {deleted:?}");
    }
}
