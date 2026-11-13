use std::sync::atomic::AtomicUsize;

use super::*;

pub(crate) fn retirement() -> (Retirement, resource::Intents) {
    Retirement::channel()
}

struct Held {
    terminations: Arc<AtomicUsize>,
    waiting: blocking::Sender<Duration>,
    confirm: blocking::Receiver<bool>,
}

impl Process for Held {
    fn terminate(&self) -> io::Result<()> {
        self.terminations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        self.waiting.send(timeout).expect("confirmation observer");
        Ok(self
            .confirm
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(false))
    }
}

pub(crate) struct Rig<K: Kind> {
    pub(crate) handle: Handle<K>,
    pub(crate) actor: tokio::task::JoinHandle<()>,
    requests: Option<blocking::Receiver<(u64, K::Request)>>,
    replies: mpsc::Sender<Option<Frame>>,
    waiting: Option<blocking::Receiver<Duration>>,
    pub(crate) confirm: blocking::Sender<bool>,
    pub(crate) terminations: Arc<AtomicUsize>,
    pub(crate) spawns: Arc<AtomicUsize>,
}

pub(crate) fn start<K: Kind>() -> Rig<K> {
    start_at(Generation(0))
}

pub(crate) fn start_at<K: Kind>(generation: Generation) -> Rig<K> {
    let (writes, requests) = blocking::sync_channel(QUEUE);
    let (replies, reads) = mpsc::channel(QUEUE);
    let (waiting, observed) = blocking::channel();
    let (confirm, confirmed) = blocking::channel();
    let terminations = Arc::new(AtomicUsize::new(0));
    let worker = Worker {
        writes,
        reads,
        process: Some(Box::new(Held {
            terminations: Arc::clone(&terminations),
            waiting,
            confirm: confirmed,
        })),
    };
    let (commands, receive) = mpsc::channel(QUEUE);
    let (handle, intents) = Handle::new(commands);
    let spawns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&spawns);
    let actor = tokio::spawn(run(
        move || {
            counted.fetch_add(1, Ordering::Relaxed);
            Err(SpawnFailure::Failed(io::Error::other(
                "no replacement in this fixture",
            )))
        },
        worker,
        receive,
        handle.notices.clone(),
        intents,
        generation,
    ));
    Rig {
        handle,
        actor,
        requests: Some(requests),
        replies,
        waiting: Some(observed),
        confirm,
        terminations,
        spawns,
    }
}

pub(crate) async fn receive_request<T: Send + 'static>(
    channel: blocking::Receiver<T>,
) -> (T, blocking::Receiver<T>) {
    tokio::task::spawn_blocking(move || {
        let value = channel
            .recv_timeout(Duration::from_secs(5))
            .expect("fixture phase");
        (value, channel)
    })
    .await
    .expect("phase waiter joins")
}

impl<K: Kind> Rig<K> {
    pub(crate) async fn next(&mut self) -> (u64, K::Request) {
        let (value, channel) =
            receive_request(self.requests.take().expect("request channel")).await;
        self.requests = Some(channel);
        value
    }

    pub(crate) async fn held(&mut self) {
        let (value, channel) =
            receive_request(self.waiting.take().expect("confirmation channel")).await;
        self.waiting = Some(channel);
        assert_eq!(value, EXIT_CONFIRMATION);
        assert_eq!(self.terminations.load(Ordering::Relaxed), 1);
        assert_eq!(self.spawns.load(Ordering::Relaxed), 0);
    }

    pub(crate) fn no_more_requests(&self) {
        assert!(matches!(
            self.requests.as_ref().expect("request receiver").try_recv(),
            Err(blocking::TryRecvError::Disconnected)
        ));
    }

    pub(crate) async fn reply(&self, id: u64, value: &K::Reply)
    where
        K::Reply: Serialize,
    {
        let mut bytes = Vec::new();
        write_message(&mut bytes, id, value, K::MAX_REPLY).expect("encoded reply");
        let mut bytes = bytes.as_slice();
        while let Some(frame) = frame::read(&mut bytes, Pipe::Data).expect("reply frame") {
            self.replies.send(Some(frame)).await.expect("actor reads");
        }
    }
}
