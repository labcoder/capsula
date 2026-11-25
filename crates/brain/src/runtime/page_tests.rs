use kumochi_protocol::message::{DATA_WINDOW, MAX_PENDING_PAGES, PageOf, ThreadKind};

use super::*;
use crate::context::Origin;
use crate::coordinator::{BuildContext, BuildKey, Coordinator, Effect, StoreOp};
use crate::runtime::DATA_CAP;

/// The frames a connection opens with, before any run state.
fn preamble(frame: &[u8]) -> bool {
    matches!(
        frame::decode::<ControlToShell>(&frame[frame::HEADER_LEN..]).expect("decode"),
        ControlToShell::Scenes { .. }
            | ControlToShell::Settings { .. }
            | ControlToShell::Account(_)
            | ControlToShell::Connections(_)
            | ControlToShell::ScreenHolds(_)
    )
}

fn threads() -> PageOf {
    PageOf::Threads {
        search: None,
        after: None,
    }
}

#[tokio::test]
async fn admission_cancel_and_deadline_end_idle_credit_and_partial_writes() {
    for deadline in [false, true] {
        for phase in 0..3 {
            let queue = RefCell::new(DataQueue::new(if phase == 1 { 0 } else { 100 }));
            let key = queue.borrow_mut().admit_page(1, 7).unwrap();
            if phase != 0 {
                queue.borrow_mut().push_page(&key, vec![9; 16]);
            }
            if phase == 2 {
                queue
                    .borrow_mut()
                    .push(crate::runtime::Owner::Marker, vec![8; 4]);
            }
            let ready = Notify::new();
            let room = Notify::new();
            let (mut pipe, mut client) = tokio::io::duplex(1);
            let writer = super::super::write_data(&mut pipe, &queue, &ready, &room);
            let stop = async {
                if phase == 2 {
                    let mut first = [0];
                    client.read_exact(&mut first).await.unwrap();
                    assert_eq!(first, [9]);
                } else {
                    tokio::task::yield_now().await;
                }
                if deadline {
                    *key.control.test.time.lock().unwrap() = Some(key.control.end);
                } else {
                    key.control.cancel();
                }
            };
            let (ended, ()) = timeout(PATIENCE, async { tokio::join!(writer, stop) })
                .await
                .unwrap();
            assert_eq!(ended, Ended::Lost, "phase={phase}, deadline={deadline}");
            assert!(!queue.borrow().accepts_page(&key));
            drop(pipe);
            let mut remaining = Vec::new();
            client.read_to_end(&mut remaining).await.unwrap();
            if phase == 2 {
                assert!(remaining.len() <= 1);
                assert!(remaining.iter().all(|byte| *byte == 9));
                assert_eq!(queue.borrow().frames_for_tests(), 1);
            } else {
                assert_eq!(remaining, Vec::<u8>::new());
            }
        }
    }
}

fn resumed_page_write_stops_before_the_next_tick(
    stop: impl FnOnce(&crate::recall::control::Control),
) {
    use std::future::Future as _;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    struct Gated {
        open: Rc<Cell<bool>>,
        bytes: Rc<RefCell<Vec<u8>>>,
    }

    impl AsyncWrite for Gated {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let mut written = self.bytes.borrow_mut();
            if written.is_empty() {
                written.push(bytes[0]);
                Poll::Ready(Ok(1))
            } else if self.open.get() {
                written.extend_from_slice(bytes);
                Poll::Ready(Ok(bytes.len()))
            } else {
                Poll::Pending
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let queue = RefCell::new(DataQueue::new(100));
    let key = queue.borrow_mut().admit_page(1, 7).unwrap();
    queue.borrow_mut().push_page(&key, vec![9; 16]);
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Marker, vec![8; 4]);
    let open = Rc::new(Cell::new(false));
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let mut pipe = Gated {
        open: Rc::clone(&open),
        bytes: Rc::clone(&bytes),
    };
    let (ready, room) = (Notify::new(), Notify::new());
    let writer = super::super::write_data(&mut pipe, &queue, &ready, &room);
    tokio::pin!(writer);
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(writer.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(*bytes.borrow(), [9]);
    stop(&key.control);
    open.set(true);
    assert_eq!(writer.as_mut().poll(&mut context), Poll::Ready(Ended::Lost));
    assert_eq!(*bytes.borrow(), [9]);
    assert_eq!(queue.borrow().frames_for_tests(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelled_page_write_cannot_resume_before_the_next_tick() {
    resumed_page_write_stops_before_the_next_tick(crate::recall::control::Control::cancel);
}

#[tokio::test(start_paused = true)]
async fn expired_page_write_cannot_resume_before_the_next_tick() {
    resumed_page_write_stops_before_the_next_tick(|control| {
        *control.test.time.lock().unwrap() = Some(control.end);
    });
}

#[test]
fn outer_page_carriers_never_print_search_or_page_content() {
    let marker = "private-page-lifecycle-marker";
    let key = crate::coordinator::PageKey::for_tests(1, 7);
    let op = StoreOp::ReadPage {
        key: key.clone(),
        page: PageOf::Threads {
            search: Some(marker.into()),
            after: None,
        },
    };
    let reply = DataToShell::Page {
        request: 7,
        thread: ThreadId(1),
        events: vec![kumochi_protocol::message::PageEvent {
            id: kumochi_protocol::message::EventId(1),
            created_ms: 0,
            run: None,
            kind: kumochi_protocol::message::PageEventKind::User,
            text: marker.into(),
            cut: false,
        }],
        next: None,
    };
    let input = Input::PageRead {
        key: key.clone(),
        reply: reply.clone(),
    };
    let mut queue = DataQueue::new(100);
    let admitted = queue.admit_page(1, 7).unwrap();
    queue.push_page(&admitted, marker.as_bytes().to_vec());
    let effect = Effect::Page { key, reply };
    let debug = format!("{op:?} {input:?} {effect:?} {queue:?}");
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
}

#[tokio::test]
async fn admission_wakes_a_writer_already_blocked_on_an_older_frame() {
    let queue = RefCell::new(DataQueue::new(100));
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Marker, vec![9; 16]);
    let ready = Notify::new();
    let room = Notify::new();
    let (mut pipe, mut client) = tokio::io::duplex(1);
    let writer = super::super::write_data(&mut pipe, &queue, &ready, &room);
    let admit = async {
        let mut byte = [0];
        client.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [9]);
        let key = queue.borrow_mut().admit_page(1, 7).unwrap();
        key.control.cancel();
        ready.notify_one();
    };
    let (ended, ()) = timeout(PATIENCE, async { tokio::join!(writer, admit) })
        .await
        .unwrap();
    assert_eq!(ended, Ended::Lost);
}

#[tokio::test]
async fn session_eof_cancels_a_page_still_waiting_at_the_store_barrier() {
    let scratch = Scratch::new("page-session-end");
    let mut brain = test_support::brain(&scratch, "");
    let mut seen = brain.observe();
    let release = brain.hold_store(|op| matches!(op, StoreOp::BeginTurn { .. }));
    assert!(brain.store.as_ref().unwrap().send(StoreOp::BeginTurn {
        thread: ThreadId(2),
        title: Some("task".into()),
        run: RunId(1),
        text: "text".into(),
        screen: None,
    }));
    while !matches!(
        seen.recv().await,
        Some(crate::runtime::Seen::StoreOpStarted(
            StoreOp::BeginTurn { .. }
        ))
    ) {}
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        request(&mut shell, 7, threads()).await;
        let key = loop {
            if let Some(crate::runtime::Seen::StoreOpStarted(StoreOp::ReadPage { key, .. })) =
                seen.recv().await
            {
                break key;
            }
        };
        while seen.recv().await != Some(crate::runtime::Seen::ReadWaiting) {}
        drop(shell);
        key
    };
    let (ended, key) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert_eq!(key.control.check(), Err(crate::recall::Error::Ended));
    release.send(()).unwrap();
    brain.close().await;
}

#[tokio::test]
async fn held_page_output_retains_its_original_admission_end_and_owner() {
    for deadline in [false, true] {
        let scratch = Scratch::new("page-held-end");
        let mut brain = test_support::brain(&scratch, "");
        let mut seen = brain.observe();
        let queue = exhausted_queue();
        queue
            .borrow_mut()
            .push(crate::runtime::Owner::Run(RunId(99)), vec![0; DATA_CAP]);
        let key = queue.borrow_mut().admit_page(0, 7).unwrap();
        let ready = Notify::new();
        let room = Notify::new();
        let (out, _control) = mpsc::channel(64);
        let (_controls_tx, mut controls) = mpsc::channel(64);
        let (_turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
        let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
        pages_tx
            .try_send(Input::ReadPage {
                key: key.clone(),
                page: threads(),
            })
            .unwrap();
        let mut tap = Tap::from_env();
        let core = brain.core(
            &mut controls,
            &mut turns,
            &mut pages,
            &room,
            Link::new(&out, &queue, &ready, &mut tap),
        );
        let (mut output, _client) = tokio::io::duplex(1);
        let writer = super::super::write_data(&mut output, &queue, &ready, &room);
        let stop = async {
            while seen.recv().await != Some(crate::runtime::Seen::PageHeld) {}
            assert_eq!(queue.borrow().frames_for_tests(), 1);
            assert_eq!(key.control.check(), Ok(()));
            if deadline {
                *key.control.test.time.lock().unwrap() = Some(key.control.end);
            } else {
                key.control.cancel();
            }
        };
        let serving = async {
            tokio::select! { ended = core => ended, ended = writer => ended }
        };
        let (ended, ()) = Box::pin(timeout(PATIENCE, async { tokio::join!(serving, stop) }))
            .await
            .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert!(queue.borrow().has_reads());
        queue.borrow_mut().end_reads();
        assert_eq!(key.control.check(), Err(crate::recall::Error::Ended));
        brain.close().await;
    }
}

async fn queue_turn(turns: &mpsc::Sender<crate::runtime::Turn>, turn: u32, text: &str) {
    let turn = crate::coordinator::Input::Turn {
        turn,
        thread: None,
        text: text.into(),
    };
    turns.send(turn).await.expect("turn");
}

/// The main thread a reply to request 7 lists as a fresh store's only row.
fn only_kumo(reply: DataToShell) -> ThreadId {
    let DataToShell::ThreadList {
        request: 7,
        threads,
        next: None,
    } = reply
    else {
        panic!("request 7's list, got {reply:?}")
    };
    let [row] = threads.as_slice() else {
        panic!("one row, got {threads:?}")
    };
    assert_eq!((row.kind, row.title.as_str()), (ThreadKind::Main, ""));
    row.id
}

async fn request(shell: &mut Shell<DuplexStream>, request: u32, page: PageOf) {
    let frame =
        frame::encode(&DataToBrain::PageRequest { request, page }, Pipe::Data).expect("encodes");
    shell.data_out.write_all(&frame).await.expect("sent");
}

fn exhausted_queue() -> RefCell<DataQueue> {
    let mut queue = DataQueue::new(window());
    queue.push(crate::runtime::Owner::Run(RunId(99)), vec![0; window()]);
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
    let mut coordinator =
        Coordinator::ready_for_tests(ThreadId(1), ThreadId(1), RunId(0), Vec::new());
    coordinator
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "go".into(),
        })
        .expect("turn");
    let page = threads();
    assert_eq!(
        coordinator.handle(Input::ReadPage {
            key: crate::coordinator::PageKey::for_tests(1, 3),
            page: page.clone()
        }),
        Ok(vec![Effect::Store(StoreOp::ReadPage {
            key: crate::coordinator::PageKey::for_tests(1, 3),
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
            key: crate::coordinator::PageKey::for_tests(1, 3),
            reply: reply.clone()
        }),
        Ok(vec![Effect::Page {
            key: crate::coordinator::PageKey::for_tests(1, 3),
            reply
        }])
    );
    assert_eq!(coordinator.current_run(), Some(RunId(1)));
    assert_eq!(coordinator.ending_runs(), 0);
    assert_eq!(
        coordinator.handle(Input::TurnStored { run: RunId(1) }),
        Ok(vec![Effect::Store(StoreOp::BuildContext(BuildContext {
            control: crate::recall::control::Control::new(
                std::time::Instant::now() + crate::coordinator::WALL_CLOCK
            ),
            key: BuildKey {
                run: RunId(1),
                round: 1,
                attempt: 1,
            },
            thread: ThreadId(2),
            origin: Origin::Task(ThreadId(2)),
            snapshot: None,
            first: true,
            arrived: crate::coordinator::Arrived::of(
                kumochi_protocol::message::ConnectionKind::ChatGpt,
                crate::context::ConnectionTag(0),
            ),
            drawings: crate::drawing::Digest::default(),
            picture_models: Vec::new(),
            key_results: Vec::new(),
        }))])
    );
}

#[tokio::test]
async fn staged_issue_data_is_dropped_without_blocking_credit() {
    use kumochi_protocol::message::{DraftId, IssueBody, IssueTitle};

    let queue = exhausted_queue();
    let ready = Notify::new();
    let (turns_tx, mut turns) = mpsc::channel(1);
    let (pages_tx, mut pages) = mpsc::channel(1);
    let (mut input, mut shell) = tokio::io::duplex(DUPLEX_BUFFER);
    let reader = super::super::read_data(
        &mut input, &turns_tx, &pages_tx, &queue, &ready, 0, None, None,
    );
    let writer = async {
        for message in [
            DataToBrain::IssueEdit {
                draft: DraftId(9),
                title: IssueTitle::new("Fix layout").expect("title"),
                body: IssueBody::new("Private details").expect("body"),
                repo: None,
            },
            DataToBrain::Credit { bytes: 17 },
        ] {
            let bytes = frame::encode(&message, Pipe::Data).expect("frame");
            shell.write_all(&bytes).await.expect("sent");
        }
        shell.shutdown().await.expect("EOF");
    };
    let ended = timeout(PATIENCE, async { tokio::join!(reader, writer).0 })
        .await
        .expect("reader finishes");
    assert_eq!(ended, Ended::Lost);
    assert_eq!(turns.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(pages.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Marker, vec![7; 17]);
    assert_eq!(
        queue.borrow_mut().pop(std::time::Instant::now()),
        Some(vec![7; 17])
    );
}

#[tokio::test]
async fn uncorrelated_cards_are_a_protocol_error_not_an_ignored_read() {
    let queue = exhausted_queue();
    let ready = Notify::new();
    let (turns, _turns) = mpsc::channel(1);
    let (pages, mut requests) = mpsc::channel(1);
    let (mut input, mut shell) = tokio::io::duplex(DUPLEX_BUFFER);
    let reader = super::super::read_data(&mut input, &turns, &pages, &queue, &ready, 1, None, None);
    let writer = async {
        let bytes = frame::encode(
            &DataToBrain::IssueCards {
                drafts: kumochi_scene::text::Bounded::default(),
            },
            Pipe::Data,
        )
        .unwrap();
        shell.write_all(&bytes).await.unwrap();
    };
    let (ended, ()) = timeout(PATIENCE, async { tokio::join!(reader, writer) })
        .await
        .unwrap();
    assert_eq!(ended, Ended::Broken("uncorrelated card request"));
    assert_eq!(requests.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(!queue.borrow().has_reads());
}

#[tokio::test]
async fn correlated_card_admission_never_holds_up_credit() {
    use kumochi_protocol::message::DraftId;
    let queue = exhausted_queue();
    let ready = Notify::new();
    let (turns, _turns) = mpsc::channel(1);
    let (pages, mut requests) = mpsc::channel(1);
    let (mut input, mut shell) = tokio::io::duplex(DUPLEX_BUFFER);
    let reader = super::super::read_data(&mut input, &turns, &pages, &queue, &ready, 3, None, None);
    let writer = async {
        for message in [
            DataToBrain::IssueCardsRequest {
                request: 7,
                drafts: kumochi_scene::text::Bounded::new(vec![DraftId(9); 16]).unwrap(),
            },
            DataToBrain::Credit { bytes: 17 },
        ] {
            shell
                .write_all(&frame::encode(&message, Pipe::Data).unwrap())
                .await
                .unwrap();
        }
        shell.shutdown().await.unwrap();
    };
    let (ended, ()) = timeout(PATIENCE, async { tokio::join!(reader, writer) })
        .await
        .unwrap();
    assert_eq!(ended, Ended::Lost);
    let Input::ReadCards { cards } = requests.try_recv().unwrap() else {
        panic!("card read");
    };
    assert_eq!(cards.key.request, 7);
    assert_eq!(cards.key.epoch, 3);
    assert_eq!(
        cards
            .sources
            .iter()
            .map(|source| source.draft)
            .collect::<Vec<_>>(),
        [DraftId(9)]
    );
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Marker, vec![7; 17]);
    assert_eq!(
        queue.borrow_mut().pop(std::time::Instant::now()),
        Some(vec![7; 17])
    );
    queue.borrow_mut().wrote();
    assert!(queue.borrow().has_reads());
    queue.borrow_mut().end_reads();
    assert_eq!(cards.key.control.check(), Err(crate::recall::Error::Ended));
}

#[tokio::test]
async fn deleted_card_content_is_removed_before_a_completion_is_held() {
    use kumochi_protocol::message::{
        ActionState, ContentHash, DraftId, IssueBody, IssueCard, IssueTitle, StateVersion,
    };
    use std::sync::Arc;
    let scratch = Scratch::new("card-held-prune");
    let mut brain = test_support::brain(&scratch, "");
    let queue = exhausted_queue();
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Marker, vec![0; DATA_CAP]);
    let cards = queue
        .borrow_mut()
        .admit_cards(
            brain.epoch,
            7,
            &kumochi_scene::text::Bounded::new(vec![DraftId(9)]).unwrap(),
        )
        .unwrap();
    let source = Arc::clone(&cards.sources[0]);
    assert!(source.bind(ThreadId(2), crate::store::Generation(0)));
    let row = crate::coordinator::CardRead {
        card: IssueCard {
            draft: DraftId(9),
            thread: ThreadId(2),
            repo: None,
            account: None,
            title: IssueTitle::new("private-card-marker").unwrap(),
            body: IssueBody::new("private-card-marker").unwrap(),
            hash: ContentHash([0; 32]),
            approval: None,
            version: StateVersion(1),
            state: ActionState::Draft,
        },
        generation: crate::store::Generation(0),
        source: Arc::clone(&source),
    };
    source.delete();
    let (out, _control) = mpsc::channel(64);
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert_eq!(
        brain.admit_page(
            Input::CardsRead {
                cards,
                reply: Ok(vec![row])
            },
            &mut link
        ),
        None
    );
    let Some(Input::CardsRead {
        reply: Ok(rows), ..
    }) = link.held_pages.pop_front()
    else {
        panic!("held batch");
    };
    assert_eq!(rows, []);
    assert!(queue.borrow().has_reads());
    drop(link);
    queue.borrow_mut().end_reads();
    brain.close().await;
}

#[tokio::test]
async fn a_repeated_active_page_id_breaks_before_a_second_read_is_forwarded() {
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let (turns_tx, _turns) = mpsc::channel(1);
    let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
    let (mut input, mut client) = tokio::io::duplex(DUPLEX_BUFFER);
    let reader = super::super::read_data(
        &mut input, &turns_tx, &pages_tx, &queue, &ready, 9, None, None,
    );
    let send = async {
        let request = DataToBrain::PageRequest {
            request: 7,
            page: threads(),
        };
        let bytes = frame::encode(&request, Pipe::Data).unwrap();
        client.write_all(&bytes).await.unwrap();
        client.write_all(&bytes).await.unwrap();
    };
    let (ended, ()) = timeout(PATIENCE, async { tokio::join!(reader, send) })
        .await
        .unwrap();
    assert_eq!(ended, Ended::Broken("too many unanswered page requests"));
    let Input::ReadPage { key, page } = pages.try_recv().unwrap() else {
        panic!("one page")
    };
    assert_eq!((key.epoch, key.request, key.token), (9, 7, 1));
    assert_eq!(page, threads());
    assert_eq!(pages.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    queue.borrow_mut().end_reads();
    assert_eq!(key.control.check(), Err(crate::recall::Error::Ended));
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
    let link = Link::new(&out, &queue, &ready, &mut tap);
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let reader = super::super::read_data(
        &mut input, &turns_tx, &pages_tx, &queue, &ready, 0, None, None,
    );
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
    let link = Link::new(&out, &queue, &ready, &mut tap);
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let writer = super::super::write_data(&mut output, &queue, &ready, &room);
    let client = async {
        let key = queue.borrow_mut().admit_page(0, 7).expect("admitted");
        pages_tx
            .send(Input::ReadPage {
                key,
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
        assert_eq!(
            queue.borrow().frames_for_tests(),
            1,
            "without credit the reply stays queued"
        );
        queue_turn(&turns_tx, 1, "go").await;
        let mut bytes = Vec::new();
        for _ in 0..6 {
            bytes = timeout(PATIENCE, control.recv())
                .await
                .expect("state")
                .expect("open");
            if !preamble(&bytes) {
                break;
            }
        }
        assert!(matches!(
            frame::decode::<ControlToShell>(&bytes[frame::HEADER_LEN..]).expect("decode"),
            ControlToShell::RunState {
                origin: Some(kumochi_protocol::message::RunOrigin::Typed { turn: 1 }),
                run: RunId(1),
                thread: ThreadId(2),
                phase: Phase::Thinking,
                ..
            }
        ));
        controls_tx
            .send((
                ControlToBrain::Stop {
                    run: RunId(1),
                    revealed: Vec::new(),
                },
                0,
            ))
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
                next: None
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
    let link = Link::new(&out, &queue, &ready, &mut tap);
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
            let key = queue.borrow_mut().admit_page(0, request).expect("admitted");
            pages_tx
                .send(Input::ReadPage { key, page })
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
        queue_turn(&turns_tx, 1, "go").await;
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
        queue_turn(&turns_tx, 2, "later").await;
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
                // The main thread, created after this task, then the task.
                let kinds: Vec<_> = threads.iter().map(|row| (row.id, row.kind)).collect();
                assert_eq!(
                    kinds,
                    [
                        (ThreadId(2), ThreadKind::Main),
                        (ThreadId(1), ThreadKind::Thread)
                    ]
                );
                assert_eq!(threads[1].title, title);
                assert_eq!(next, None);
                replies.push(request);
            }
            DataToShell::Page {
                request,
                thread,
                events,
                next: before,
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
            other @ (DataToShell::StepCommit { .. }
            | DataToShell::PageFailed { .. }
            | DataToShell::SceneSnapshot { .. }
            | DataToShell::ScenesRemoved { .. }
            | DataToShell::ScenesReplaced { .. }
            | DataToShell::SceneBegin { .. }
            | DataToShell::SceneStepCommit { .. }
            | DataToShell::SceneSnapshotV1 { .. }
            | DataToShell::SceneCutApplied { .. }
            | DataToShell::SceneEnd { .. }
            | DataToShell::Models { .. }
            | DataToShell::IssueCard(_)
            | DataToShell::IssueCardRead { .. }
            | DataToShell::IssueCardsDone { .. }
            | DataToShell::IssueReview { .. }
            | DataToShell::Transcript(_)
            | DataToShell::SegmentQueued { .. }
            | DataToShell::SpeechPcm(_)
            | DataToShell::HoldWords { .. }
            | DataToShell::HoldStored { .. }
            | DataToShell::EventsChanged { .. }) => {
                panic!("unexpected {other:?}")
            }
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
        DataToShell::Page {
            request: 7,
            thread: ThreadId(999),
            events: Vec::new(),
            next: None,
        },
        DataToShell::PageFailed { request: 7 },
    ] {
        brain
            .inputs_tx
            .send(Input::PageRead {
                key: crate::coordinator::PageKey::for_tests(1, 7),
                reply,
            })
            .expect("late completion");
    }
    let ((control, data), mut shell) = duplex();
    let client = async {
        request(&mut shell, 7, threads()).await;
        assert_eq!(only_kumo(shell.data().await), ThreadId(1));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_page_read_failure_answers_page_failed_and_keeps_the_connection() {
    let scratch = Scratch::new("page-failed");
    let store = Store::open(&scratch.db()).expect("store");
    store
        .conn_for_tests()
        .execute_batch("DROP TABLE threads")
        .expect("broken schema");
    let custody = crate::test_support::custody(&store);
    let mut brain = Brain::new(
        store,
        custody,
        ThreadId(1),
        (ThreadId(1), RunId(0)),
        Vec::new(),
        crate::runtime::Selected::Fixture(Box::new(Fixture::parse("").expect("fixture"))),
        scratch.config(),
        crate::test_support::accounts(&scratch),
    )
    .expect("brain");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        for id in [7, 8] {
            request(&mut shell, id, threads()).await;
            let reply = timeout(
                PATIENCE,
                ipc::read::<DataToShell>(&mut shell.data_in, Pipe::Data),
            )
            .await
            .expect("answered")
            .expect("read");
            assert_eq!(reply, Some(DataToShell::PageFailed { request: id }));
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost, "only the shell leaving ends it");
    brain.close().await;
}

#[tokio::test]
async fn a_reply_that_lands_while_the_queue_is_full_waits_for_room() {
    let scratch = Scratch::new("page-held");
    let mut brain = test_support::brain(&scratch, "");
    let mut observed = brain.observe();
    let queue = RefCell::new(DataQueue::new(window()));
    queue
        .borrow_mut()
        .push(crate::runtime::Owner::Run(RunId(99)), vec![0; DATA_CAP]);
    let (ready, room) = (Notify::new(), Notify::new());
    let (out, _control) = mpsc::channel(64);
    let (_controls_tx, mut controls) = mpsc::channel(64);
    let (_turns_tx, mut turns) = mpsc::channel(MAX_PENDING_TURNS);
    let (pages_tx, mut pages) = mpsc::channel(MAX_PENDING_PAGES);
    let mut tap = Tap::from_env();
    let link = Link::new(&out, &queue, &ready, &mut tap);
    let core = brain.core(&mut controls, &mut turns, &mut pages, &room, link);
    let client = async {
        let key = queue.borrow_mut().admit_page(0, 7).expect("admitted");
        pages_tx
            .send(Input::ReadPage {
                key,
                page: threads(),
            })
            .await
            .expect("request");
        timeout(PATIENCE, async {
            while observed.recv().await != Some(crate::runtime::Seen::PageHeld) {}
        })
        .await
        .expect("the reply is held");
        assert_eq!(
            queue.borrow().frames_for_tests(),
            1,
            "nothing else is queued"
        );
        queue.borrow_mut().revoke(RunId(99));
        room.notify_one();
        timeout(PATIENCE, async {
            while queue.borrow().frames_for_tests() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the held reply is queued once there's room");
        let frame = queue
            .borrow_mut()
            .pop(std::time::Instant::now())
            .expect("credit for the reply");
        let reply = frame::decode::<DataToShell>(&frame[frame::HEADER_LEN..]).expect("decode");
        assert_eq!(only_kumo(reply), ThreadId(1));
    };
    tokio::select! { ended = core => panic!("core ended {ended:?}"), () = client => {} }
    brain.close().await;
}

/// A page read the shell sends after a picture's sent marker waits for the held mark, then shows
/// the still active run's turn as sent; an active pending turn shows no picture until then.
#[tokio::test]
async fn a_page_read_after_the_sent_marker_waits_for_the_mark_and_shows_the_active_turn_sent() {
    use kumochi_protocol::message::{HeldScreen, PageEventKind};
    let scratch = Scratch::new("page-after-marker");
    let mut brain = test_support::brain(&scratch, "");
    let mut seen = brain.observe();
    assert!(brain.store.as_ref().unwrap().send(StoreOp::BeginTurn {
        thread: ThreadId(2),
        title: Some("task".into()),
        run: RunId(1),
        text: "text".into(),
        screen: Some(HeldScreen::Attached),
    }));
    let release = brain.hold_store(|op| matches!(op, StoreOp::SentScreen { .. }));
    assert!(
        brain
            .store
            .as_ref()
            .unwrap()
            .send(StoreOp::SentScreen { run: RunId(1) })
    );
    while !matches!(
        seen.recv().await,
        Some(crate::runtime::Seen::StoreOpStarted(
            StoreOp::SentScreen { .. }
        ))
    ) {}
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        request(
            &mut shell,
            7,
            PageOf::Events {
                thread: ThreadId(2),
                before: None,
            },
        )
        .await;
        timeout(PATIENCE, async {
            while seen.recv().await != Some(crate::runtime::Seen::ReadWaiting) {}
        })
        .await
        .expect("the read waits for the store");
        release.send(()).unwrap();
        let reply = timeout(
            PATIENCE,
            ipc::read::<DataToShell>(&mut shell.data_in, Pipe::Data),
        )
        .await
        .expect("answered")
        .expect("read");
        let Some(DataToShell::Page { events, .. }) = reply else {
            panic!("a page");
        };
        assert!(
            events.iter().any(|event| event.kind
                == PageEventKind::HeldTurn {
                    screen: HeldScreen::Attached
                }),
            "the active turn reads as sent"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

/// The events marker goes only on its hold's own connection.
#[tokio::test]
async fn the_events_marker_goes_only_on_its_holds_connection() {
    use kumochi_protocol::message::GestureId;
    for (stale, frames) in [(false, 1), (true, 0)] {
        let scratch = Scratch::new("page-marker-epoch");
        let mut brain = test_support::brain(&scratch, "");
        let queue = RefCell::new(DataQueue::new(100));
        let ready = Notify::new();
        let (out, _control) = mpsc::channel(64);
        let mut tap = Tap::from_env();
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        let hold = crate::coordinator::HoldKey {
            epoch: brain.epoch + u64::from(stale),
            gesture: GestureId(1),
        };
        brain
            .apply(
                vec![Effect::EventsChanged {
                    hold,
                    thread: ThreadId(2),
                }],
                Some(&mut link),
            )
            .expect("room");
        assert_eq!(queue.borrow().frames_for_tests(), frames, "stale {stale}");
        brain.close().await;
    }
}
