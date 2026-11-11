use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as blocking;
use std::task::{Context, Poll, Wake, Waker};

use super::*;
use crate::supervise::fixture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    Load,
    Unload,
}

struct Readiness(Mutex<Option<blocking::Sender<tokio::time::Instant>>>);

impl Wake for Readiness {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(ready) = self.0.lock().expect("readiness lock").take()
            && ready.send(tokio::time::Instant::now()).is_err()
        {
            tracing::debug!("test readiness observer closed");
        }
    }
}

struct Pause {
    admitted: blocking::Sender<tokio::time::Instant>,
    readiness: Arc<Readiness>,
    resume: oneshot::Receiver<()>,
    polls: Arc<AtomicUsize>,
}

pub(super) struct Gate {
    cut: Cut,
    pause: Option<Pause>,
    waiting: Option<blocking::Sender<bool>>,
    stopped: Option<blocking::Sender<()>>,
}

impl Drop for Gate {
    fn drop(&mut self) {
        if let Some(stopped) = self.stopped.take()
            && stopped.send(()).is_err()
        {
            tracing::debug!("test owner observer closed");
        }
    }
}

pub(crate) struct Control {
    admitted: Option<blocking::Receiver<tokio::time::Instant>>,
    ready: Option<blocking::Receiver<tokio::time::Instant>>,
    waiting: Option<blocking::Receiver<bool>>,
    stopped: Option<blocking::Receiver<()>>,
    resume: Option<oneshot::Sender<()>>,
    polls: Arc<AtomicUsize>,
    owner: Arc<super::Control>,
}

async fn phase<T: Send + 'static>(slot: &mut Option<blocking::Receiver<T>>) -> T {
    let (value, receive) = fixture::receive_request(slot.take().expect("phase receiver")).await;
    *slot = Some(receive);
    value
}

impl Control {
    pub(crate) async fn admitted(&mut self) -> tokio::time::Instant {
        phase(&mut self.admitted).await
    }

    pub(crate) fn not_ready(&self) {
        assert_eq!(
            self.ready.as_ref().expect("readiness receiver").try_recv(),
            Err(blocking::TryRecvError::Empty)
        );
    }

    pub(crate) async fn ready(&mut self) -> tokio::time::Instant {
        phase(&mut self.ready).await
    }

    pub(crate) fn paused(&self) {
        assert_eq!(self.polls.load(Ordering::Relaxed), 1);
    }

    pub(crate) fn resume(&mut self) {
        self.resume
            .take()
            .expect("one resume")
            .send(())
            .expect("caller is parked");
    }

    pub(crate) async fn waiting(&mut self) {
        assert!(
            phase(&mut self.waiting).await,
            "the real exit wait polled Pending"
        );
        assert_eq!(self.polls.load(Ordering::Relaxed), 2);
    }

    pub(crate) async fn stopped(&mut self) {
        phase(&mut self.stopped).await;
    }

    pub(crate) fn owned(&self, ask: u64) {
        assert_eq!(self.owner.lock().current, Some(ask));
    }

    pub(crate) fn released(&self) {
        assert_eq!(self.owner.lock().current, None);
    }
}

pub(super) fn hold<F: Future>(
    gate: &mut Option<Gate>,
    cut: Cut,
    future: F,
) -> impl Future<Output = F::Output> + use<F> {
    let pause = gate
        .as_mut()
        .filter(|gate| gate.cut == cut)
        .and_then(|gate| gate.pause.take());
    async move {
        let Some(pause) = pause else {
            return future.await;
        };
        let mut future = std::pin::pin!(future);
        let started = tokio::time::Instant::now();
        let waker = Waker::from(Arc::clone(&pause.readiness));
        pause.polls.fetch_add(1, Ordering::Relaxed);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending(),
            "initial admission remains pending"
        );
        pause.admitted.send(started).expect("admission observer");
        pause.resume.await.expect("explicit caller resume");
        std::future::poll_fn(|cx| {
            pause.polls.fetch_add(1, Ordering::Relaxed);
            future.as_mut().poll(cx)
        })
        .await
    }
}

pub(super) fn observe_wait<F: Future>(
    gate: &mut Option<Gate>,
    future: F,
) -> impl Future<Output = F::Output> + use<F> {
    let mut waiting = gate.as_mut().and_then(|gate| gate.waiting.take());
    async move {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| {
            let result = future.as_mut().poll(cx);
            if let Some(waiting) = waiting.take() {
                waiting
                    .send(matches!(result, Poll::Pending))
                    .expect("exit-wait observer");
            }
            result
        })
        .await
    }
}

impl SttHandle {
    pub(crate) fn spawn_poll_cut(
        runtime: Handle,
        speech: SpeechHandle,
        reports: Reports,
        cut: Cut,
    ) -> io::Result<(Self, Control)> {
        let (admitted, admission) = blocking::channel();
        let (ready, readiness) = blocking::channel();
        let (waiting, waited) = blocking::channel();
        let (stopped, stop) = blocking::channel();
        let (resume, resumed) = oneshot::channel();
        let polls = Arc::new(AtomicUsize::new(0));
        let gate = Gate {
            cut,
            pause: Some(Pause {
                admitted,
                readiness: Arc::new(Readiness(Mutex::new(Some(ready)))),
                resume: resumed,
                polls: Arc::clone(&polls),
            }),
            waiting: Some(waiting),
            stopped: Some(stopped),
        };
        let owner = Self::spawn_adapter(
            runtime,
            speech,
            PathBuf::from("natives"),
            reports,
            Some(gate),
        )?;
        let control = Control {
            admitted: Some(admission),
            ready: Some(readiness),
            waiting: Some(waited),
            stopped: Some(stop),
            resume: Some(resume),
            polls,
            owner: Arc::clone(&owner.control),
        };
        Ok((owner, control))
    }
}
