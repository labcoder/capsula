use super::tests::frames;
use super::*;

/// String replies; one that starts with "stage" doesn't end its request.
struct Staged;

impl Kind for Staged {
    const ARG: &'static str = "--staged";
    const NAME: &'static str = "staged";
    const MAX_REQUEST: usize = 1024;
    const MAX_REPLY: usize = 4 * 1024 * 1024;
    type Request = Vec<u8>;
    type Reply = String;

    fn is_terminal(reply: &String) -> bool {
        !reply.starts_with("stage")
    }
}

/// A kind whose single reply may pass a stream's total.
struct Large;

impl Kind for Large {
    const ARG: &'static str = "--large";
    const NAME: &'static str = "large";
    const MAX_REQUEST: usize = 1024;
    const MAX_REPLY: usize = MAX_STREAM_BYTES + 1;
    type Request = Vec<u8>;
    type Reply = Vec<u8>;
}

const SERVED: Generation = Generation(3);
const MIB_4: usize = 4 * 1024 * 1024;

fn far() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

/// One worker generation served over channels: the worker's frames go in on `reads`, the requests
/// it would read come out of `written`.
struct Fake {
    handle: Handle<Staged>,
    reads: mpsc::Sender<Option<Frame>>,
    written: blocking::Receiver<(u64, Vec<u8>)>,
    served: tokio::task::JoinHandle<Option<LossReason>>,
}

fn fake() -> Fake {
    let (commands, mut receive) = mpsc::channel(QUEUE);
    let (reads, mut replies) = mpsc::channel(QUEUE);
    let (writes, written) = blocking::sync_channel(QUEUE);
    let (handle, mut intents) = Handle::new(commands);
    let served = tokio::spawn(async move {
        let mut id = 0;
        serve::<Staged>(
            &mut replies,
            &writes,
            &mut receive,
            &mut id,
            SERVED,
            &mut intents,
        )
        .await
    });
    Fake {
        handle,
        reads,
        written,
        served,
    }
}

impl Fake {
    async fn request(&self) -> (u64, Vec<u8>) {
        for _ in 0..100 {
            if let Ok(request) = self.written.try_recv() {
                return request;
            }
            tokio::task::yield_now().await;
        }
        panic!("no request written");
    }

    async fn reply(&self, id: u64, reply: &str) {
        for frame in frames(id, &reply, Staged::MAX_REPLY) {
            self.reads.send(Some(frame)).await.expect("serving");
        }
    }

    /// Lets the supervisor take everything sent so far.
    async fn settle(&self) {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
}

fn stage(text: &str) -> (String, Generation) {
    (text.to_owned(), SERVED)
}

/// A stage whose serialized form is `len` bytes: a four-byte length, then the text.
fn sized(len: usize) -> String {
    let text = format!("stage{}", "x".repeat(len - 4 - 5));
    assert_eq!(frame::encode_payload(&text).expect("encoded").len(), len);
    text
}

/// A pending stream with room for every message it may receive.
type Streaming = (
    BTreeMap<u64, Pending<Staged>>,
    mpsc::Receiver<(String, Generation)>,
);

fn streaming() -> Streaming {
    let (stages, replies) = mpsc::channel(MAX_MESSAGES);
    let pending = Pending::new(far(), Destination::Stream(stages));
    (BTreeMap::from([(1, pending)]), replies)
}

fn push(pending: &mut BTreeMap<u64, Pending<Staged>>, reply: &str) -> Result<(), WorkerLost> {
    frames(1, &reply, Staged::MAX_REPLY)
        .into_iter()
        .try_for_each(|frame| accept(pending, frame, SERVED))
}

#[tokio::test(start_paused = true)]
async fn stages_arrive_in_order_and_only_the_terminal_ends_the_stream() {
    let fake = fake();
    let mut stream = fake
        .handle
        .request_stream(b"go".to_vec(), far())
        .expect("queued");
    assert_eq!(fake.request().await, (1, b"go".to_vec()));
    for reply in ["stage 1", "stage 2", "done"] {
        fake.reply(1, reply).await;
    }
    for reply in ["stage 1", "stage 2", "done"] {
        assert_eq!(stream.next().await, Some(Ok(stage(reply))));
    }
    assert_eq!(stream.next().await, None);
    fake.reply(1, "done").await;
    assert_eq!(
        fake.served.await.expect("served"),
        Some(LossReason::Malformed),
        "a second terminal"
    );
}

#[tokio::test(start_paused = true)]
async fn a_single_request_refuses_an_intermediate_reply() {
    let fake = fake();
    let worker = async {
        let (id, _) = fake.request().await;
        fake.reply(id, "stage 1").await;
    };
    let (reply, ()) = tokio::join!(fake.handle.request(vec![], far()), worker);
    assert_eq!(reply, Err(WorkerLost::Lost));
    assert_eq!(
        fake.served.await.expect("served"),
        Some(LossReason::Malformed)
    );
    assert_eq!(
        fake.handle.request(vec![2], far()).await,
        Err(WorkerLost::Lost)
    );
}

#[tokio::test(start_paused = true)]
async fn a_closed_stream_is_drained_without_losing_the_worker() {
    let fake = fake();
    let mut stream = fake.handle.request_stream(vec![1], far()).expect("queued");
    assert_eq!(fake.request().await, (1, vec![1]));
    fake.reply(1, "stage 1").await;
    assert_eq!(stream.next().await, Some(Ok(stage("stage 1"))));
    drop(stream);
    fake.reply(1, "stage 2").await;
    fake.reply(1, "done").await;
    fake.settle().await;
    assert!(
        !fake.served.is_finished(),
        "the closed stream lost the worker"
    );
    let worker = async {
        assert_eq!(fake.request().await, (2, vec![2]));
        fake.reply(2, "pong").await;
    };
    let (reply, ()) = tokio::join!(fake.handle.request(vec![2], far()), worker);
    assert_eq!(reply, Ok(("pong".to_owned(), SERVED)));
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_falls_behind_ends_without_losing_the_worker() {
    let fake = fake();
    let mut stream = fake.handle.request_stream(vec![1], far()).expect("queued");
    assert_eq!(fake.request().await.0, 1);
    for number in 1..=5 {
        fake.reply(1, &format!("stage {number}")).await;
    }
    fake.reply(1, "done").await;
    fake.settle().await;
    assert!(!fake.served.is_finished(), "a slow reader lost the worker");
    for number in 1..=4 {
        assert_eq!(
            stream.next().await,
            Some(Ok(stage(&format!("stage {number}"))))
        );
    }
    assert_eq!(
        stream.next().await,
        Some(Err(WorkerLost::Lost)),
        "the fifth"
    );
    assert_eq!(stream.next().await, None);
    let worker = async {
        assert_eq!(fake.request().await, (2, vec![2]));
        fake.reply(2, "pong").await;
    };
    let (reply, ()) = tokio::join!(fake.handle.request(vec![2], far()), worker);
    assert_eq!(reply, Ok(("pong".to_owned(), SERVED)));
}

#[test]
fn sixteen_messages_fit_and_a_seventeenth_loses_the_worker() {
    let (mut pending, replies) = streaming();
    for _ in 0..15 {
        assert_eq!(push(&mut pending, "stage"), Ok(()));
    }
    assert_eq!(push(&mut pending, "done"), Ok(()));
    assert_eq!(replies.len(), 16);
    assert!(pending.is_empty());
    let (mut pending, _replies) = streaming();
    for _ in 0..16 {
        assert_eq!(push(&mut pending, "stage"), Ok(()));
    }
    assert_eq!(push(&mut pending, "done"), Err(WorkerLost::Lost));
}

#[test]
fn twelve_mib_fit_across_messages_and_one_more_chunk_is_refused_unfinished() {
    let (mut pending, replies) = streaming();
    for len in [MIB_4, MIB_4, MIB_4 - 1] {
        assert_eq!(push(&mut pending, &sized(len)), Ok(()));
    }
    assert_eq!(push(&mut pending, ""), Ok(()), "the last byte of 12 MiB");
    assert_eq!(replies.len(), 4);
    assert!(pending.is_empty());
    let (mut pending, _replies) = streaming();
    for _ in 0..3 {
        assert_eq!(push(&mut pending, &sized(MIB_4)), Ok(()));
    }
    let partial = Frame {
        id: 1,
        chunk: 0,
        last: false,
        body: vec![0],
    };
    assert_eq!(accept(&mut pending, partial, SERVED), Err(WorkerLost::Lost));
}

#[test]
fn a_single_reply_keeps_its_kinds_cap_past_a_streams_total() {
    let body = vec![7_u8; Large::MAX_REPLY - 4];
    assert_eq!(
        frame::encode_payload(&body).expect("encoded").len(),
        Large::MAX_REPLY
    );
    let (answer, mut reply) = oneshot::channel();
    let pending = Pending::<Large>::new(far(), Destination::Single(answer));
    let mut pending = BTreeMap::from([(1, pending)]);
    for frame in frames(1, &body, Large::MAX_REPLY) {
        assert_eq!(accept(&mut pending, frame, SERVED), Ok(()));
    }
    assert_eq!(reply.try_recv(), Ok(Ok((body, SERVED))));
}

#[tokio::test(start_paused = true)]
async fn a_stream_keeps_its_absolute_deadline_through_its_stages() {
    let fake = fake();
    let mut late = fake
        .handle
        .request_stream(vec![9], Instant::now())
        .expect("queued");
    assert_eq!(late.next().await, Some(Err(WorkerLost::Lost)));
    let mut stream = fake
        .handle
        .request_stream(vec![1], Instant::now() + Duration::from_millis(1500))
        .expect("queued");
    assert_eq!(
        fake.request().await,
        (1, vec![1]),
        "the late one isn't sent"
    );
    tokio::time::advance(Duration::from_millis(1000)).await;
    fake.reply(1, "stage 1").await;
    assert_eq!(stream.next().await, Some(Ok(stage("stage 1"))));
    tokio::time::advance(Duration::from_millis(499)).await;
    fake.settle().await;
    assert!(!fake.served.is_finished());
    tokio::time::advance(Duration::from_millis(2)).await;
    fake.settle().await;
    assert!(fake.served.is_finished(), "a stage extended the deadline");
    assert_eq!(
        fake.served.await.expect("served"),
        Some(LossReason::Deadline)
    );
    assert_eq!(stream.next().await, Some(Err(WorkerLost::Lost)));
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_stream_ends_at_its_deadline() {
    let (commands, _held) = mpsc::channel(QUEUE);
    let (handle, _intents) = Handle::<Staged>::new(commands);
    let started = Instant::now();
    let mut stream = handle
        .request_stream(vec![], started + Duration::from_millis(1500))
        .expect("queued");
    assert_eq!(stream.next().await, Some(Err(WorkerLost::Lost)));
    assert_eq!(started.elapsed(), Duration::from_millis(1500));
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn a_lost_worker_ends_its_stream_after_the_stages_already_delivered() {
    let fake = fake();
    let mut stream = fake.handle.request_stream(vec![1], far()).expect("queued");
    assert_eq!(fake.request().await.0, 1);
    fake.reply(1, "stage 1").await;
    let unfinished = Frame {
        id: 1,
        chunk: 0,
        last: false,
        body: vec![0; 8],
    };
    fake.reads.send(Some(unfinished)).await.expect("serving");
    fake.reads.send(None).await.expect("serving");
    assert_eq!(fake.served.await.expect("served"), Some(LossReason::Ended));
    assert_eq!(stream.next().await, Some(Ok(stage("stage 1"))));
    assert_eq!(stream.next().await, Some(Err(WorkerLost::Lost)));
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn an_unwritable_stream_loses_the_worker() {
    let Fake {
        handle,
        reads: _reads,
        written,
        served,
    } = fake();
    drop(written);
    let mut stream = handle.request_stream(vec![1], far()).expect("queued");
    assert_eq!(served.await.expect("served"), Some(LossReason::Unwritable));
    assert_eq!(stream.next().await, Some(Err(WorkerLost::Lost)));
}

#[test]
fn a_streams_debug_shows_no_reply() {
    let (commands, mut receive) = mpsc::channel(QUEUE);
    let (handle, _intents) = Handle::<Staged>::new(commands);
    let stream = handle.request_stream(vec![], far()).expect("queued");
    let Destination::Stream(stages) = receive.try_recv().expect("queued").answer else {
        panic!("a stream");
    };
    let marker = "kumochi-sentinel-7f3a";
    stages
        .try_send((format!("stage {marker}"), SERVED))
        .expect("room");
    let printed = format!("{stream:?}");
    let bytes = format!("{:?}", marker.as_bytes());
    assert!(!printed.contains(marker));
    assert!(!printed.contains(bytes.trim_matches(['[', ']'])));
    assert_eq!(
        printed,
        r#"ReplyStream { kind: "staged", queued: 1, done: false, .. }"#
    );
}
