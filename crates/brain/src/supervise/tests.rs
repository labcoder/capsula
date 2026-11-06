use std::future::Future as _;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll, Waker};

use super::wire::CHUNK;
use super::*;

/// A kind with small requests and string replies, never started.
struct Test;

impl Kind for Test {
    const ARG: &'static str = "--test";
    const NAME: &'static str = "test";
    const MAX_REQUEST: usize = 1024;
    const MAX_REPLY: usize = 4 * 1024 * 1024;
    type Request = Vec<u8>;
    type Reply = String;
}

/// `message` as the frames a worker writes for it.
pub(super) fn frames(id: u64, message: &impl Serialize, max: usize) -> Vec<Frame> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, id, message, max).expect("written");
    let mut reader = bytes.as_slice();
    let mut frames = Vec::new();
    while let Some(next) = frame::read::<Frame>(&mut reader, Pipe::Data).expect("a frame") {
        frames.push(next);
    }
    frames
}

type Reply = oneshot::Receiver<Result<(String, Generation), WorkerLost>>;

fn pending(id: u64) -> (BTreeMap<u64, Pending<Test>>, Reply) {
    let (answer, receive) = oneshot::channel();
    (
        BTreeMap::from([(
            id,
            Pending::new(
                Instant::now() + Duration::from_secs(1),
                Destination::Single(answer),
            ),
        )]),
        receive,
    )
}

fn handle() -> (Handle<Test>, mpsc::Receiver<Command<Test>>) {
    let (commands, receive) = mpsc::channel(QUEUE);
    (Handle::new(commands), receive)
}

#[test]
fn identities_completion_trailing_and_gaps() {
    let pong = || frames(1, &"pong", Test::MAX_REPLY).remove(0);
    let (mut requests, mut receive) = pending(1);
    let mut other = pong();
    other.id = 2;
    assert_eq!(
        accept(&mut requests, other, Generation(3)),
        Err(WorkerLost::Lost)
    );
    assert_eq!(accept(&mut requests, pong(), Generation(3)), Ok(()));
    assert_eq!(
        receive.try_recv(),
        Ok(Ok(("pong".to_owned(), Generation(3))))
    );
    assert_eq!(
        accept(&mut requests, pong(), Generation(3)),
        Err(WorkerLost::Lost),
        "after the last"
    );
    let (mut requests, _) = pending(1);
    let mut trailing = pong();
    trailing.body.push(0);
    assert_eq!(
        accept(&mut requests, trailing, Generation(0)),
        Err(WorkerLost::Lost)
    );
    let (mut requests, _) = pending(1);
    let first = Frame {
        id: 1,
        chunk: 0,
        last: false,
        body: vec![0],
    };
    assert_eq!(accept(&mut requests, first, Generation(0)), Ok(()));
    let gap = Frame {
        id: 1,
        chunk: 2,
        last: true,
        body: vec![0],
    };
    assert_eq!(
        accept(&mut requests, gap, Generation(0)),
        Err(WorkerLost::Lost)
    );
}

#[test]
fn a_long_message_is_chunked_under_the_pipe_cap_and_reassembled() {
    let long = "x".repeat(2 * CHUNK + 10);
    let chunks = frames(7, &long, Test::MAX_REPLY);
    assert_eq!(chunks.len(), 3);
    for (number, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.id, 7);
        assert_eq!(chunk.chunk, u32::try_from(number).expect("small"));
        assert_eq!(chunk.last, number == 2);
        assert!(chunk.body.len() <= CHUNK);
    }
    let encoded = frame::encode(&chunks[0], Pipe::Data).expect("under the cap");
    assert!(encoded.len() <= Pipe::Data.max_payload() + frame::HEADER_LEN);
    let (mut requests, mut receive) = pending(7);
    for chunk in chunks {
        assert_eq!(accept(&mut requests, chunk, Generation(0)), Ok(()));
    }
    assert_eq!(receive.try_recv(), Ok(Ok((long, Generation(0)))));
}

#[test]
fn reassembly_cap_empty_gap_and_duplicate() {
    let chunk = |number, last, body| Frame {
        id: 1,
        chunk: number,
        last,
        body,
    };
    let mut assembly = Assembly::new(8);
    assert_eq!(assembly.push(chunk(0, false, vec![0; 8])), Ok(None));
    assert_eq!(
        assembly.push(chunk(1, true, vec![0])),
        Err(WorkerLost::Lost),
        "over the cap"
    );
    for frame in [chunk(0, true, vec![]), chunk(1, true, vec![0])] {
        assert_eq!(Assembly::new(8).push(frame), Err(WorkerLost::Lost));
    }
    let mut assembly = Assembly::new(8);
    assert_eq!(assembly.push(chunk(0, false, vec![1])), Ok(None));
    assert_eq!(
        assembly.push(chunk(0, true, vec![2])),
        Err(WorkerLost::Lost),
        "a repeat"
    );
    assert_eq!(assembly.push(chunk(1, true, vec![2])), Ok(Some(vec![1, 2])));
}

#[test]
fn a_message_over_its_kinds_cap_writes_nothing() {
    let mut written = Vec::new();
    assert!(matches!(
        write_message(&mut written, 1, &vec![0_u8; 2000], Test::MAX_REQUEST),
        Err(frame::FrameError::TooLarge { .. })
    ));
    assert!(written.is_empty());
}

#[test]
fn the_worker_reads_whole_requests_and_refuses_broken_ones() {
    let request = vec![9_u8; 900];
    let mut bytes = Vec::new();
    write_message(&mut bytes, 3, &request, Test::MAX_REQUEST).expect("written");
    write_message(&mut bytes, 4, &vec![1_u8], Test::MAX_REQUEST).expect("written");
    let mut input = bytes.as_slice();
    let read = |input: &mut &[u8]| read_request::<Vec<u8>>(input, Test::MAX_REQUEST);
    assert_eq!(read(&mut input).expect("first").map(|(id, _)| id), Some(3));
    assert_eq!(read(&mut input).expect("second"), Some((4, vec![1])));
    assert!(read(&mut input).expect("clean end").is_none());
    let mut mixed = Vec::new();
    for frame in [
        Frame {
            id: 5,
            chunk: 0,
            last: false,
            body: vec![1],
        },
        Frame {
            id: 6,
            chunk: 1,
            last: true,
            body: vec![1],
        },
    ] {
        frame::write(&mut mixed, &frame, Pipe::Data).expect("written");
    }
    assert!(matches!(
        read(&mut mixed.as_slice()),
        Err(frame::FrameError::Chunks)
    ));
    let truncated = &mixed[..=mixed.len() / 2];
    assert!(read(&mut &truncated[..]).is_err());
}

#[tokio::test]
async fn request_queue_accepts_sixteen_and_refuses_seventeen() {
    let (handle, mut receive) = handle();
    let until = Instant::now() + Duration::from_secs(60);
    let mut requests = Vec::new();
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..16 {
        let mut request = Box::pin(handle.request(vec![], until));
        assert_eq!(request.as_mut().poll(&mut context), Poll::Pending);
        requests.push(request);
    }
    assert_eq!(handle.request(vec![], until).await, Err(WorkerLost::Lost));
    assert_eq!(receive.len(), 16);
    for _ in 0..16 {
        let Destination::Single(answer) = receive.try_recv().expect("accepted request").answer
        else {
            panic!("a single request");
        };
        complete::<Test>(answer, Ok(("pong".to_owned(), Generation(1))));
    }
    for request in requests {
        assert_eq!(request.await, Ok(("pong".to_owned(), Generation(1))));
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_queued_behind_a_held_respawn_keeps_its_deadline() {
    let (handle, mut receive) = handle();
    let mut request =
        Box::pin(handle.request(vec![], Instant::now() + Duration::from_millis(1500)));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(request.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(receive.len(), 1);
    tokio::time::advance(Duration::from_millis(1499)).await;
    assert_eq!(request.as_mut().poll(&mut context), Poll::Pending);
    tokio::time::advance(Duration::from_millis(2)).await;
    assert_eq!(request.await, Err(WorkerLost::Lost));
    assert!(
        receive
            .try_recv()
            .expect("queued while respawning")
            .answer
            .is_closed()
    );
}

#[tokio::test(start_paused = true)]
async fn third_fourth_fifth_loss_and_window_boundary() {
    let mut history = VecDeque::new();
    let now = Instant::now();
    assert_eq!(backoff(&mut history, now), Duration::ZERO);
    assert_eq!(backoff(&mut history, now), Duration::ZERO);
    assert_eq!(backoff(&mut history, now), Duration::from_millis(200));
    assert_eq!(backoff(&mut history, now), Duration::from_secs(1));
    assert_eq!(backoff(&mut history, now), Duration::from_secs(5));
    tokio::time::advance(
        WINDOW
            .checked_sub(Duration::from_nanos(1))
            .expect("one ns under window"),
    )
    .await;
    assert_eq!(
        backoff(&mut history, Instant::now()),
        Duration::from_secs(5)
    );
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert_eq!(backoff(&mut history, Instant::now()), Duration::ZERO);
}

#[test]
fn a_17_chunk_message_reassembles_at_4_mib() {
    let most = Pipe::Data.max_payload() - 6;
    let sizes = [vec![most; 16], vec![Test::MAX_REPLY - 16 * most]].concat();
    assert_eq!(sizes.len(), 17);
    let mut assembly = Assembly::new(Test::MAX_REPLY);
    for (index, size) in sizes.iter().enumerate() {
        let last = index == 16;
        let sent = Frame {
            id: 1,
            chunk: u32::try_from(index).expect("17 chunks"),
            last,
            body: vec![42; *size],
        };
        let encoded = frame::encode(&sent, Pipe::Data).expect("within the frame cap");
        let read = frame::read::<Frame>(&mut encoded.as_slice(), Pipe::Data)
            .expect("read")
            .expect("frame");
        let done = assembly.push(read).expect("within the message cap");
        assert_eq!(
            done.map(|bytes| bytes.len()),
            last.then_some(Test::MAX_REPLY)
        );
    }
}

#[test]
fn each_malformed_frame_is_its_own_frame_error() {
    let over = (256 * 1024 + 1_u32).to_le_bytes().to_vec();
    let whole = Frame {
        id: 1,
        chunk: 0,
        last: true,
        body: vec![1; 64],
    };
    let at_cap = frame::encode(&whole, Pipe::Data).expect("frame");
    let truncated = at_cap[..at_cap.len() - 1].to_vec();
    let mistyped = frame::encode(&(1_u64, 0_u32), Pipe::Data).expect("not a frame");
    let read = |bytes: Vec<u8>| frame::read::<Frame>(&mut bytes.as_slice(), Pipe::Data);
    assert!(matches!(
        read(over),
        Err(frame::FrameError::TooLarge {
            len: 262_145,
            max: 262_144
        })
    ));
    assert!(matches!(read(truncated), Err(frame::FrameError::Truncated)));
    assert!(matches!(
        read(mistyped),
        Err(frame::FrameError::Malformed(_))
    ));
}

#[tokio::test]
async fn exit_confirmation_fails_requests_fast_and_reports_the_exit_before_the_loss() {
    for (confirmed, later) in [
        (false, WorkerLost::ExitUnconfirmed),
        (true, WorkerLost::Lost),
    ] {
        let (decide, exited) = blocking::channel();
        let (asked, asks) = blocking::channel();
        let (handle, mut commands) = handle();
        let (mut exits, mut losses) = (handle.exits(), handle.lost());
        let notices = handle.notices.clone();
        let held = move |timeout| {
            asked.send(timeout).expect("the test listens");
            exited.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
        };
        let confirming = tokio::spawn(async move {
            confirm_exit::<Test>(held, Generation(7), &mut commands, &notices).await
        });
        let request = handle.request(vec![], Instant::now() + Duration::from_secs(60));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), request).await,
            Ok(Err(WorkerLost::Lost)),
            "the runtime kept serving and the command failed at once"
        );
        assert_eq!(
            asks.recv_timeout(Duration::from_secs(1)),
            Ok(EXIT_CONFIRMATION)
        );
        assert!(!confirming.is_finished());
        assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
        assert_eq!(
            losses.try_recv(),
            Err(broadcast::error::TryRecvError::Empty),
            "no loss before the exit is settled"
        );
        decide.send(confirmed).expect("still waiting");
        assert_eq!(confirming.await.expect("joined"), confirmed);
        assert_eq!(
            exits.try_recv(),
            Ok(Exit {
                generation: Generation(7),
                confirmed
            })
        );
        assert_eq!(losses.try_recv(), Ok(Generation(7)));
        let until = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            handle.request(vec![], until).await,
            Err(later),
            "with the supervisor gone, only an unconfirmed exit refuses by type"
        );
        assert_eq!(handle.request_stream(vec![], until).err(), Some(later));
    }
}

#[tokio::test]
async fn an_unseen_exit_ends_the_supervisor_without_a_respawn_and_refuses_by_type() {
    let worker = Worker::<Test> {
        process: None,
        writes: blocking::sync_channel(QUEUE).0,
        reads: mpsc::channel(QUEUE).1,
    };
    let (commands, receive) = mpsc::channel(QUEUE);
    let handle = Handle::<Test>::new(commands);
    let (mut losses, mut exits) = (handle.lost(), handle.exits());
    let spawns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&spawns);
    let spawn = move || {
        counted.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::other("no worker in this test"))
    };
    let running = tokio::spawn(run(spawn, worker, receive, handle.notices.clone()));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), losses.recv()).await,
        Ok(Ok(Generation(0)))
    );
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: false
        }),
        "the exit is reported before its loss"
    );
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the supervisor ends")
        .expect("joined");
    assert_eq!(
        spawns.load(Ordering::Relaxed),
        0,
        "no worker starts beside one that may still run"
    );
    let until = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        handle.request(vec![], until).await,
        Err(WorkerLost::ExitUnconfirmed)
    );
}
