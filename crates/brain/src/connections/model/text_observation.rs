//! Bounded, content-free reservation and local I/O stamps in the copied Dev brain only.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use kumochi_protocol::constants::{BRAIN_EXE, LIVE_BUDGET_ENV, LIVE_CHATGPT_TEXT_ENV};
use kumochi_protocol::message::Effort;

use crate::connections::http_client::Endpoint;
use crate::coordinator::SendKey;
use crate::engines::RunRequest;
use crate::platform::qpc_now;

static OBSERVATION: OnceLock<Option<Arc<Observation>>> = OnceLock::new();

pub(crate) struct Observation {
    pid: u32,
    ceiling: u32,
    counts: Mutex<Counts>,
}

struct Counts {
    out: Option<Box<dyn Write + Send>>,
    model: u32,
    aux: u32,
}

impl Observation {
    pub(crate) fn active() -> Option<&'static Arc<Self>> {
        OBSERVATION.get_or_init(Self::open).as_ref()
    }

    fn open() -> Option<Arc<Self>> {
        if std::env::var(LIVE_CHATGPT_TEXT_ENV).as_deref() != Ok("1") {
            return None;
        }
        let ceiling = std::env::var(LIVE_BUDGET_ENV).ok()?.parse().ok()?;
        let exe = std::env::current_exe().ok()?;
        let bin = exe.parent()?;
        let run = bin.parent()?;
        if !(1..=60).contains(&ceiling)
            || exe.file_name()? != BRAIN_EXE
            || bin.file_name()? != "bin"
            || run.file_name()?.to_str()?.parse::<u64>().is_err()
            || run.parent()?.file_name()? != "live"
        {
            return None;
        }
        let out = File::create_new(run.join("chatgpt-text.stamps")).ok()?;
        Self::start(out, ceiling)
    }

    fn start(mut out: impl Write + Send + 'static, ceiling: u32) -> Option<Arc<Self>> {
        let pid = std::process::id();
        writeln!(out, "ready {pid} {ceiling} {}", qpc_now()?).ok()?;
        Some(Arc::new(Self {
            pid,
            ceiling,
            counts: Mutex::new(Counts {
                out: Some(Box::new(out)),
                model: 0,
                aux: 0,
            }),
        }))
    }

    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn finish(&self) {
        let mut counts = self.counts();
        let (pid, model, aux) = (self.pid, counts.model, counts.aux);
        counts.write(|at, out| writeln!(out, "final {pid} {model} {aux} {at}"));
    }

    pub(crate) fn auxiliary(&self, endpoint: &Endpoint) {
        let class = match endpoint {
            Endpoint::Token => 1,
            Endpoint::Discovery => 2,
            Endpoint::Jwks(_) => 3,
            Endpoint::Revocation(_) => 4,
            Endpoint::Models(_) => 5,
            Endpoint::AnthropicModels(_) | Endpoint::GitHub(_) => 0,
        };
        let mut counts = self.counts();
        let ordinal = counts.aux.saturating_add(1);
        counts.aux = ordinal;
        if ordinal > 64 {
            counts.out = None;
        }
        let pid = self.pid;
        counts.write(|at, out| writeln!(out, "aux {pid} {ordinal} {class} {at}"));
    }
}

impl Counts {
    fn write(&mut self, stamp: impl FnOnce(u64, &mut dyn Write) -> std::io::Result<()>) {
        let Some(out) = self.out.as_mut() else { return };
        if qpc_now().is_none_or(|at| stamp(at, out).is_err()) {
            self.out = None;
        }
    }
}

/// The ordinal joins the transport and parser records for one actual reservation.
#[derive(Clone)]
pub(crate) struct Stamp {
    observation: Arc<Observation>,
    sequence: Arc<AtomicU32>,
    run: u64,
    owner: u8,
    key: u64,
    round: u32,
    target: u8,
    effort: u8,
}

#[derive(Clone, Copy)]
pub(super) enum End {
    Closed,
    Pooled,
    NoIo,
}

impl Stamp {
    pub(crate) fn new(request: &RunRequest, observation: &Arc<Observation>) -> Self {
        let (owner, key, round) = match request.key {
            SendKey::Build(key) => (1, key.attempt, key.round),
            SendKey::Job(key) => (2, key.serial, 0),
        };
        let (target, effort) = request.request.choice.as_ref().map_or((0, 0), |choice| {
            let target = match choice.model.as_str() {
                "gpt-6-luna" => 1,
                "gpt-6.1-sol" => 2,
                _ => 0,
            };
            let effort = match choice.effort {
                Effort::Low => 1,
                Effort::Medium => 2,
                Effort::High => 3,
                Effort::XHigh => 4,
                Effort::Max => 5,
            };
            (target, effort)
        });
        Self {
            observation: Arc::clone(observation),
            sequence: Arc::default(),
            run: request.run.0,
            owner,
            key,
            round,
            target,
            effort,
        }
    }

    pub(super) fn reserved(&self) {
        let pid = self.observation.pid;
        let mut counts = self.observation.counts();
        let ordinal = counts.model.saturating_add(1);
        counts.model = ordinal;
        if ordinal > self.observation.ceiling {
            counts.out = None;
            return;
        }
        self.sequence.store(ordinal, Ordering::Release);
        let Self {
            run,
            owner,
            key,
            round,
            target,
            effort,
            ..
        } = self;
        counts.write(|at, out| {
            writeln!(
                out,
                "reserve {pid} {ordinal} {run} {owner} {key} {round} {target} {effort} {at}"
            )
        });
    }

    pub(crate) fn first_text(&self) {
        self.mark("text");
    }

    pub(super) fn ended(&self, end: End) {
        self.mark(match end {
            End::Closed => "closed",
            End::Pooled => "pooled",
            End::NoIo => "no_io",
        });
    }

    fn mark(&self, kind: &'static str) {
        let ordinal = self.sequence.load(Ordering::Acquire);
        if ordinal != 0 {
            let pid = self.observation.pid;
            self.observation
                .counts()
                .write(|at, out| writeln!(out, "{kind} {pid} {ordinal} {at}"));
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture(
    ceiling: u32,
    receive: impl FnMut(&str) + Send + 'static,
) -> Arc<Observation> {
    struct Lines<F>(F, Vec<u8>);

    impl<F: FnMut(&str)> Write for Lines<F> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            for byte in bytes {
                if *byte == b'\n' {
                    (self.0)(std::str::from_utf8(&self.1).expect("fixture observes ASCII"));
                    self.1.clear();
                } else {
                    self.1.push(*byte);
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    Observation::start(Lines(receive, Vec::new()), ceiling).expect("the memory recorder opens")
}
