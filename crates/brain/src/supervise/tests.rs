use std::future::Future as _;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll, Waker};

use super::wire::CHUNK;
use super::*;

#[path = "dispatch_tests.rs"]
mod dispatch;

#[path = "resource_tests.rs"]
mod resources;

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

fn handle() -> (
    Handle<Test>,
    mpsc::Receiver<Command<Test>>,
    resource::Intents,
) {
    let (commands, receive) = mpsc::channel(QUEUE);
    let (handle, intents) = Handle::new(commands);
    (handle, receive, intents)
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
    let (handle, mut receive, _intents) = handle();
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
    let (handle, mut receive, _intents) = handle();
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

/// A process whose exit the test decides once the wait for it has begun.
struct Held {
    asked: blocking::Sender<Duration>,
    exited: blocking::Receiver<bool>,
    terminations: Arc<AtomicUsize>,
}

impl Process for Held {
    fn terminate(&self) -> io::Result<()> {
        self.terminations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        self.asked.send(timeout).expect("the test listens");
        Ok(self
            .exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(false))
    }
}

type Played = (
    blocking::Receiver<(u64, Vec<u8>)>,
    mpsc::Sender<Option<Frame>>,
);

/// A worker whose pipes the test plays: the requests written to it and the frames it answers.
fn staged(process: Option<Box<dyn Process>>) -> (Worker<Test>, Played) {
    let (writes, requests) = blocking::sync_channel(QUEUE);
    let (replies, reads) = mpsc::channel(QUEUE);
    let worker = Worker {
        process,
        writes,
        reads,
    };
    (worker, (requests, replies))
}

type Request = std::pin::Pin<Box<dyn Future<Output = Result<(String, Generation), WorkerLost>>>>;

/// A supervisor whose first worker is lost and whose exit wait is held, with one request sent
/// during that wait and the one replacement it may spawn.
struct Confirming {
    handle: Handle<Test>,
    running: tokio::task::JoinHandle<()>,
    decide: blocking::Sender<bool>,
    held: Request,
    next: Played,
    spawns: Arc<AtomicUsize>,
}

async fn confirming() -> Confirming {
    let (decide, exited) = blocking::channel();
    let (asked, asks) = blocking::channel();
    let held = Held {
        asked,
        exited,
        terminations: Arc::default(),
    };
    let (first, (_written, lose)) = staged(Some(Box::new(held)));
    let (next, played) = staged(None);
    let next = Arc::new(Mutex::new(Some(next)));
    let spawns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&spawns);
    let spawn = move || {
        counted.fetch_add(1, Ordering::Relaxed);
        next.lock()
            .expect("unpoisoned")
            .take()
            .ok_or_else(|| SpawnFailure::Failed(io::Error::other("one replacement")))
    };
    let (handle, receive, intents) = handle();
    let running = tokio::spawn(run(
        spawn,
        first,
        receive,
        handle.notices.clone(),
        intents,
        Generation(0),
    ));
    lose.send(None).await.expect("the supervisor reads");
    let asked = tokio::task::spawn_blocking(move || asks.recv_timeout(Duration::from_secs(5)));
    assert_eq!(asked.await.expect("joined"), Ok(EXIT_CONFIRMATION));
    let sender = handle.clone();
    let mut held: Request = Box::pin(async move {
        let until = Instant::now() + Duration::from_secs(60);
        sender.request(b"held".to_vec(), until).await
    });
    let mut context = Context::from_waker(Waker::noop());
    assert!(held.as_mut().poll(&mut context).is_pending(), "queued");
    tokio::task::yield_now().await;
    assert!(
        held.as_mut().poll(&mut context).is_pending(),
        "held through the exit wait, not failed"
    );
    Confirming {
        handle,
        running,
        decide,
        held,
        next: played,
        spawns,
    }
}

#[tokio::test]
async fn a_request_during_exit_confirmation_is_served_by_the_replacement_after_the_loss() {
    let rig = confirming().await;
    let (mut exits, mut losses) = (rig.handle.exits(), rig.handle.lost());
    assert_eq!(exits.try_recv(), Err(broadcast::error::TryRecvError::Empty));
    assert_eq!(
        losses.try_recv(),
        Err(broadcast::error::TryRecvError::Empty),
        "no loss before the exit is settled"
    );
    rig.decide.send(true).expect("still waiting");
    let (requests, replies) = rig.next;
    let written =
        tokio::task::spawn_blocking(move || requests.recv_timeout(Duration::from_secs(5)));
    let (id, body) = written.await.expect("joined").expect("the held request");
    assert_eq!(body, b"held");
    for frame in frames(id, &"served", Test::MAX_REPLY) {
        replies
            .send(Some(frame))
            .await
            .expect("the supervisor reads");
    }
    assert_eq!(rig.held.await, Ok(("served".to_owned(), Generation(1))));
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: true
        })
    );
    assert_eq!(losses.try_recv(), Ok(Generation(0)));
    assert_eq!(rig.spawns.load(Ordering::Relaxed), 1);
    rig.running.abort();
}

#[tokio::test]
async fn an_unseen_exit_refuses_held_and_later_requests_by_type_without_a_respawn() {
    let rig = confirming().await;
    let (mut exits, mut losses) = (rig.handle.exits(), rig.handle.lost());
    rig.decide.send(false).expect("still waiting");
    assert_eq!(rig.held.await, Err(WorkerLost::ExitUnconfirmed));
    tokio::time::timeout(Duration::from_secs(5), rig.running)
        .await
        .expect("the supervisor ends")
        .expect("joined");
    assert_eq!(
        rig.spawns.load(Ordering::Relaxed),
        0,
        "no worker starts beside one that may still run"
    );
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: false
        })
    );
    assert_eq!(losses.try_recv(), Ok(Generation(0)));
    let until = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        rig.handle.request(vec![], until).await,
        Err(WorkerLost::ExitUnconfirmed)
    );
    assert_eq!(
        rig.handle.request_stream(vec![], until).err(),
        Some(WorkerLost::ExitUnconfirmed)
    );
}

#[cfg(windows)]
#[tokio::test]
#[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
async fn a_live_worker_that_refuses_termination_is_reported_unconfirmed() {
    let mut child = {
        let _spawning = crate::platform::spawn_lock();
        std::process::Command::new("cmd")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("cmd starts")
    };
    let process = Helper::watching(child.id()).expect("a handle without termination rights");
    let (worker, (_written, lose)) = staged(Some(Box::new(process)));
    let (handle, receive, intents) = handle();
    let mut exits = handle.exits();
    let running = tokio::spawn(run(
        || -> Result<Worker<Test>, SpawnFailure> {
            Err(SpawnFailure::Failed(io::Error::other("no replacement")))
        },
        worker,
        receive,
        handle.notices.clone(),
        intents,
        Generation(0),
    ));
    lose.send(None).await.expect("the supervisor reads");
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the supervisor ends")
        .expect("joined");
    assert_eq!(
        exits.try_recv(),
        Ok(Exit {
            generation: Generation(0),
            confirmed: false
        })
    );
    let until = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        handle.request(vec![], until).await,
        Err(WorkerLost::ExitUnconfirmed)
    );
    assert!(child.try_wait().expect("cmd").is_none(), "still running");
    child.kill().expect("cmd ends");
    child.wait().expect("cmd ends");
}

#[test]
fn a_worker_whose_threads_fail_is_a_plain_failure_only_once_its_process_exits() {
    // Abandons a worker whose process exits when asked, or doesn't: the failure and the wait.
    let abandoned = |exits| {
        let (asked, asks) = blocking::channel();
        let (decide, exited) = blocking::channel();
        decide.send(exits).expect("decided");
        let terminations = Arc::new(AtomicUsize::new(0));
        let held = Held {
            asked,
            exited,
            terminations: Arc::clone(&terminations),
        };
        let (mut worker, _played) = staged(Some(Box::new(held)));
        let failure = worker.abandon(io::Error::other("no reader thread"));
        assert_eq!(terminations.load(Ordering::Relaxed), 1, "terminated");
        assert!(
            worker.process.is_none(),
            "dropping the worker ends nothing twice"
        );
        (failure, asks.try_recv().expect("the exit was waited for"))
    };
    let (failure, waited) = abandoned(true);
    assert!(
        matches!(failure, SpawnFailure::Failed(ref error) if error.to_string() == "no reader thread")
    );
    assert_eq!(waited, EXIT_CONFIRMATION);
    let (failure, waited) = abandoned(false);
    assert!(matches!(failure, SpawnFailure::ExitUnconfirmed));
    assert_eq!(waited, EXIT_CONFIRMATION);
}

/// A supervisor whose worker is lost with its exit seen, and whose one replacement start fails
/// with `failure` while a request waits for it. Returns that request's answer, then the answer to
/// one sent after the supervisor ended.
async fn refused_respawn(failure: SpawnFailure) -> [Result<(String, Generation), WorkerLost>; 2] {
    let (asked, _asks) = blocking::channel();
    let (decide, exited) = blocking::channel();
    decide.send(true).expect("the first worker exits");
    let held = Held {
        asked,
        exited,
        terminations: Arc::default(),
    };
    let (first, (_written, lose)) = staged(Some(Box::new(held)));
    let (started, starts) = blocking::channel();
    let (fail, failures) = blocking::channel();
    let failures = Arc::new(Mutex::new(failures));
    let spawn = move || -> Result<Worker<Test>, SpawnFailure> {
        started.send(()).expect("the test listens");
        Err(failures
            .lock()
            .expect("unpoisoned")
            .recv()
            .expect("the test decides"))
    };
    let (handle, receive, intents) = handle();
    let running = tokio::spawn(run(
        spawn,
        first,
        receive,
        handle.notices.clone(),
        intents,
        Generation(0),
    ));
    lose.send(None).await.expect("the supervisor reads");
    let starts = tokio::task::spawn_blocking(move || {
        starts
            .recv_timeout(Duration::from_secs(5))
            .expect("a replacement starts");
        starts
    })
    .await
    .expect("joined");
    let until = Instant::now() + Duration::from_secs(60);
    let sender = handle.clone();
    let mut queued: Request =
        Box::pin(async move { sender.request(b"held".to_vec(), until).await });
    let mut context = Context::from_waker(Waker::noop());
    assert!(queued.as_mut().poll(&mut context).is_pending(), "queued");
    fail.send(failure).expect("the start waits");
    let queued = queued.await;
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the supervisor ends")
        .expect("joined");
    assert_eq!(
        starts.try_recv(),
        Err(blocking::TryRecvError::Disconnected),
        "one start, then none"
    );
    [queued, handle.request(vec![], until).await]
}

#[tokio::test]
async fn a_replacement_left_running_by_its_failed_start_refuses_every_handle() {
    assert_eq!(
        refused_respawn(SpawnFailure::ExitUnconfirmed).await,
        [
            Err(WorkerLost::ExitUnconfirmed),
            Err(WorkerLost::ExitUnconfirmed)
        ]
    );
}

#[tokio::test]
async fn a_respawn_refused_by_a_shut_down_job_ends_the_supervisor_without_fatal() {
    assert_eq!(
        refused_respawn(SpawnFailure::ShutDown).await,
        [Err(WorkerLost::Lost), Err(WorkerLost::Lost)]
    );
}

#[cfg(windows)]
#[test]
fn a_start_into_a_shut_down_job_is_a_shutdown() {
    let job = Job::new().expect("job");
    job.shut_down().expect("an empty job ends");
    assert!(matches!(
        Worker::<Test>::spawn(Path::new("unused.exe"), &job),
        Err(SpawnFailure::ShutDown)
    ));
}

/// A waker that records what the exit channel holds at the instant a loss wakes it, which is
/// inside the loss's send, before the sender runs another line.
struct Peek(Mutex<(broadcast::Receiver<Exit>, Seen)>);

type Seen = Option<Result<Exit, broadcast::error::TryRecvError>>;

impl std::task::Wake for Peek {
    fn wake(self: Arc<Self>) {
        let mut peek = self.0.lock().expect("unpoisoned");
        let seen = peek.0.try_recv();
        peek.1.get_or_insert(seen);
    }
}

#[tokio::test]
async fn whoever_a_loss_wakes_already_sees_that_losss_exit() {
    let (handle, _commands, _intents) = handle();
    let peek = Arc::new(Peek(Mutex::new((handle.exits(), None))));
    let waker = Waker::from(Arc::clone(&peek));
    let mut losses = handle.lost();
    let mut lost = Box::pin(losses.recv());
    assert!(
        lost.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(confirm_exit::<Test>(|_| true, Generation(4), &handle.notices).await);
    assert_eq!(
        peek.0.lock().expect("unpoisoned").1,
        Some(Ok(Exit {
            generation: Generation(4),
            confirmed: true
        })),
        "the exit was broadcast before the loss woke anyone"
    );
    assert_eq!(lost.await, Ok(Generation(4)));
}
