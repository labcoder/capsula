use capsula_protocol::message::{DATA_WINDOW, MAX_PENDING_PAGES, PageOf};

use super::*;
use crate::coordinator::{Coordinator, Effect, StoreOp};

fn threads() -> PageOf {
    PageOf::Threads {
        search: None,
        after: None,
    }
}

async fn request(shell: &mut Shell<DuplexStream>, request: u32, page: PageOf) {
    let frame =
        frame::encode(&DataToBrain::PageRequest { request, page }, Pipe::Data).expect("encodes");
    shell.data_out.write_all(&frame).await.expect("sent");
}

fn exhausted_queue() -> RefCell<DataQueue> {
    let mut queue = DataQueue::new(window());
    queue.push(Some(RunId(99)), vec![0; window()]);
    assert_eq!(
        queue
            .pop(std::time::Instant::now())
            .expect("initial credit")
            .len(),
        window()
    );
    queue.wrote();
    RefCell::new(queue)
}

#[test]
fn page_forwarding_leaves_the_current_run_and_its_pending_writes_alone() {
    let mut coordinator = Coordinator::new(ThreadId(0), RunId(0));
    coordinator
        .handle(Input::Turn {
            thread: None,
            text: "go".to_owned(),
        })
        .expect("turn");
    let page = threads();
    assert_eq!(
        coordinator.handle(Input::ReadPage {
            epoch: 1,
            request: 3,
            page: page.clone()
        }),
        Ok(vec![Effect::Store(StoreOp::ReadPage {
            epoch: 1,
            request: 3,
            page
        })])
    );
    let reply = DataToShell::ThreadList {
        request: 3,
        threads: Vec::new(),
        next: None,
    };
    assert_eq!(
        coordinator.handle(Input::PageRead {
            epoch: 1,
            request: 3,
            reply: Some(reply.clone())
        }),
        Ok(vec![Effect::Data(reply)])
    );
    assert_eq!(coordinator.current_run(), Some(RunId(1)));
    assert_eq!(coordinator.ending_runs(), 0);
    assert_eq!(
        coordinator.handle(Input::TurnStored { run: RunId(1) }),
        Ok(vec![Effect::StartEngine {
            run: RunId(1),
            text: "go".to_owned()
        }])
    );
}

#[tokio::test]
async fn a_fifth_unanswered_page_breaks_the_connection_even_after_reads_finish() {
    let scratch = Scratch::new("page-flood");
    let mut brain = test_support::brain(&scratch, "");
    let queue = exhausted_queue();
    let ready = Notify::new();
    let room = Notify::new();
    let (out, _control) = mpsc::channel(64);
    let (_controls_tx, mut controls) = mpsc::channel(64);
    let (turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
    let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
    let (mut input, mut shell) = tokio::io::duplex(DUPLEX_BUFFER);
    let mut tap = Tap::from_env();
    let link = Link {
        out: &out,
        queue: &queue,
        ready: &ready,
        tap: &mut tap,
    };
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let reader = super::super::read_data(&mut input, &turns_tx, &pages_tx, &queue, &ready, 0);
    let client = async {
        for request in 0..u32::try_from(MAX_PENDING_PAGES).expect("small") {
            let frame = frame::encode(
                &DataToBrain::PageRequest {
                    request,
                    page: threads(),
                },
                Pipe::Data,
            )
            .expect("frame");
            shell.write_all(&frame).await.expect("sent");
        }
        timeout(PATIENCE, async {
            while queue.borrow().frames_for_tests() < MAX_PENDING_PAGES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all four replies are waiting for credit");
        let frame = frame::encode(
            &DataToBrain::PageRequest {
                request: 4,
                page: threads(),
            },
            Pipe::Data,
        )
        .expect("frame");
        shell.write_all(&frame).await.expect("fifth sent");
        std::future::pending::<()>().await;
    };
    let ended = timeout(PATIENCE, async {
        tokio::select! { ended = core => ended, ended = reader => ended, () = client => unreachable!() }
    }).await.expect("connection ends");
    assert_eq!(ended, Ended::Broken("too many unanswered page requests"));
    brain.close().await;
}

#[tokio::test]
async fn a_page_waits_for_credit_and_is_not_revoked_by_a_matching_run_id() {
    let scratch = Scratch::new("page-credit");
    let mut brain = test_support::brain(&scratch, "5000 text later");
    let queue = exhausted_queue();
    let (ready, room) = (Notify::new(), Notify::new());
    let (out, mut control) = mpsc::channel(64);
    let (controls_tx, mut controls) = mpsc::channel(64);
    let (turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
    let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
    let (mut output, mut client) = tokio::io::duplex(DUPLEX_BUFFER);
    let mut tap = Tap::from_env();
    let link = Link {
        out: &out,
        queue: &queue,
        ready: &ready,
        tap: &mut tap,
    };
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let writer = super::super::write_data(&mut output, &queue, &ready, &room);
    let client = async {
        assert!(queue.borrow_mut().admit_page());
        pages_tx
            .send(Input::ReadPage {
                epoch: 0,
                request: 7,
                page: PageOf::Events {
                    thread: ThreadId(1),
                    before: None,
                },
            })
            .await
            .expect("request");
        timeout(PATIENCE, async {
            while queue.borrow().bytes() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reply queued");
        assert!(
            timeout(
                Duration::from_millis(30),
                ipc::read::<DataToShell>(&mut client, Pipe::Data)
            )
            .await
            .is_err()
        );
        turns_tx.send((None, "go".to_owned())).await.expect("turn");
        let bytes = timeout(PATIENCE, control.recv())
            .await
            .expect("state")
            .expect("open");
        assert!(matches!(
            frame::decode::<ControlToShell>(&bytes[frame::HEADER_LEN..]).expect("decode"),
            ControlToShell::RunState {
                run: RunId(1),
                thread: ThreadId(1),
                phase: Phase::Thinking
            }
        ));
        controls_tx
            .send(ControlToBrain::Stop {
                run: RunId(1),
                revealed: Vec::new(),
            })
            .await
            .expect("stop");
        loop {
            let bytes = timeout(PATIENCE, control.recv())
                .await
                .expect("stop answered")
                .expect("open");
            if matches!(
                frame::decode::<ControlToShell>(&bytes[frame::HEADER_LEN..]).expect("decode"),
                ControlToShell::RunState {
                    run: RunId(1),
                    phase: Phase::Interrupted,
                    ..
                }
            ) {
                break;
            }
        }
        queue.borrow_mut().grant(DATA_WINDOW);
        ready.notify_one();
        assert_eq!(
            ipc::read::<DataToShell>(&mut client, Pipe::Data)
                .await
                .expect("read"),
            Some(DataToShell::Page {
                request: 7,
                thread: ThreadId(1),
                events: Vec::new(),
                before: None
            })
        );
    };
    tokio::select! { ended = core => panic!("core ended {ended:?}"), ended = writer => panic!("writer ended {ended:?}"), () = client => {} }
    brain.close().await;
}

fn paged_fixture(scratch: &Scratch) -> (Brain, String, String, String) {
    let title = "title".repeat(6_000);
    let history = "history".repeat(4_000);
    let mut store = Store::open(&scratch.db()).expect("store");
    store
        .begin_turn(ThreadId(1), Some(&title), RunId(1), &history)
        .expect("history");
    store
        .end_run(ThreadId(1), RunId(1), &[], crate::store::RunEnd::Done)
        .expect("ended");
    drop(store);
    let expected = format!(
        "{}{}{}",
        "a".repeat(140_000),
        "b".repeat(140_000),
        "c".repeat(129_600)
    );
    let script = format!(
        "0 text {}\n0 text {}\n0 text {}",
        "a".repeat(140_000),
        "b".repeat(140_000),
        "c".repeat(129_600)
    );
    (
        test_support::brain(scratch, &script),
        title,
        history,
        expected,
    )
}

#[tokio::test]
async fn a_fixture_finishes_in_order_while_both_pages_wait_for_credit_and_a_later_turn_starts() {
    let scratch = Scratch::new("pages-stream");
    let (mut brain, title, history, expected) = paged_fixture(&scratch);
    let queue = exhausted_queue();
    let (ready, room) = (Notify::new(), Notify::new());
    let (out, mut control) = mpsc::channel(64);
    let (_controls_tx, mut controls) = mpsc::channel(64);
    let (turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
    let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
    let (mut output, mut client) = tokio::io::duplex(DUPLEX_BUFFER);
    let mut tap = Tap::from_env();
    let link = Link {
        out: &out,
        queue: &queue,
        ready: &ready,
        tap: &mut tap,
    };
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let writer = super::super::write_data(&mut output, &queue, &ready, &room);
    let client = async {
        for (request, page) in [
            (10, threads()),
            (
                11,
                PageOf::Events {
                    thread: ThreadId(1),
                    before: None,
                },
            ),
        ] {
            assert!(queue.borrow_mut().admit_page());
            pages_tx
                .send(Input::ReadPage {
                    epoch: 0,
                    request,
                    page,
                })
                .await
                .expect("request");
        }
        timeout(PATIENCE, async {
            while queue.borrow().frames_for_tests() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both pages queued without credit");
        turns_tx.send((None, "go".to_owned())).await.expect("turn");
        let mut states = Vec::new();
        while states.len() < 3 {
            let bytes = timeout(PATIENCE, control.recv())
                .await
                .expect("state arrives")
                .expect("open");
            let state: ControlToShell = frame::decode(&bytes[frame::HEADER_LEN..]).expect("state");
            if let ControlToShell::RunState { run, phase, .. } = state {
                states.push((run, phase));
            }
        }
        assert_eq!(
            states,
            [
                (RunId(2), Phase::Thinking),
                (RunId(2), Phase::Answering),
                (RunId(2), Phase::Done)
            ]
        );
        queue.borrow_mut().grant(DATA_WINDOW);
        ready.notify_one();
        read_pages_and_text(&mut client, &title, &history, &expected).await;
        turns_tx
            .send((None, "later".to_owned()))
            .await
            .expect("turn");
        let bytes = timeout(PATIENCE, control.recv())
            .await
            .expect("state arrives")
            .expect("open");
        let state: ControlToShell = frame::decode(&bytes[frame::HEADER_LEN..]).expect("state");
        assert!(matches!(
            state,
            ControlToShell::RunState {
                run: RunId(3),
                phase: Phase::Thinking,
                ..
            }
        ));
    };
    tokio::select! { ended = core => panic!("core ended {ended:?}"), ended = writer => panic!("writer ended {ended:?}"), () = client => {} }
    brain.offline(Input::ShellLost);
    brain.close().await;
}

async fn read_pages_and_text(
    client: &mut DuplexStream,
    title: &str,
    history: &str,
    expected: &str,
) {
    let mut got = String::new();
    let mut replies = Vec::new();
    loop {
        let next = timeout(PATIENCE, ipc::read::<DataToShell>(client, Pipe::Data))
            .await
            .expect("data arrives")
            .expect("read")
            .expect("open");
        match next {
            DataToShell::ThreadList {
                request,
                threads,
                next,
            } => {
                assert_eq!(threads.len(), 1);
                assert_eq!(threads[0].id, ThreadId(1));
                assert_eq!(threads[0].title, title);
                assert_eq!(next, None);
                replies.push(request);
            }
            DataToShell::Page {
                request,
                thread,
                events,
                before,
            } => {
                assert_eq!(thread, ThreadId(1));
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].text, history);
                assert_eq!(events[0].run, Some(RunId(1)));
                assert!(!events[0].cut);
                assert_eq!(before, None);
                replies.push(request);
            }
            DataToShell::AssistantText { run, delta } => {
                assert_eq!(run, RunId(2));
                got.push_str(&delta);
            }
            DataToShell::RunEnd { run } => {
                assert_eq!(run, RunId(2));
                break;
            }
            other @ DataToShell::StepCommit { .. } => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(got, expected);
    assert_eq!(replies, [10, 11]);
}

#[tokio::test]
async fn old_connection_page_success_and_failure_cannot_reach_a_new_connection() {
    let scratch = Scratch::new("page-epoch");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), shell) = duplex();
    drop(shell);
    let mut tap = Tap::from_env();
    assert_eq!(brain.serve(control, data, &mut tap).await, Ended::Lost);
    for reply in [
        Some(DataToShell::Page {
            request: 7,
            thread: ThreadId(999),
            events: Vec::new(),
            before: None,
        }),
        None,
    ] {
        brain
            .inputs_tx
            .send(Input::PageRead {
                epoch: 1,
                request: 7,
                reply,
            })
            .expect("late completion");
    }
    let ((control, data), mut shell) = duplex();
    let client = async {
        request(&mut shell, 7, threads()).await;
        assert_eq!(
            shell.data().await,
            DataToShell::ThreadList {
                request: 7,
                threads: Vec::new(),
                next: None
            }
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_page_read_failure_ends_the_connection_without_an_empty_reply() {
    let scratch = Scratch::new("page-failed");
    let store = Store::open(&scratch.db()).expect("store");
    store
        .conn_for_tests()
        .execute_batch("DROP TABLE threads")
        .expect("broken schema");
    let mut brain = Brain::new(
        store,
        (ThreadId(0), RunId(0)),
        Fixture::parse("").expect("fixture"),
    )
    .expect("brain");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        request(&mut shell, 7, threads()).await;
        let reply = timeout(
            PATIENCE,
            ipc::read::<DataToShell>(&mut shell.data_in, Pipe::Data),
        )
        .await
        .expect("connection ends")
        .expect("read");
        assert_eq!(reply, None);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}
