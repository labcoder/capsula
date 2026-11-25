//! Custody of a brain the shell starts as its own child. One owner thread spawns it, observes it,
//! signals it and reaps it, and nothing else in the shell waits on it. The system calls come from
//! the platform through [`Os`]; the order of decisions lives here, so every platform tests it.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How often the owner observes its child. A poll period, not a scheduling bound.
const POLL: Duration = Duration::from_millis(20);

const SPAWN: u8 = 1;
const QUIT: u8 = 1 << 1;

/// Orders Quit against the spawn claim. Both operations act on this one word, so they're totally
/// ordered: a claim that lands first owns whatever its spawn produced, and after Quit no claim
/// succeeds.
#[derive(Debug, Default)]
pub(crate) struct SpawnWord(AtomicU8);

impl SpawnWord {
    /// Marks Quit for good. Never waits.
    pub(crate) fn quit(&self) {
        self.0.fetch_or(QUIT, Ordering::SeqCst);
    }

    /// Takes the one spawn permit, only while neither Quit nor another claim is set.
    pub(crate) fn claim(&self) -> bool {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
                (word & (SPAWN | QUIT) == 0).then_some(word | SPAWN)
            })
            .is_ok()
    }

    /// Gives the permit back: the spawn produced no child, or the child was reaped.
    fn release(&self) {
        self.0.fetch_and(!SPAWN, Ordering::SeqCst);
    }

    fn quitting(&self) -> bool {
        self.0.load(Ordering::SeqCst) & QUIT != 0
    }
}

/// How a reaped child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    Code(i32),
    Signal(i32),
}

/// One exact-pid, non-blocking wait on the child, classified by the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// No status yet.
    Running,
    /// A stop, a continue or any status that is neither an exit nor a signal. Not reaped: the
    /// pid is still our child, and it counts as alive for every rule.
    Stopped,
    /// Reaped, terminal.
    Reaped(Exit),
    /// The wait failed for a reason other than an interruption: custody is gone. Never success,
    /// and the child is never signalled again.
    Lost,
}

/// The system calls the owner makes, all on its own thread.
pub(crate) trait Os: Send + 'static {
    /// True when this process would reap its children itself (SIGCHLD ignored, or
    /// `SA_NOCLDWAIT`). Read before the spawn and before each signal.
    fn auto_reaps(&self) -> bool;
    /// Starts the child.
    fn spawn(&mut self) -> io::Result<()>;
    /// Waits on the child's exact pid without blocking, retrying interruptions.
    fn poll(&mut self) -> Observation;
    /// Sends the child SIGKILL.
    fn kill(&mut self) -> io::Result<()>;
}

/// What the owner tells the link, in order: one spawn report, then at most one settlement.
#[derive(Debug)]
pub(crate) enum Report {
    /// The child exists and this owner holds it.
    Spawned,
    /// No child exists, and none will from this owner.
    NotSpawned(Refusal),
    /// Terminal for every decision about this child.
    Settled(Settled),
}

/// Why the owner started no child.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Refusal {
    #[error("quitting")]
    Quitting,
    #[error("another brain child holds the spawn claim")]
    Claimed,
    #[error("the shell reaps its children itself")]
    AutoReap,
    #[error("can't start the brain")]
    Spawn(#[source] io::Error),
}

/// How custody of a spawned child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Reaped, and the spawn claim is free again.
    Reaped(Exit),
    /// Custody is gone. The claim stays taken, so nothing replaces this child.
    Lost,
    /// Still unreaped after the kill's bounded wait. The claim stays taken; the owner keeps
    /// reaping in the background while the shell lives, but reports nothing more.
    Unsettled,
}

impl Settled {
    /// The child's half of a clean Quit: reaped with exit code 0. The link's half is that it
    /// sent `Quitting`; without both, the Quit isn't clean.
    pub(crate) fn is_clean(self) -> bool {
        self == Self::Reaped(Exit::Code(0))
    }
}

/// The link's side of one owner thread. Dropping it retires a live child, as [`Self::retire`]
/// does.
#[derive(Debug)]
pub(crate) struct ChildOwner {
    retire: Sender<()>,
    reports: Receiver<Report>,
}

impl ChildOwner {
    /// Starts the owner thread, which claims `word` and spawns on that thread, so a child never
    /// exists without its owner. Retirement waits `wait` for the child to exit, then kills it and
    /// waits `wait` once more.
    ///
    /// # Errors
    /// Fails when the thread can't start; then nothing was claimed or spawned.
    pub(crate) fn start(os: impl Os, word: Arc<SpawnWord>, wait: Duration) -> io::Result<Self> {
        let (retire, retires) = mpsc::channel();
        let (report, reports) = mpsc::channel();
        // Detached: after Unsettled it keeps reaping for as long as the shell lives.
        thread::Builder::new()
            .name("brain child".to_owned())
            .spawn(move || own(os, &word, wait, retires, &report))?;
        Ok(Self { retire, reports })
    }

    /// Starts bounded retirement of a live child. Never waits.
    pub(crate) fn retire(&self) {
        // A finished owner has nothing left to retire.
        let _settled = self.retire.send(());
    }

    /// The spawn report, which the owner sends right after its single spawn attempt.
    ///
    /// # Errors
    /// `Timeout` when none came by `by`: the outcome is pending, and a child may still start.
    /// `Disconnected` when the thread died before reporting.
    pub(crate) fn spawned(&self, by: Instant) -> Result<Report, RecvTimeoutError> {
        self.reports
            .recv_timeout(by.saturating_duration_since(Instant::now()))
    }

    /// The next report, or None when none arrives by `by`.
    pub(crate) fn next(&self, by: Instant) -> Option<Report> {
        self.reports
            .recv_timeout(by.saturating_duration_since(Instant::now()))
            .ok()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Watching,
    /// Waiting for the child to exit by itself until this deadline.
    Grace(Instant),
    /// Killed, or the kill failed; waiting until this deadline.
    Killed(Instant),
    Unsettled,
}

fn own(
    mut os: impl Os,
    word: &SpawnWord,
    wait: Duration,
    retires: Receiver<()>,
    report: &Sender<Report>,
) {
    // A link that's gone doesn't end custody, so a closed report channel is ignored throughout.
    if let Err(refusal) = spawn(&mut os, word) {
        // Whatever the child would have inherited is closed before the refusal is heard.
        drop(os);
        let _unheard = report.send(Report::NotSpawned(refusal));
        return;
    }
    let _unheard = report.send(Report::Spawned);
    let mut retires = Some(retires);
    let mut retiring = false;
    let mut phase = Phase::Watching;
    loop {
        // Read before the poll: a decision at a deadline follows a poll at or after it, so a child
        // that exits by the deadline is reaped, never killed. Each wait starts from a fresh read.
        let now = Instant::now();
        let settled = match os.poll() {
            Observation::Reaped(exit) => Some(Settled::Reaped(exit)),
            Observation::Lost => Some(Settled::Lost),
            Observation::Running | Observation::Stopped => None,
        };
        if let Some(settled) = settled {
            if phase != Phase::Unsettled {
                if matches!(settled, Settled::Reaped(_)) {
                    word.release();
                }
                let _unheard = report.send(Report::Settled(settled));
            }
            return;
        }
        phase = match phase {
            Phase::Watching if retiring || word.quitting() => Phase::Grace(Instant::now() + wait),
            Phase::Grace(deadline) if now >= deadline => {
                kill(&mut os);
                Phase::Killed(Instant::now() + wait)
            }
            Phase::Killed(deadline) if now >= deadline => {
                tracing::error!("the brain child is still unreaped after SIGKILL");
                let _unheard = report.send(Report::Settled(Settled::Unsettled));
                Phase::Unsettled
            }
            phase => phase,
        };
        match retires.as_ref().map(|retires| retires.recv_timeout(POLL)) {
            Some(Ok(())) => retiring = true,
            Some(Err(RecvTimeoutError::Timeout)) => {}
            Some(Err(RecvTimeoutError::Disconnected)) => {
                retiring = true;
                retires = None;
            }
            None => thread::sleep(POLL),
        }
    }
}

fn spawn(os: &mut impl Os, word: &SpawnWord) -> Result<(), Refusal> {
    if !word.claim() {
        return Err(if word.quitting() {
            Refusal::Quitting
        } else {
            Refusal::Claimed
        });
    }
    let spawned = if os.auto_reaps() {
        Err(Refusal::AutoReap)
    } else {
        os.spawn().map_err(Refusal::Spawn)
    };
    if spawned.is_err() {
        word.release();
    }
    spawned
}

/// One kill, never retried: a refusal or a failure leaves the bounded wait to decide.
fn kill(os: &mut impl Os) {
    if os.auto_reaps() {
        tracing::error!("not signalling the brain child: the shell reaps its children itself");
        return;
    }
    if let Err(error) = os.kill() {
        tracing::error!(
            errno = error.raw_os_error(),
            "couldn't kill the brain child"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Barrier, Mutex, MutexGuard, PoisonError};

    use super::*;

    const WAIT: Duration = Duration::from_millis(60);
    const BOUND: Duration = Duration::from_secs(5);

    /// A scripted child. It runs (or stays stopped) until `exits` comes due, a successful kill
    /// ends it with SIGKILL when `killable`, and `lost` makes every wait fail. A gate holds the
    /// next call of its kind until the test has seen it enter and released it.
    #[derive(Default)]
    #[expect(
        clippy::struct_excessive_bools,
        reason = "a test script's independent switches"
    )]
    struct Script {
        auto_reaps: bool,
        spawn_fails: bool,
        stopped: bool,
        exits: Option<(Instant, Exit)>,
        killable: bool,
        kill_fails: bool,
        lost: bool,
        spawn_gate: Option<Gate>,
        poll_gate: Option<Gate>,
        kill_gate: Option<Gate>,
        spawns: u32,
        kills: u32,
        killed_at: Option<Instant>,
        reaped: bool,
        /// Polls after the reap, and signals after a reap or a loss.
        forbidden: u32,
    }

    /// The fake's side of a gate. Every wait is bounded, and a test that fails releases it.
    struct Gate {
        entered: mpsc::Sender<()>,
        release: Receiver<()>,
    }

    /// The test's side of a gate.
    struct Held {
        entered: Receiver<()>,
        release: Sender<()>,
    }

    fn gate() -> (Gate, Held) {
        let (entered, entered_seen) = mpsc::channel();
        let (release, released) = mpsc::channel();
        (
            Gate {
                entered,
                release: released,
            },
            Held {
                entered: entered_seen,
                release,
            },
        )
    }

    impl Gate {
        fn pass(self) {
            let _unwatched = self.entered.send(());
            match self.release.recv_timeout(BOUND) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => panic!("the test released the gate in time"),
            }
        }
    }

    impl Held {
        fn entered(&self) {
            self.entered
                .recv_timeout(BOUND)
                .expect("the gated call came within the bound");
        }

        fn release(self) {
            let _released = self.release.send(());
        }
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Script>>);

    impl Fake {
        fn with(edit: impl FnOnce(&mut Script)) -> Self {
            let fake = Self::default();
            edit(&mut fake.script());
            fake
        }

        fn script(&self) -> MutexGuard<'_, Script> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn pass(gate: Option<Gate>) {
            if let Some(gate) = gate {
                gate.pass();
            }
        }

        fn until(&self, done: impl Fn(&Script) -> bool) {
            let by = Instant::now() + BOUND;
            while !done(&self.script()) {
                assert!(Instant::now() < by, "the owner got there within the bound");
                thread::sleep(POLL);
            }
        }
    }

    impl Os for Fake {
        fn auto_reaps(&self) -> bool {
            self.script().auto_reaps
        }

        fn spawn(&mut self) -> io::Result<()> {
            let gate = self.script().spawn_gate.take();
            Self::pass(gate);
            let mut script = self.script();
            script.spawns += 1;
            if script.spawn_fails {
                return Err(io::ErrorKind::NotFound.into());
            }
            Ok(())
        }

        fn poll(&mut self) -> Observation {
            let gate = self.script().poll_gate.take();
            Self::pass(gate);
            let mut script = self.script();
            if script.reaped {
                script.forbidden += 1;
                return Observation::Lost;
            }
            if script.lost {
                return Observation::Lost;
            }
            if let Some((at, exit)) = script.exits
                && Instant::now() >= at
            {
                script.reaped = true;
                return Observation::Reaped(exit);
            }
            if script.stopped {
                Observation::Stopped
            } else {
                Observation::Running
            }
        }

        fn kill(&mut self) -> io::Result<()> {
            let mut script = self.script();
            if script.reaped || script.lost {
                script.forbidden += 1;
            }
            script.kills += 1;
            script.killed_at = Some(Instant::now());
            let gate = script.kill_gate.take();
            drop(script);
            Self::pass(gate);
            let mut script = self.script();
            if script.kill_fails {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            if script.killable {
                script.exits = Some((Instant::now(), Exit::Signal(9)));
            }
            Ok(())
        }
    }

    fn next(owner: &ChildOwner) -> Report {
        owner
            .next(Instant::now() + BOUND)
            .expect("the owner reports within the bound")
    }

    fn started(fake: &Fake, word: &Arc<SpawnWord>) -> ChildOwner {
        let owner = ChildOwner::start(fake.clone(), Arc::clone(word), WAIT).expect("owner thread");
        assert!(matches!(next(&owner), Report::Spawned), "spawned");
        owner
    }

    /// Whether a fresh owner gets the claim. One that does keeps it, so call this last.
    fn claimable(word: &Arc<SpawnWord>) -> bool {
        let owner = ChildOwner::start(Fake::with(|s| s.lost = true), Arc::clone(word), WAIT)
            .expect("owner thread");
        match next(&owner) {
            Report::Spawned => true,
            Report::NotSpawned(Refusal::Claimed) => false,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_quit_before_the_claim_spawns_nothing() {
        let word = Arc::new(SpawnWord::default());
        word.quit();
        let fake = Fake::default();
        let owner = ChildOwner::start(fake.clone(), Arc::clone(&word), WAIT).expect("owner");
        assert!(matches!(
            next(&owner),
            Report::NotSpawned(Refusal::Quitting)
        ));
        assert_eq!(fake.script().spawns, 0, "no spawn after Quit");
    }

    #[test]
    fn a_quit_after_the_claim_retires_the_child_it_spawned() {
        let word = Arc::new(SpawnWord::default());
        let (gate, held) = gate();
        let fake = Fake::with(|s| {
            s.killable = true;
            s.spawn_gate = Some(gate);
        });
        let owner = ChildOwner::start(fake.clone(), Arc::clone(&word), WAIT).expect("owner");
        held.entered();
        word.quit();
        held.release();
        assert!(matches!(next(&owner), Report::Spawned));
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Signal(9)))
        ));
        let script = fake.script();
        assert_eq!((script.spawns, script.kills, script.forbidden), (1, 1, 0));
        drop(script);
        let later = ChildOwner::start(Fake::default(), word, WAIT).expect("owner");
        assert!(matches!(
            next(&later),
            Report::NotSpawned(Refusal::Quitting)
        ));
    }

    #[test]
    fn quit_racing_the_claim_never_leaves_a_live_child() {
        for _ in 0..40 {
            let word = Arc::new(SpawnWord::default());
            let fake = Fake::with(|s| s.killable = true);
            let barrier = Arc::new(Barrier::new(2));
            let quitter = {
                let (word, barrier) = (Arc::clone(&word), Arc::clone(&barrier));
                thread::spawn(move || {
                    barrier.wait();
                    word.quit();
                })
            };
            barrier.wait();
            let owner =
                ChildOwner::start(fake.clone(), Arc::clone(&word), Duration::ZERO).expect("owner");
            quitter.join().expect("quitter");
            match next(&owner) {
                Report::NotSpawned(Refusal::Quitting) => {
                    assert_eq!(fake.script().spawns, 0, "Quit first spawns nothing");
                }
                Report::Spawned => {
                    assert!(matches!(
                        next(&owner),
                        Report::Settled(Settled::Reaped(Exit::Signal(9)))
                    ));
                    assert!(fake.script().reaped, "the claim-first child was retired");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn a_failed_spawn_frees_the_claim_and_is_not_retried() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.spawn_fails = true);
        let owner = ChildOwner::start(fake.clone(), Arc::clone(&word), WAIT).expect("owner");
        assert!(matches!(
            next(&owner),
            Report::NotSpawned(Refusal::Spawn(error)) if error.kind() == io::ErrorKind::NotFound
        ));
        assert!(
            owner.next(Instant::now() + WAIT).is_none(),
            "nothing after a refusal"
        );
        assert_eq!(
            fake.script().spawns,
            1,
            "no second spawn without a new claim"
        );
        assert!(claimable(&word), "a fresh claim succeeds");
    }

    #[test]
    fn the_auto_reap_check_refuses_before_any_spawn() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.auto_reaps = true);
        let owner = ChildOwner::start(fake.clone(), Arc::clone(&word), WAIT).expect("owner");
        assert!(matches!(
            next(&owner),
            Report::NotSpawned(Refusal::AutoReap)
        ));
        assert_eq!(fake.script().spawns, 0, "nothing spawned");
        assert!(claimable(&word), "the claim was given back");
    }

    #[test]
    fn an_auto_reap_set_after_the_spawn_stops_the_signal() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.killable = true);
        let owner = started(&fake, &word);
        fake.script().auto_reaps = true;
        owner.retire();
        assert!(matches!(next(&owner), Report::Settled(Settled::Unsettled)));
        assert_eq!(fake.script().kills, 0, "never signalled");
        assert!(!claimable(&word), "nothing replaces an unsettled child");
        fake.script().exits = Some((Instant::now(), Exit::Code(0)));
        fake.until(|s| s.reaped);
    }

    #[test]
    fn a_held_child_blocks_every_other_claim_until_it_is_reaped() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.killable = true);
        let owner = started(&fake, &word);
        let second = Fake::default();
        let refused = ChildOwner::start(second.clone(), Arc::clone(&word), WAIT).expect("owner");
        assert!(matches!(
            next(&refused),
            Report::NotSpawned(Refusal::Claimed)
        ));
        assert_eq!(second.script().spawns, 0, "no second child");
        owner.retire();
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Signal(9)))
        ));
        assert!(claimable(&word), "a replacement may claim after the reap");
    }

    #[test]
    fn an_exit_without_quit_is_reaped_and_frees_the_claim() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.exits = Some((Instant::now(), Exit::Code(3))));
        let owner = started(&fake, &word);
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(3)))
        ));
        thread::sleep(WAIT);
        let script = fake.script();
        assert_eq!((script.kills, script.forbidden), (0, 0), "never signalled");
        drop(script);
        assert!(claimable(&word), "the claim is free after the reap");
    }

    #[test]
    fn an_exit_due_by_the_quit_deadline_is_reaped_without_a_signal() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.killable = true);
        let owner = started(&fake, &word);
        // The owner's deadline is at least this, so the exit is due by its kill turn.
        fake.script().exits = Some((Instant::now() + WAIT, Exit::Code(0)));
        word.quit();
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Code(0)))
        ));
        assert_eq!(fake.script().kills, 0, "no signal around the reap");
    }

    #[test]
    fn the_grace_starts_when_quit_is_seen_not_when_a_slow_poll_began() {
        let word = Arc::new(SpawnWord::default());
        let (gate, held) = gate();
        let fake = Fake::with(|s| s.killable = true);
        let owner = started(&fake, &word);
        fake.script().poll_gate = Some(gate);
        held.entered();
        thread::sleep(WAIT * 2);
        let quit_at = Instant::now();
        word.quit();
        held.release();
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Signal(9)))
        ));
        let killed_at = fake.script().killed_at.expect("killed");
        assert!(killed_at >= quit_at + WAIT, "the whole grace after Quit");
    }

    #[test]
    fn the_wait_after_a_slow_kill_starts_when_the_kill_returns() {
        let word = Arc::new(SpawnWord::default());
        let (gate, held) = gate();
        let fake = Fake::with(|s| {
            s.kill_fails = true;
            s.kill_gate = Some(gate);
        });
        let owner = started(&fake, &word);
        owner.retire();
        held.entered();
        thread::sleep(WAIT * 2);
        let returned_at = Instant::now();
        held.release();
        assert!(matches!(next(&owner), Report::Settled(Settled::Unsettled)));
        assert!(
            Instant::now() >= returned_at + WAIT,
            "the whole wait after the kill"
        );
        fake.script().exits = Some((Instant::now(), Exit::Code(0)));
        fake.until(|s| s.reaped);
    }

    #[test]
    fn a_stopped_child_is_never_reaped_and_is_killed_at_the_deadline() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| {
            s.stopped = true;
            s.killable = true;
        });
        let owner = started(&fake, &word);
        let retired_at = Instant::now();
        owner.retire();
        assert!(matches!(
            next(&owner),
            Report::Settled(Settled::Reaped(Exit::Signal(9)))
        ));
        let script = fake.script();
        assert_eq!(script.kills, 1, "one kill");
        let killed_at = script.killed_at.expect("killed");
        assert!(
            killed_at >= retired_at + WAIT,
            "not killed before the deadline"
        );
    }

    #[test]
    fn a_failed_kill_is_not_retried_and_leaves_custody_unsettled() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.kill_fails = true);
        let owner = started(&fake, &word);
        let retired_at = Instant::now();
        owner.retire();
        assert!(matches!(next(&owner), Report::Settled(Settled::Unsettled)));
        assert!(
            Instant::now() >= retired_at + WAIT * 2,
            "unsettled only after both waits"
        );
        thread::sleep(WAIT * 2);
        assert_eq!(fake.script().kills, 1, "no second signal");
        assert!(!claimable(&word), "nothing replaces an unsettled child");
        fake.script().exits = Some((Instant::now(), Exit::Code(0)));
        fake.until(|s| s.reaped);
        assert!(
            owner.next(Instant::now() + WAIT).is_none(),
            "unsettled is final"
        );
        assert_eq!(fake.script().forbidden, 0, "no poll after the late reap");
        assert!(!claimable(&word), "a late reap frees no claim");
    }

    #[test]
    fn a_lost_child_is_never_signalled_or_replaced() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::default();
        let owner = started(&fake, &word);
        owner.retire();
        fake.script().lost = true;
        assert!(matches!(next(&owner), Report::Settled(Settled::Lost)));
        thread::sleep(WAIT * 2);
        assert_eq!(fake.script().kills, 0, "never signalled after Lost");
        assert!(!claimable(&word), "nothing replaces a lost child");
    }

    #[test]
    fn dropping_the_owner_retires_a_live_child() {
        let word = Arc::new(SpawnWord::default());
        let fake = Fake::with(|s| s.killable = true);
        drop(started(&fake, &word));
        fake.until(|s| s.reaped);
        let freed_by = Instant::now() + BOUND;
        while !claimable(&word) {
            assert!(
                Instant::now() < freed_by,
                "the claim is freed after the reap"
            );
        }
        let script = fake.script();
        assert_eq!(
            (script.kills, script.forbidden),
            (1, 0),
            "killed once at the deadline"
        );
    }

    #[test]
    fn only_a_reap_with_exit_code_0_is_clean() {
        assert!(Settled::Reaped(Exit::Code(0)).is_clean(), "exit 0");
        for settled in [
            Settled::Reaped(Exit::Code(3)),
            Settled::Reaped(Exit::Signal(9)),
            Settled::Lost,
            Settled::Unsettled,
        ] {
            assert!(!settled.is_clean(), "{settled:?}");
        }
    }

    #[test]
    fn a_spawn_late_past_the_wait_is_pending_and_its_child_is_retired() {
        let word = Arc::new(SpawnWord::default());
        let (gate, held) = gate();
        let fake = Fake::with(|s| {
            s.killable = true;
            s.spawn_gate = Some(gate);
        });
        let owner = ChildOwner::start(fake.clone(), Arc::clone(&word), WAIT).expect("owner");
        held.entered();
        let by = Instant::now() + WAIT;
        assert!(matches!(owner.spawned(by), Err(RecvTimeoutError::Timeout)));
        assert!(Instant::now() >= by, "waited until the deadline");
        drop(owner);
        held.release();
        assert!(!word.claim(), "the late child holds the claim");
        fake.until(|s| s.reaped);
        let script = fake.script();
        assert_eq!((script.spawns, script.kills, script.forbidden), (1, 1, 0));
    }

    /// Delegates to a fake, and is slow to drop, so a refusal heard before the drop shows.
    struct SlowDrop(Fake, Arc<std::sync::atomic::AtomicBool>);

    impl Os for SlowDrop {
        fn auto_reaps(&self) -> bool {
            self.0.auto_reaps()
        }
        fn spawn(&mut self) -> io::Result<()> {
            self.0.spawn()
        }
        fn poll(&mut self) -> Observation {
            self.0.poll()
        }
        fn kill(&mut self) -> io::Result<()> {
            self.0.kill()
        }
    }

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            thread::sleep(WAIT);
            self.1.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_refusal_is_heard_only_after_the_adapter_is_gone() {
        for refusal in ["quit", "claimed", "auto-reap"] {
            let word = Arc::new(SpawnWord::default());
            let _holder = (refusal == "claimed").then(|| started(&Fake::default(), &word));
            if refusal == "quit" {
                word.quit();
            }
            let fake = Fake::with(|s| s.auto_reaps = refusal == "auto-reap");
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let os = SlowDrop(fake, Arc::clone(&dropped));
            let owner = ChildOwner::start(os, Arc::clone(&word), WAIT).expect("owner");
            assert!(
                matches!(
                    owner.spawned(Instant::now() + BOUND),
                    Ok(Report::NotSpawned(_))
                ),
                "{refusal}"
            );
            assert!(
                dropped.load(Ordering::SeqCst),
                "{refusal}: dropped before the report"
            );
        }
    }
}
