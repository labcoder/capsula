use super::{Dispatch, Dispatching, Generation, Instant, WorkerLost, oneshot, watch};

#[derive(Clone, Debug)]
pub(crate) struct Retirement(watch::Sender<Option<Generation>>);

pub(super) type Intents = watch::Receiver<Option<Generation>>;

impl Retirement {
    pub(super) fn channel() -> (Self, Intents) {
        let (send, receive) = watch::channel(None);
        (Self(send), receive)
    }

    pub(crate) fn request(&self, generation: Generation) -> Result<(), WorkerLost> {
        self.0.send_if_modified(|current| {
            if current.is_none_or(|current| current < generation) {
                *current = Some(generation);
                true
            } else {
                false
            }
        });
        if self.0.is_closed() {
            Err(WorkerLost::ExitUnconfirmed)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn closed(&self) {
        self.0.closed().await;
    }
}

struct Guard {
    generation: Generation,
    retirement: Retirement,
    armed: bool,
}

impl Guard {
    fn new(generation: Generation, retirement: Retirement) -> Self {
        Self {
            generation,
            retirement,
            armed: true,
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed && self.retirement.request(self.generation).is_err() {
            tracing::debug!("resource retirement unavailable");
        }
    }
}

pub(crate) struct Completion<R> {
    reply: R,
    generation: Generation,
    until: Instant,
    guard: Guard,
}

impl<R> Completion<R> {
    pub(crate) fn reply(&self) -> &R {
        &self.reply
    }

    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }

    pub(crate) fn handoff(mut self) -> Result<(R, Generation), WorkerLost> {
        if Instant::now() >= self.until || self.generation != self.guard.generation {
            return Err(WorkerLost::Lost);
        }
        self.guard.armed = false;
        Ok((self.reply, self.generation))
    }
}

impl<R> std::fmt::Debug for Completion<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

pub(super) struct Destination<R> {
    answer: oneshot::Sender<Result<Completion<R>, WorkerLost>>,
    expected: Option<Generation>,
    until: Instant,
    guard: Option<Guard>,
    retirement: Retirement,
    pub(super) dispatch: Dispatching,
}

impl<R> Destination<R> {
    pub(super) fn new(
        expected: Option<Generation>,
        until: Instant,
        retirement: Retirement,
        dispatch: Option<watch::Sender<Dispatch>>,
    ) -> (Self, oneshot::Receiver<Result<Completion<R>, WorkerLost>>) {
        let (answer, receive) = oneshot::channel();
        let guard = expected.map(|generation| Guard::new(generation, retirement.clone()));
        (
            Self {
                answer,
                expected,
                until,
                guard,
                retirement,
                dispatch: Dispatching(dispatch),
            },
            receive,
        )
    }

    pub(super) fn is_closed(&self) -> bool {
        self.answer.is_closed()
    }

    pub(super) fn matches(&self, generation: Generation) -> bool {
        self.expected.is_none_or(|expected| expected == generation)
    }

    pub(super) fn dispatched(&mut self, generation: Generation) {
        self.dispatch.dispatched(generation);
        if self.guard.is_none() {
            self.guard = Some(Guard::new(generation, self.retirement.clone()));
        }
    }

    pub(super) fn deliver(self, reply: R, generation: Generation) {
        let completion = Completion {
            reply,
            generation,
            until: self.until,
            guard: self.guard.expect("a resource reply follows dispatch"),
        };
        if self.answer.send(Ok(completion)).is_err() {
            tracing::debug!("resource requester cancelled");
        }
    }

    pub(super) fn fail(self) {
        if self.answer.send(Err(WorkerLost::Lost)).is_err() {
            tracing::debug!("resource requester cancelled");
        }
    }
}
