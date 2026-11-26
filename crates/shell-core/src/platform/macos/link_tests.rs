//! The link over the pair with a scripted brain and child: custody settles before any
//! replacement, Busy and Lost refuse, Quit retires the child, terminal custody ends the pair, and
//! each Quit's outcome belongs to the attempt it ended.
//! Synthetic sockets and a fake child only: nothing is spawned and no Keychain is read.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::TryRecvError;
use kumochi_protocol::constants::SESSION_READY;
use kumochi_protocol::message::{AudioSession, ControlToBrain};
use kumochi_session::Channel;

use super::leg::tests::{Commit, SECRET, ends, scripted};
use super::{NotStarted, ShellEnds};
use crate::link::{
    DrawingText, LinkEvent, LinkHandle, LinkState, QuitEnd, QuitOutcome, start_scripted,
};
use crate::platform::Connection;
use crate::platform::child::{
    ChildOwner, Exit, Observation, Os, Refusal, Report, Settled, SpawnWord, Unconnected,
};
use crate::platform::connect::{Failed, LinkError, connect_pair_with};

/// The owner's grace before a kill, and its wait after one.
const WAIT: Duration = Duration::from_millis(300);
const BOUND: Duration = Duration::from_secs(10);
/// How long a brain waits to see the shell shut its ends down before it goes on regardless.
const SHUTDOWN_SEEN: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Script {
    spawns: u32,
    kills: u32,
    exit: Option<Exit>,
    lost: bool,
    kill_fails: bool,
    /// A kill that blocks until this is released or dropped.
    kill_blocks: Option<Receiver<()>>,
    /// The next poll says it entered, then blocks until released or dropped.
    poll_blocks: Option<(mpsc::Sender<()>, Receiver<()>)>,
    killed: Option<mpsc::Sender<()>>,
    /// The owner's grace and wait in place of [`WAIT`].
    grace: Option<Duration>,
    /// Told when the brain reaches the point its script names.
    reached: Option<mpsc::Sender<()>>,
}

/// A fake child: running until it exits or is killed; a kill ends it with SIGKILL and tells
/// the brain thread, which then closes its ends as a dead process's would close. A refused kill
/// changes nothing.
#[derive(Clone, Default)]
struct Child(Arc<Mutex<Script>>);

impl Child {
    fn script(&self) -> MutexGuard<'_, Script> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn exits(&self, exit: Exit) {
        self.script().exit.get_or_insert(exit);
    }

    fn on_kill(&self) -> Receiver<()> {
        let (killed, seen) = mpsc::channel();
        self.script().killed = Some(killed);
        seen
    }
}

impl Os for Child {
    fn auto_reaps(&self) -> bool {
        false
    }

    fn spawn(&mut self) -> io::Result<()> {
        self.script().spawns += 1;
        Ok(())
    }

    fn poll(&mut self) -> Observation {
        let blocks = self.script().poll_blocks.take();
        if let Some((entered, release)) = blocks {
            let _seen = entered.send(());
            let _released = release.recv_timeout(BOUND);
        }
        let script = self.script();
        if script.lost {
            return Observation::Lost;
        }
        script
            .exit
            .map_or(Observation::Running, Observation::Reaped)
    }

    fn kill(&mut self) -> io::Result<()> {
        let blocks = self.script().kill_blocks.take();
        if let Some(blocks) = blocks {
            let _released = blocks.recv_timeout(BOUND);
        }
        let mut script = self.script();
        script.kills += 1;
        if script.kill_fails {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        script.exit.get_or_insert(Exit::Signal(9));
        if let Some(killed) = script.killed.take() {
            let _unwatched = killed.send(());
        }
        Ok(())
    }
}

/// What the scripted brain does.
#[derive(Clone, Copy)]
enum Then {
    /// Never writes READY; closes when killed.
    NoReady,
    /// Exits with this code before READY, as a brain whose instance another session holds.
    ExitsBeforeReady(i32),
    /// Writes READY and closes, so the initiator fails.
    ClosesAfterReady,
    /// Commits, then reads until `Quitting` and exits with this code.
    QuitExits(i32),
    /// Commits, then ignores everything until killed.
    Stalls,
    /// Commits, then closes while the child lives on until killed.
    Closes,
    /// Commits, then exits with this code on its own.
    ExitsAfterCommit(i32),
    /// Commits, then closes as the child's custody is lost.
    LostCloses,
    /// Writes READY, reads the first byte of the initiator's opening, says so and holds both ends
    /// open until killed.
    HoldsInHandshake,
    /// Waits to see the shell's ends shut down, says so, writes READY all the same and holds both
    /// ends open until killed.
    ReadyAfterShutdown,
    /// Writes READY, waits to see the shell's ends shut down, says so and holds both ends open
    /// until killed.
    ReadyBeforeShutdown,
}

fn brain(child: &Child, then: Then) -> ShellEnds {
    let (shell, control, data) = ends();
    let child = child.clone();
    let killed = child.on_kill();
    let until_killed = move || {
        let _seen = killed.recv_timeout(BOUND);
    };
    thread::spawn(move || {
        let reached = || {
            if let Some(reached) = child.script().reached.take() {
                let _unwatched = reached.send(());
            }
        };
        match then {
            Then::NoReady => return until_killed(),
            Then::ExitsBeforeReady(code) => return child.exits(Exit::Code(code)),
            Then::ReadyAfterShutdown => {
                shut_down_seen(&control);
                reached();
                // The shell shut its end down, so the byte has nowhere to go.
                let _refused = (&control).write_all(&[SESSION_READY]);
                return until_killed();
            }
            _ => {}
        }
        (&control).write_all(&[SESSION_READY]).expect("READY");
        match then {
            Then::ClosesAfterReady => return,
            Then::HoldsInHandshake => {
                let mut opening = [0];
                (&control)
                    .read_exact(&mut opening)
                    .expect("the initiator's first byte");
                reached();
                return until_killed();
            }
            Then::ReadyBeforeShutdown => {
                shut_down_seen(&control);
                reached();
                return until_killed();
            }
            _ => {}
        }
        let deadline = Instant::now() + BOUND;
        let mut brain = scripted(
            control,
            data,
            AudioSession::Legacy,
            Commit::Settings,
            deadline,
        );
        match then {
            Then::QuitExits(code) => {
                while brain.control_message() != ControlToBrain::Quitting {}
                child.exits(Exit::Code(code));
            }
            Then::Stalls => until_killed(),
            Then::ExitsAfterCommit(code) => child.exits(Exit::Code(code)),
            Then::LostCloses => child.script().lost = true,
            Then::Closes
            | Then::NoReady
            | Then::ExitsBeforeReady(_)
            | Then::ClosesAfterReady
            | Then::HoldsInHandshake
            | Then::ReadyAfterShutdown
            | Then::ReadyBeforeShutdown => {}
        }
        drop(brain);
    });
    shell
}

/// Waits until the shell's end of `control` is shut down, or [`SHUTDOWN_SEEN`] has passed.
fn shut_down_seen(control: &UnixStream) {
    control
        .set_read_timeout(Some(SHUTDOWN_SEEN))
        .expect("bounded");
    let mut byte = [0];
    // EOF is the shutdown; a timeout means it never came, which the test then shows.
    let _seen = (&*control).read(&mut byte);
}

fn owned(child: &Child, word: &Arc<SpawnWord>) -> Result<ChildOwner, Refusal> {
    let wait = child.script().grace.unwrap_or(WAIT);
    let owner = ChildOwner::start(child.clone(), Arc::clone(word), wait).map_err(Refusal::Spawn)?;
    match owner.spawned(Instant::now() + BOUND) {
        Ok(Report::Spawned) => Ok(owner),
        Ok(Report::NotSpawned(refusal)) => Err(refusal),
        Ok(Report::Settled(_)) | Err(_) => Err(Refusal::Spawn(io::ErrorKind::BrokenPipe.into())),
    }
}

fn attempt(
    word: &Arc<SpawnWord>,
    child: &Child,
    then: Then,
    by: Instant,
    read_item: impl FnOnce(
        &mut [u8; kumochi_session::SECRET_LEN],
    ) -> Result<(), kumochi_storage::ItemError>,
) -> Result<(Connection, ChildOwner), Failed> {
    attempt_with(word, child, then, by, || Ok(()), read_item)
}

/// [`attempt`] with `harden` in place of a hardening that does nothing.
fn attempt_with(
    word: &Arc<SpawnWord>,
    child: &Child,
    then: Then,
    by: Instant,
    harden: impl FnOnce() -> io::Result<()>,
    read_item: impl FnOnce(
        &mut [u8; kumochi_session::SECRET_LEN],
    ) -> Result<(), kumochi_storage::ItemError>,
) -> Result<(Connection, ChildOwner), Failed> {
    connect_pair_with(
        |_| {
            let owner = owned(child, word).map_err(NotStarted::Refused)?;
            Ok((owner, brain(child, then)))
        },
        harden,
        read_item,
        Channel::Dev,
        AudioSession::Legacy,
        None,
        by,
        |_| DrawingText::default(),
    )
}

#[expect(clippy::unnecessary_wraps, reason = "it stands in for the item read")]
fn honest(item: &mut [u8; kumochi_session::SECRET_LEN]) -> Result<(), kumochi_storage::ItemError> {
    *item = SECRET;
    Ok(())
}

type Attempt = Result<(Connection, Option<ChildOwner>), Failed>;

#[expect(
    clippy::unnecessary_wraps,
    reason = "it stands in for the accessor's own value"
)]
fn ended(generation: Option<u64>, outcome: QuitOutcome) -> Option<QuitEnd> {
    Some(QuitEnd {
        generation,
        outcome,
    })
}

/// A link whose first attempt meets `child` scripted by `then`; later attempts come from `next`.
fn link(
    child: &Child,
    then: Then,
    mut next: impl FnMut(&Arc<SpawnWord>, Instant) -> Attempt + Send + 'static,
) -> (LinkHandle, async_channel::Receiver<LinkEvent>) {
    let (events, states) = async_channel::unbounded();
    let child = child.clone();
    let mut first = true;
    let handle = start_scripted(events, AudioSession::Legacy, move |word, _, by| {
        if std::mem::take(&mut first) {
            return attempt(word, &child, then, by, honest).map(|(c, owner)| (c, Some(owner)));
        }
        next(word, by)
    });
    (handle, states)
}

fn until(states: &async_channel::Receiver<LinkEvent>, wanted: impl Fn(&LinkEvent) -> bool) {
    let by = Instant::now() + BOUND;
    loop {
        match states.try_recv() {
            Ok(event) if wanted(&event) => return,
            Ok(_) => {}
            Err(TryRecvError::Empty) if Instant::now() < by => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the event didn't come in time: {error:?}"),
        }
    }
}

fn connected(event: &LinkEvent) -> bool {
    matches!(event, LinkEvent::Connected(_))
}

fn failed(event: &LinkEvent) -> bool {
    matches!(event, LinkEvent::State(LinkState::Failed))
}

fn refused(_: &Arc<SpawnWord>, _: Instant) -> Attempt {
    Err(LinkError::Rejected.into())
}

#[test]
fn a_quit_taken_by_a_child_that_exits_0_is_clean() {
    let child = Child::default();
    let (link, states) = link(&child, Then::QuitExits(0), refused);
    until(&states, connected);
    assert!(link.begin_quit());
    link.finish_quit();
    assert_eq!(link.quit_end(), ended(Some(1), QuitOutcome::Clean));
    let script = child.script();
    assert_eq!((script.exit, script.kills), (Some(Exit::Code(0)), 0));
}

#[test]
fn a_quit_is_unclean_unless_the_live_child_exits_0() {
    for (then, exit, kills) in [
        (Then::QuitExits(1), Exit::Code(1), 0),
        (Then::Stalls, Exit::Signal(9), 1),
    ] {
        let child = Child::default();
        let (link, states) = link(&child, then, refused);
        until(&states, connected);
        assert!(link.begin_quit());
        link.finish_quit();
        assert_eq!(link.quit_end(), ended(Some(1), QuitOutcome::Reaped(exit)));
        let script = child.script();
        assert_eq!((script.exit, script.kills), (Some(exit), kills));
    }
}

#[test]
fn a_quit_between_attempts_after_a_child_exited_0_is_not_clean() {
    let child = Child::default();
    let (link, states) = link(&child, Then::ExitsAfterCommit(0), refused);
    until(&states, connected);
    until(&states, failed);
    assert!(link.begin_quit());
    link.finish_quit();
    assert_eq!(link.quit_end(), None);
}

#[test]
fn a_quit_outcome_is_the_attempt_quit_ended_not_an_earlier_childs_clean_exit() {
    let (first, second) = (Child::default(), Child::default());
    let (link, states) = link(&first, Then::ExitsAfterCommit(0), move |word, by| {
        attempt(word, &second, Then::QuitExits(3), by, honest).map(|(c, owner)| (c, Some(owner)))
    });
    until(&states, connected);
    until(&states, connected);
    assert!(link.begin_quit());
    link.finish_quit();
    assert_eq!(
        link.quit_end(),
        ended(Some(2), QuitOutcome::Reaped(Exit::Code(3)))
    );
    assert_eq!(first.script().exit, Some(Exit::Code(0)));
}

#[test]
fn a_quit_queued_but_never_written_is_not_clean_on_an_exit_0() {
    let child = Child::default();
    // The test decides when the child exits, so no kill may come first.
    child.script().grace = Some(BOUND);
    let (link, states) = link(&child, Then::Stalls, refused);
    until(&states, connected);
    let hold = link.control_writer_hold();
    hold.store(1, Ordering::Release);
    assert!(link.begin_quit());
    thread::scope(|scope| {
        let quit = scope.spawn(|| link.finish_quit());
        let by = Instant::now() + BOUND;
        while hold.load(Ordering::Acquire) != 2 {
            assert!(
                Instant::now() < by,
                "Quitting reached the writer, which holds it unwritten"
            );
            thread::sleep(Duration::from_millis(2));
        }
        child.exits(Exit::Code(0));
        quit.join().expect("finish_quit");
    });
    assert_eq!(
        link.quit_end(),
        ended(Some(1), QuitOutcome::Reaped(Exit::Code(0)))
    );
    assert_eq!(child.script().kills, 0);
}

#[test]
fn a_refused_kill_ends_the_link_while_the_peer_holds_both_ends_open() {
    let child = Child::default();
    child.script().kill_fails = true;
    let (link, states) = link(&child, Then::Stalls, refused);
    until(&states, connected);
    let quit = Instant::now();
    assert!(link.begin_quit());
    link.finish_quit();
    let took = quit.elapsed();
    assert_eq!(link.quit_end(), ended(Some(1), QuitOutcome::Unsettled));
    assert!(took < 2 * WAIT + Duration::from_secs(2), "{took:?}");
    let script = child.script();
    assert_eq!((script.exit, script.kills), (None, 1));
}

#[test]
fn a_kill_that_blocks_ends_the_link_unsettled_while_the_peer_holds_both_ends_open() {
    let child = Child::default();
    let (unblock, blocks) = mpsc::channel();
    child.script().kill_blocks = Some(blocks);
    let (link, states) = link(&child, Then::Stalls, refused);
    until(&states, connected);
    let quit = Instant::now();
    assert!(link.begin_quit());
    link.finish_quit();
    let took = quit.elapsed();
    assert_eq!(link.quit_end(), ended(Some(1), QuitOutcome::Unsettled));
    assert!(took < 4 * WAIT + Duration::from_secs(2), "{took:?}");
    assert_eq!(child.script().exit, None, "the kill is still blocked");
    drop(unblock);
}

#[test]
fn a_quit_during_a_failed_spawn_tells_a_pending_spawn_from_none() {
    for (pending, outcome) in [
        (true, QuitOutcome::SpawnPending),
        (false, QuitOutcome::NotSpawned),
    ] {
        let (entered, attempting) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let (events, _states) = async_channel::unbounded();
        let link = start_scripted(events, AudioSession::Legacy, move |_, _, by| {
            let _seen = entered.send(());
            let _released = released.recv_timeout(BOUND);
            connect_pair_with(
                |_| {
                    Err(if pending {
                        NotStarted::Pending
                    } else {
                        NotStarted::Refused(Refusal::Quitting)
                    })
                },
                || Ok(()),
                honest,
                Channel::Dev,
                AudioSession::Legacy,
                None,
                by,
                |_| DrawingText::default(),
            )
            .map(|(connection, owner)| (connection, Some(owner)))
        });
        attempting.recv_timeout(BOUND).expect("the attempt started");
        assert!(link.begin_quit());
        drop(release);
        link.finish_quit();
        assert_eq!(link.quit_end(), ended(None, outcome), "pending {pending}");
    }
}

#[test]
fn a_poll_that_blocks_ends_the_link_unsettled_while_the_peer_holds_both_ends_open() {
    let child = Child::default();
    let (link, states) = link(&child, Then::Stalls, refused);
    until(&states, connected);
    let (entered, polling) = mpsc::channel();
    let (unblock, blocks) = mpsc::channel();
    child.script().poll_blocks = Some((entered, blocks));
    polling
        .recv_timeout(BOUND)
        .expect("the owner is inside its poll");
    let quit = Instant::now();
    assert!(link.begin_quit());
    link.finish_quit();
    let took = quit.elapsed();
    assert_eq!(link.quit_end(), ended(Some(1), QuitOutcome::Unsettled));
    assert!(took < 3 * WAIT + Duration::from_secs(2), "{took:?}");
    assert_eq!(child.script().kills, 0, "the owner never got past its poll");
    drop(unblock);
}

/// A brain for the queued pair that says READY only once `go` comes, then exits 0 on Quit.
fn parked_brain(child: &Child, go: Receiver<()>) -> ShellEnds {
    let (shell, control, data) = ends();
    let child = child.clone();
    thread::spawn(move || {
        let _go = go.recv_timeout(BOUND);
        (&control).write_all(&[SESSION_READY]).expect("READY");
        let deadline = Instant::now() + BOUND;
        let mut brain = scripted(
            control,
            data,
            AudioSession::QueuedRequired,
            Commit::Settings,
            deadline,
        );
        while brain.control_message() != ControlToBrain::Quitting {}
        child.exits(Exit::Code(0));
        drop(brain);
    });
    shell
}

#[test]
fn a_quit_parked_before_the_queued_writers_start_is_clean_on_its_own_receipt() {
    let child = Child::default();
    // The handshake under Quit takes its own time, so no kill may come first.
    child.script().grace = Some(BOUND);
    let (go, gate) = mpsc::channel();
    let (events, _states) = async_channel::unbounded();
    let mut gate = Some(gate);
    let brain_child = child.clone();
    let link = start_scripted(events, AudioSession::QueuedRequired, move |word, _, by| {
        let gate = gate.take().ok_or(LinkError::Rejected)?;
        connect_pair_with(
            |_| {
                let owner = owned(&brain_child, word).map_err(NotStarted::Refused)?;
                Ok((owner, parked_brain(&brain_child, gate)))
            },
            || Ok(()),
            honest,
            Channel::Dev,
            AudioSession::QueuedRequired,
            None,
            by,
            |_| DrawingText::default(),
        )
        .map(|(connection, owner)| (connection, Some(owner)))
    });
    let by = Instant::now() + BOUND;
    while child.script().spawns == 0 {
        assert!(Instant::now() < by, "the child started");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(link.begin_quit());
    go.send(()).expect("the brain waits for its go");
    link.finish_quit();
    assert_eq!(link.quit_end(), ended(None, QuitOutcome::Clean));
    let script = child.script();
    assert_eq!((script.exit, script.kills), (Some(Exit::Code(0)), 0));
}

#[test]
fn a_replacement_starts_only_after_the_lost_connections_child_is_reaped() {
    let first = Child::default();
    let seen = Arc::new(Mutex::new(None));
    let (link, states) = {
        let (earlier, second, seen) = (first.clone(), Child::default(), Arc::clone(&seen));
        link(&first, Then::Closes, move |word, _| {
            let reaped = earlier.script().exit;
            let claimed = owned(&second, word)
                .map(drop)
                .map_err(|refusal| refusal.to_string());
            *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some((reaped, claimed));
            Err(LinkError::Rejected.into())
        })
    };
    until(&states, failed);
    assert_eq!(
        seen.lock().unwrap_or_else(PoisonError::into_inner).take(),
        Some((Some(Exit::Signal(9)), Ok(())))
    );
    assert_eq!(first.script().kills, 1);
    assert!(link.begin_quit());
    link.finish_quit();
}

#[test]
fn after_a_lost_child_no_attempt_spawns_again_even_on_retry() {
    let first = Child::default();
    let second = Child::default();
    let refusals = Arc::new(AtomicUsize::new(0));
    let (link, states) = {
        let (second, refusals) = (second.clone(), Arc::clone(&refusals));
        link(&first, Then::LostCloses, move |word, _| {
            if matches!(owned(&second, word), Err(Refusal::Claimed)) {
                refusals.fetch_add(1, Ordering::SeqCst);
            }
            Err(LinkError::Launch(io::ErrorKind::ResourceBusy.into()).into())
        })
    };
    until(&states, failed);
    assert!(link.retry());
    until(&states, failed);
    assert_eq!(refusals.load(Ordering::SeqCst), 2);
    assert_eq!((second.script().spawns, first.script().kills), (0, 0));
    assert!(link.begin_quit());
    link.finish_quit();
}

#[test]
fn a_child_that_exits_busy_before_ready_is_busy_and_any_other_exit_is_rejected() {
    for code in [75, 76] {
        let child = Child::default();
        let word = Arc::new(SpawnWord::default());
        let result = attempt(
            &word,
            &child,
            Then::ExitsBeforeReady(code),
            Instant::now() + BOUND,
            honest,
        );
        let reaped = Some(Unconnected::Settled(Settled::Reaped(Exit::Code(code))));
        match (code, result) {
            (
                75,
                Err(Failed {
                    error: LinkError::Busy,
                    child,
                }),
            )
            | (
                76,
                Err(Failed {
                    error: LinkError::Rejected,
                    child,
                }),
            ) if child == reaped => {}
            (_, other) => panic!("exit {code}: {:?}", other.map(drop)),
        }
        assert_eq!(child.script().kills, 0, "exit {code}");
    }
}

#[test]
fn quit_while_the_brain_hasnt_said_ready_retires_its_child_well_before_the_deadline() {
    let child = Child::default();
    let (link, _states) = link(&child, Then::NoReady, refused);
    let by = Instant::now() + BOUND;
    while child.script().spawns == 0 {
        assert!(Instant::now() < by, "the child started");
        thread::sleep(Duration::from_millis(5));
    }
    let quit = Instant::now();
    assert!(link.begin_quit());
    link.finish_quit();
    let took = quit.elapsed();
    assert!(took < 2 * WAIT + Duration::from_secs(2), "{took:?}");
    let script = child.script();
    assert_eq!((script.exit, script.kills), (Some(Exit::Signal(9)), 1));
    assert_eq!(
        link.quit_end(),
        ended(None, QuitOutcome::Reaped(Exit::Signal(9)))
    );
}

#[test]
fn an_attempt_that_expires_before_ready_returns_only_once_its_child_is_reaped() {
    let child = Child::default();
    let word = Arc::new(SpawnWord::default());
    let start = Instant::now();
    let result = attempt(&word, &child, Then::NoReady, start + WAIT, honest);
    assert!(matches!(
        result,
        Err(Failed {
            error: LinkError::ConnectExpired,
            child: Some(Unconnected::Settled(Settled::Reaped(Exit::Signal(9)))),
        })
    ));
    assert_eq!(
        child.script().exit,
        Some(Exit::Signal(9)),
        "settled before the return"
    );
    assert!(start.elapsed() < 3 * WAIT + Duration::from_secs(2));
    assert!(word.claim(), "the reap freed the claim");
}

#[test]
fn a_refused_attempt_names_none_of_the_item_bytes() {
    const SENTINEL: [u8; kumochi_session::SECRET_LEN] = *b"SENTINEL-ITEM-BYTES-0123456789AB";
    let child = Child::default();
    let word = Arc::new(SpawnWord::default());
    let error = attempt(
        &word,
        &child,
        Then::ClosesAfterReady,
        Instant::now() + BOUND,
        |item| {
            *item = SENTINEL;
            Ok(())
        },
    )
    .map(drop)
    .expect_err("the initiator fails");
    for shown in [format!("{error:?}"), error.error.to_string()] {
        assert!(!shown.contains("SENTINEL"), "as text: {shown}");
        assert!(!shown.contains("83, 69, 78"), "as a byte list: {shown}");
    }
}

/// How a test ends custody while an attempt runs.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// Waiting on the child fails.
    Lost,
    /// Quit, then a refused kill outlasts the owner's wait.
    Unsettled,
}

impl Ending {
    fn child(self) -> Child {
        let child = Child::default();
        child.script().kill_fails = matches!(self, Self::Unsettled);
        child
    }

    fn end(self, child: &Child, word: &SpawnWord) {
        match self {
            Self::Lost => child.script().lost = true,
            Self::Unsettled => word.quit(),
        }
    }

    fn settled(self) -> Settled {
        match self {
            Self::Lost => Settled::Lost,
            Self::Unsettled => Settled::Unsettled,
        }
    }
}

/// What an attempt that ended under a test did.
struct Ended {
    error: LinkError,
    took: Duration,
    reads: usize,
}

/// Runs one attempt against `child` scripted by `then` on its own thread, with `harden` and an
/// item read that counts and then runs `reading`, while `during` ends custody on the test's
/// thread. The attempt has [`BOUND`] and must fail with custody ended as `ending` says.
fn attempt_while(
    ending: Ending,
    child: &Child,
    then: Then,
    harden: impl FnOnce() -> io::Result<()> + Send,
    reading: impl FnOnce() + Send,
    during: impl FnOnce(&Arc<SpawnWord>),
) -> Ended {
    let word = Arc::new(SpawnWord::default());
    let reads = AtomicUsize::new(0);
    let start = Instant::now();
    let result = thread::scope(|scope| {
        let attempt = scope.spawn(|| {
            attempt_with(&word, child, then, start + BOUND, harden, |item| {
                reads.fetch_add(1, Ordering::SeqCst);
                reading();
                honest(item)
            })
            .map(drop)
        });
        during(&word);
        attempt.join().expect("the attempt returned")
    });
    let took = start.elapsed();
    match result {
        Err(Failed {
            error,
            child: Some(Unconnected::Settled(settled)),
        }) if settled == ending.settled() => Ended {
            error,
            took,
            reads: reads.load(Ordering::SeqCst),
        },
        other => panic!("{ending:?}: {other:?}"),
    }
}

fn spawned(child: &Child) {
    let by = Instant::now() + BOUND;
    while child.script().spawns == 0 {
        assert!(Instant::now() < by, "the child started");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Well before the attempt's [`BOUND`]: the owner's grace and wait, and slack for a busy host.
fn prompt(took: Duration) -> bool {
    took < 2 * WAIT + Duration::from_secs(2)
}

#[test]
fn custody_ending_before_ready_ends_the_attempt_and_reads_no_item() {
    for ending in [Ending::Lost, Ending::Unsettled] {
        let child = ending.child();
        let ended = attempt_while(
            ending,
            &child,
            Then::NoReady,
            || Ok(()),
            || {},
            |word| {
                spawned(&child);
                ending.end(&child, word);
            },
        );
        assert!(matches!(ended.error, LinkError::Rejected), "{ending:?}");
        assert!(prompt(ended.took), "{ending:?}: {:?}", ended.took);
        assert_eq!(ended.reads, 0, "{ending:?}");
    }
}

#[test]
fn custody_ending_during_the_handshake_ends_the_attempt() {
    for ending in [Ending::Lost, Ending::Unsettled] {
        let child = ending.child();
        let (reached, in_handshake) = mpsc::channel();
        child.script().reached = Some(reached);
        let ended = attempt_while(
            ending,
            &child,
            Then::HoldsInHandshake,
            || Ok(()),
            || {},
            |word| {
                in_handshake
                    .recv_timeout(BOUND)
                    .expect("the brain is in the handshake");
                ending.end(&child, word);
            },
        );
        let LinkError::Io(error) = &ended.error else {
            panic!("{ending:?}: {:?}", ended.error);
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{ending:?}");
        assert!(prompt(ended.took), "{ending:?}: {:?}", ended.took);
        assert_eq!(ended.reads, 1, "{ending:?}: read before custody ended");
    }
}

#[test]
fn no_item_is_read_once_custody_has_ended() {
    for ending in [Ending::Lost, Ending::Unsettled] {
        // A READY the brain sends only after custody ended never arrives.
        let child = ending.child();
        let (reached, shut) = mpsc::channel();
        child.script().reached = Some(reached);
        let ended = attempt_while(
            ending,
            &child,
            Then::ReadyAfterShutdown,
            || Ok(()),
            || {},
            |word| {
                spawned(&child);
                ending.end(&child, word);
                shut.recv_timeout(BOUND).expect("the brain looked");
            },
        );
        assert!(matches!(ended.error, LinkError::Rejected), "{ending:?}");
        assert_eq!(ended.reads, 0, "{ending:?}: a late READY");

        // A READY taken before custody ended reads nothing once it has.
        let child = ending.child();
        let (reached, shut) = mpsc::channel();
        child.script().reached = Some(reached);
        let (entered, hardening) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let ended = attempt_while(
            ending,
            &child,
            Then::ReadyBeforeShutdown,
            move || {
                let _seen = entered.send(());
                let _released = released.recv_timeout(BOUND);
                Ok(())
            },
            || {},
            |word| {
                hardening.recv_timeout(BOUND).expect("READY came");
                ending.end(&child, word);
                shut.recv_timeout(BOUND).expect("the brain looked");
                drop(release);
            },
        );
        assert!(matches!(ended.error, LinkError::Custody), "{ending:?}");
        assert_eq!(ended.reads, 0, "{ending:?}: custody ended while hardening");

        // An item read that outlasts custody doesn't count.
        let child = ending.child();
        let (reached, shut) = mpsc::channel();
        child.script().reached = Some(reached);
        let (entered, reading) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let ended = attempt_while(
            ending,
            &child,
            Then::ReadyBeforeShutdown,
            || Ok(()),
            move || {
                let _seen = entered.send(());
                let _released = released.recv_timeout(BOUND);
            },
            |word| {
                reading.recv_timeout(BOUND).expect("the item read began");
                ending.end(&child, word);
                shut.recv_timeout(BOUND).expect("the brain looked");
                drop(release);
            },
        );
        assert!(matches!(ended.error, LinkError::Custody), "{ending:?}");
        assert_eq!(ended.reads, 1, "{ending:?}: read once, then refused");
    }
}
