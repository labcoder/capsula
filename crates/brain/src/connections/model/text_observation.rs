//! Bounded, content-free reservation and local I/O stamps in the copied Dev brain only.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use kumochi_protocol::constants::{
    BRAIN_EXE, LIVE_BUDGET_ENV, LIVE_CHATGPT_TEXT_ENV, LIVE_CHATGPT_TEXT_STAMPS_FILE,
    LIVE_RUN_BIN_DIR, LIVE_RUNS_DIR, ProductIdentity,
};
use kumochi_protocol::message::Effort;

mod drawing;
pub(crate) use drawing::{DrawStage, REPLAY_CALL, REPLAY_STEPS};

use crate::connections::catalog::DEFAULT_SLUG;
use crate::connections::http_client::Endpoint;
use crate::coordinator::SendKey;
use crate::engines::RunRequest;
use crate::platform::qpc_now;

static OBSERVATION: OnceLock<Option<Arc<Observation>>> = OnceLock::new();
const MAX_AUX: u32 = 64;

pub(crate) struct Observation {
    pid: u32,
    ceiling: u32,
    invalid: AtomicBool,
    counts: Mutex<Counts>,
    draw: Option<Mutex<drawing::DrawReceipt>>,
}

struct Counts {
    out: Option<Box<dyn Write + Send>>,
    model: u32,
    aux: u32,
    rows: Vec<Row>,
}

fn opening(
    identity: ProductIdentity,
    opt_in: &str,
    budget: &str,
    exe: &Path,
) -> Option<(u32, PathBuf)> {
    if !matches!(
        (identity, opt_in),
        (ProductIdentity::DEV, "1") | (ProductIdentity::GATE, "draw")
    ) {
        return None;
    }
    let ceiling = budget.parse().ok()?;
    let bin = exe.parent()?;
    let run = bin.parent()?;
    let number = run.file_name()?.to_str()?;
    if !(1..=60).contains(&ceiling)
        || (opt_in == "draw" && ceiling != 1)
        || exe.file_name()? != BRAIN_EXE
        || bin.file_name()? != LIVE_RUN_BIN_DIR
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number.parse::<u64>().is_err()
        || run.parent()?.file_name()? != LIVE_RUNS_DIR
    {
        return None;
    }
    Some((ceiling, run.join(LIVE_CHATGPT_TEXT_STAMPS_FILE)))
}

impl Observation {
    pub(crate) fn prepare(exe: &Path) {
        OBSERVATION.get_or_init(|| Self::open(exe));
    }

    pub(crate) fn active() -> Option<&'static Arc<Self>> {
        Self::current().filter(|record| record.draw.is_none())
    }

    pub(crate) fn current() -> Option<&'static Arc<Self>> {
        OBSERVATION.get().and_then(Option::as_ref)
    }

    pub(crate) fn drawing() -> Option<&'static Arc<Self>> {
        Self::current().filter(|record| record.draw.is_some())
    }

    fn open(exe: &Path) -> Option<Arc<Self>> {
        let opt_in = std::env::var(LIVE_CHATGPT_TEXT_ENV).ok()?;
        let budget = std::env::var(LIVE_BUDGET_ENV).ok()?;
        let (ceiling, path) = opening(ProductIdentity::ACTIVE, &opt_in, &budget, exe)?;
        Self::start_mode(
            File::create_new(&path).ok()?,
            ceiling,
            (opt_in == "draw").then(drawing::DrawReceipt::default),
        )
    }

    #[cfg(test)]
    fn create(path: &Path, ceiling: u32) -> Option<Arc<Self>> {
        Self::start(File::create_new(path).ok()?, ceiling)
    }

    #[cfg(test)]
    fn start(out: impl Write + Send + 'static, ceiling: u32) -> Option<Arc<Self>> {
        Self::start_mode(out, ceiling, None)
    }

    #[cfg(test)]
    pub(crate) fn draw_fixture() -> Arc<Self> {
        Self::start_mode(io::sink(), 1, Some(drawing::DrawReceipt::default()))
            .expect("a fixture can open an in-memory drawing receipt")
    }

    fn start_mode(
        mut out: impl Write + Send + 'static,
        ceiling: u32,
        draw: Option<drawing::DrawReceipt>,
    ) -> Option<Arc<Self>> {
        let pid = std::process::id();
        writeln!(out, "ready {pid} {ceiling} {}", qpc_now()?).ok()?;
        Some(Arc::new(Self {
            pid,
            ceiling,
            invalid: AtomicBool::new(false),
            counts: Mutex::new(Counts {
                out: Some(Box::new(out)),
                model: 0,
                aux: 0,
                rows: Vec::with_capacity(usize::try_from(3 * ceiling + MAX_AUX).ok()?),
            }),
            draw: draw.map(Mutex::new),
        }))
    }

    fn record(&self, at: Option<u64>, append: impl FnOnce(&mut Counts, u64) -> bool) {
        if self.invalid.load(Ordering::Acquire) {
            return;
        }
        if let (Some(at), Ok(mut counts)) = (at, self.counts.try_lock())
            && counts.out.is_some()
            && append(&mut counts, at)
        {
            return;
        }
        self.invalid.store(true, Ordering::Release);
    }

    pub(crate) fn finish(&self) {
        let Ok(mut counts) = self.counts.try_lock() else {
            self.invalid.store(true, Ordering::Release);
            return;
        };
        let Some(mut out) = counts.out.take() else {
            return;
        };
        if self.invalid.load(Ordering::Acquire) {
            return;
        }
        let pid = self.pid;
        let result = counts
            .rows
            .iter()
            .try_for_each(|row| row.write(pid, &mut out));
        if result.is_ok()
            && self.draw.as_ref().is_none_or(|draw| {
                draw.lock()
                    .is_ok_and(|draw| draw.write(pid, &mut out).is_ok())
            })
            && !self.invalid.load(Ordering::Acquire)
            && let Some(at) = qpc_now()
        {
            let (model, aux) = (counts.model, counts.aux);
            if writeln!(out, "final {pid} {model} {aux} {at}").is_ok() {
                return;
            }
        }
        self.invalid.store(true, Ordering::Release);
    }

    pub(crate) fn auxiliary(&self, endpoint: &Endpoint) {
        let at = qpc_now();
        let class = match endpoint {
            Endpoint::Token => 1,
            Endpoint::Discovery => 2,
            Endpoint::Jwks(_) => 3,
            Endpoint::Revocation(_) => 4,
            Endpoint::Models(_) => 5,
            Endpoint::GitHub(_) => 0,
        };
        self.record(at, |counts, at| {
            let ordinal = counts.aux + 1;
            if ordinal > MAX_AUX || !counts.push(at, ordinal, Event::Aux(class)) {
                return false;
            }
            counts.aux = ordinal;
            true
        });
    }

    #[cfg(test)]
    pub(crate) fn buffered_for_tests(&self) -> usize {
        self.counts.lock().unwrap().rows.len()
    }
}

impl Counts {
    fn push(&mut self, at: u64, ordinal: u32, event: Event) -> bool {
        if self.rows.len() == self.rows.capacity() {
            return false;
        }
        self.rows.push(Row { at, ordinal, event });
        true
    }
}

struct Row {
    at: u64,
    ordinal: u32,
    event: Event,
}

enum Event {
    Reserve(Identity),
    Text,
    Closed(u64),
    Pooled,
    NoIo,
    Aux(u8),
}

impl Row {
    fn write(&self, pid: u32, out: &mut impl Write) -> io::Result<()> {
        let Self { at, ordinal, event } = self;
        match event {
            Event::Reserve(Identity {
                run,
                owner,
                key,
                round,
                target,
                effort,
            }) => writeln!(
                out,
                "reserve {pid} {ordinal} {run} {owner} {key} {round} {target} {effort} {at}"
            ),
            Event::Text => writeln!(out, "text {pid} {ordinal} {at}"),
            Event::Closed(before) => writeln!(out, "closed {pid} {ordinal} {before} {at}"),
            Event::Pooled => writeln!(out, "pooled {pid} {ordinal} {at}"),
            Event::NoIo => writeln!(out, "no_io {pid} {ordinal} {at}"),
            Event::Aux(class) => writeln!(out, "aux {pid} {ordinal} {class} {at}"),
        }
    }
}

/// The ordinal joins the transport and parser records for one actual reservation.
#[derive(Clone)]
pub(crate) struct Stamp {
    observation: Arc<Observation>,
    sequence: Arc<AtomicU32>,
    identity: Identity,
}

#[derive(Clone, Copy)]
struct Identity {
    run: u64,
    owner: u8,
    key: u64,
    round: u32,
    target: u8,
    effort: u8,
}

#[derive(Clone, Copy)]
pub(super) enum End {
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
                DEFAULT_SLUG => 1,
                // This is the other fixed target of the live comparison.
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
            identity: Identity {
                run: request.run.0,
                owner,
                key,
                round,
                target,
                effort,
            },
        }
    }

    pub(super) fn reserved(&self) {
        self.observation.record(qpc_now(), |counts, at| {
            let ordinal = counts.model + 1;
            if ordinal > self.observation.ceiling
                || !counts.push(at, ordinal, Event::Reserve(self.identity))
            {
                return false;
            }
            counts.model = ordinal;
            self.sequence.store(ordinal, Ordering::Release);
            true
        });
    }

    pub(crate) fn first_text(&self) {
        self.mark(qpc_now(), Event::Text);
    }

    pub(super) fn close<T>(&self, connection: T) {
        let before = qpc_now();
        drop(connection);
        let after = qpc_now();
        if let Some(before) = before {
            self.mark(
                after.filter(|after| *after >= before),
                Event::Closed(before),
            );
        } else {
            self.observation.invalid.store(true, Ordering::Release);
        }
    }

    pub(super) fn ended(&self, end: End) {
        self.mark(
            qpc_now(),
            match end {
                End::Pooled => Event::Pooled,
                End::NoIo => Event::NoIo,
            },
        );
    }

    fn mark(&self, at: Option<u64>, event: Event) {
        let ordinal = self.sequence.load(Ordering::Acquire);
        if ordinal != 0 {
            self.observation
                .record(at, |counts, at| counts.push(at, ordinal, event));
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

#[cfg(test)]
pub(crate) fn stalled_fixture(
    ceiling: u32,
    mut receive: impl FnMut(&str) + Send + 'static,
) -> (Arc<Observation>, std::sync::mpsc::Sender<()>) {
    let (release, wait) = std::sync::mpsc::channel();
    let mut wait = Some(wait);
    let observation = fixture(ceiling, move |line| {
        if !line.starts_with("ready ")
            && let Some(wait) = wait.take()
        {
            wait.recv_timeout(std::time::Duration::from_secs(2))
                .expect("the writer must not start before dispatch ended");
        }
        receive(line);
    });
    (observation, release)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kumochi_protocol::constants::{
        BRAIN_EXE, LIVE_CHATGPT_TEXT_STAMPS_FILE, LIVE_RUN_BIN_DIR, LIVE_RUNS_DIR, ProductIdentity,
    };

    use super::{Event, Observation, opening};
    use crate::connections::http_client::Endpoint;
    use crate::test_support::Scratch;

    #[test]
    fn drawing_observation_admits_only_the_one_gate_fixture() {
        let run = Path::new(LIVE_RUNS_DIR).join("42");
        let exe = run.join(LIVE_RUN_BIN_DIR).join(BRAIN_EXE);
        assert_eq!(
            opening(ProductIdentity::GATE, "draw", "1", &exe),
            Some((1, run.join(LIVE_CHATGPT_TEXT_STAMPS_FILE)))
        );
        assert_eq!(opening(ProductIdentity::DEV, "draw", "1", &exe), None);
        assert_eq!(opening(ProductIdentity::GATE, "1", "1", &exe), None);
        assert_eq!(opening(ProductIdentity::GATE, "draw", "2", &exe), None);
    }

    #[test]
    fn observation_opening_checks_the_opt_in_budget_and_copy_layout() {
        let live = Path::new("target").join(LIVE_RUNS_DIR);
        let run = live.join("42");
        let bin = run.join(LIVE_RUN_BIN_DIR);
        let exe = bin.join(BRAIN_EXE);
        for (budget, ceiling) in [("1", 1), ("60", 60)] {
            assert_eq!(
                opening(ProductIdentity::DEV, "1", budget, &exe),
                Some((ceiling, run.join(LIVE_CHATGPT_TEXT_STAMPS_FILE)))
            );
        }
        for budget in ["0", "61", "", "nonnumber"] {
            assert_eq!(
                opening(ProductIdentity::DEV, "1", budget, &exe),
                None,
                "budget {budget}"
            );
        }
        for opt_in in ["", "0", "true", "01"] {
            assert_eq!(
                opening(ProductIdentity::DEV, opt_in, "1", &exe),
                None,
                "opt-in {opt_in}"
            );
        }
        for identity in [ProductIdentity::GATE, ProductIdentity::RELEASE] {
            assert_eq!(opening(identity, "1", "1", &exe), None);
        }
        for wrong in [
            bin.join("different.exe"),
            run.join("not-bin").join(BRAIN_EXE),
            live.join("x").join(LIVE_RUN_BIN_DIR).join(BRAIN_EXE),
            live.join("+42").join(LIVE_RUN_BIN_DIR).join(BRAIN_EXE),
            live.join("18446744073709551616")
                .join(LIVE_RUN_BIN_DIR)
                .join(BRAIN_EXE),
            Path::new("not-live")
                .join("42")
                .join(LIVE_RUN_BIN_DIR)
                .join(BRAIN_EXE),
            Path::new(BRAIN_EXE).to_owned(),
        ] {
            assert_eq!(
                opening(ProductIdentity::DEV, "1", "1", &wrong),
                None,
                "path {wrong:?}"
            );
        }
        let greatest = live.join("18446744073709551615");
        assert_eq!(
            opening(
                ProductIdentity::DEV,
                "1",
                "1",
                &greatest.join(LIVE_RUN_BIN_DIR).join(BRAIN_EXE)
            ),
            Some((1, greatest.join(LIVE_CHATGPT_TEXT_STAMPS_FILE)))
        );
    }

    #[test]
    fn observation_creates_only_a_fresh_stamp_file() {
        let scratch = Scratch::new("observation-stamp");
        std::fs::create_dir(scratch.dir()).unwrap();
        let path = scratch.dir().join(LIVE_CHATGPT_TEXT_STAMPS_FILE);
        let observation = Observation::create(&path, 1).expect("a fresh recorder");
        observation.finish();
        drop(observation);
        let ready = std::fs::read(&path).unwrap();
        let expected = format!("ready {} 1 ", std::process::id());
        assert!(ready.starts_with(expected.as_bytes()));
        assert!(Observation::create(&path, 1).is_none());
        assert_eq!(std::fs::read(path).unwrap(), ready);
    }

    #[test]
    fn observation_finishes_after_a_late_auxiliary_call_invalidates_teardown() {
        use std::future::pending;
        use std::sync::{Arc, Mutex};

        struct Late(Arc<Observation>);
        impl Drop for Late {
            fn drop(&mut self) {
                self.0.auxiliary(&Endpoint::Token);
            }
        }

        for (prior, expected) in [(63, 66), (64, 1)] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let out = Arc::clone(&seen);
            let observation =
                super::fixture(1, move |line| out.lock().unwrap().push(line.to_owned()));
            for _ in 0..prior {
                observation.auxiliary(&Endpoint::Token);
            }
            let late = Late(Arc::clone(&observation));
            runtime.block_on(async {
                drop(tokio::spawn(async move {
                    let _late = late;
                    pending::<()>().await;
                }));
                tokio::task::yield_now().await;
            });
            assert_eq!(observation.buffered_for_tests(), prior);
            crate::end_runtime(runtime, Some(&observation));
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), expected);
            assert!(seen[0].starts_with("ready "));
            if prior == 63 {
                assert!(seen[64].starts_with(&format!("aux {} 64 1 ", std::process::id())));
                assert!(seen[65].starts_with(&format!("final {} 0 64 ", std::process::id())));
            }
        }
    }

    #[test]
    fn observation_invalidates_a_busy_buffer_without_waiting() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&seen);
        let observation = super::fixture(1, move |line| out.lock().unwrap().push(line.to_owned()));
        let (held, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        std::thread::scope(|scope| {
            let held_observation = Arc::clone(&observation);
            scope.spawn(move || {
                let _busy = held_observation.counts.lock().unwrap();
                held.send(()).unwrap();
                let _released = wait.recv_timeout(Duration::from_secs(2));
            });
            ready.recv().unwrap();
            observation.auxiliary(&Endpoint::Token);
            release
                .send(())
                .expect("append did not wait for the buffer");
        });
        observation.finish();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("ready "));
    }

    #[test]
    fn observation_does_not_grow_a_full_buffer() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&seen);
        let observation = super::fixture(1, move |line| out.lock().unwrap().push(line.to_owned()));
        for ordinal in 1..=67 {
            observation.record(Some(1), |counts, at| counts.push(at, ordinal, Event::Text));
        }
        assert_eq!(observation.buffered_for_tests(), 67);
        observation.auxiliary(&Endpoint::Token);
        assert_eq!(observation.buffered_for_tests(), 67);
        observation.finish();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn observation_writes_no_final_if_persistence_fails() {
        use std::io::{self, Write};
        use std::sync::{Arc, Mutex};

        struct FailsOnce(Arc<Mutex<(Vec<u8>, bool)>>);
        impl Write for FailsOnce {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let mut data = self.0.lock().unwrap();
                if data.0.ends_with(b"\n") && !data.1 {
                    data.1 = true;
                    return Err(io::ErrorKind::WriteZero.into());
                }
                data.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let data = Arc::new(Mutex::new((Vec::new(), false)));
        let observation = Observation::start(FailsOnce(Arc::clone(&data)), 1).unwrap();
        observation.auxiliary(&Endpoint::Token);
        assert!(!data.lock().unwrap().1);
        observation.finish();
        let data = data.lock().unwrap();
        assert!(data.1, "the sink failed when persistence began");
        let lines = std::str::from_utf8(&data.0)
            .unwrap()
            .lines()
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("ready "));
    }
}
