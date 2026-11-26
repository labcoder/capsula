//! The link over the pair with a scripted brain and child: custody settles before any
//! replacement, Busy and Lost refuse, Quit retires the child, terminal custody ends the pair, and
//! each Quit's outcome belongs to the attempt it ended.
//! Synthetic sockets and a fake child only: nothing is spawned and no Keychain is read.

use std::io::{self, Write as _};
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
use crate::link::{DrawingText, LinkEvent, LinkHandle, LinkState, QuitOutcome, start_scripted};
use crate::platform::Connection;
use crate::platform::child::{ChildOwner, Exit, Observation, Os, Refusal, Report, SpawnWord};
use crate::platform::connect::{LinkError, connect_pair_with};

/// The owner's grace before a kill, and its wait after one.
const WAIT: Duration = Duration::from_millis(300);
const BOUND: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Script {
    spawns: u32,
    kills: u32,
    exit: Option<Exit>,
    lost: bool,
    kill_fails: bool,
    killed: Option<mpsc::Sender<()>>,
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
        let script = self.script();
        if script.lost {
            return Observation::Lost;
        }
        script
            .exit
            .map_or(Observation::Running, Observation::Reaped)
    }

    fn kill(&mut self) -> io::Result<()> {
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
}

fn brain(child: &Child, then: Then) -> ShellEnds {
    let (shell, control, data) = ends();
    let child = child.clone();
    let killed = child.on_kill();
    let until_killed = move || {
        let _seen = killed.recv_timeout(BOUND);
    };
    thread::spawn(move || {
        match then {
            Then::NoReady => return until_killed(),
            Then::ExitsBeforeReady(code) => return child.exits(Exit::Code(code)),
            _ => {}
        }
        (&control).write_all(&[SESSION_READY]).expect("READY");
        if matches!(then, Then::ClosesAfterReady) {
            return;
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
            Then::Closes | Then::NoReady | Then::ExitsBeforeReady(_) | Then::ClosesAfterReady => {}
        }
        drop(brain);
    });
    shell
}

fn owned(child: &Child, word: &Arc<SpawnWord>) -> Result<ChildOwner, Refusal> {
    let owner = ChildOwner::start(child.clone(), Arc::clone(word), WAIT).map_err(Refusal::Spawn)?;
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
) -> Result<(Connection, ChildOwner), LinkError> {
    connect_pair_with(
        |_| {
            let owner = owned(child, word).map_err(NotStarted::Refused)?;
            Ok((owner, brain(child, then)))
        },
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

type Attempt = Result<(Connection, Option<ChildOwner>), LinkError>;

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
    Err(LinkError::Rejected)
}

#[test]
fn a_quit_taken_by_a_child_that_exits_0_is_clean() {
    let child = Child::default();
    let (link, states) = link(&child, Then::QuitExits(0), refused);
    until(&states, connected);
    assert!(link.begin_quit());
    link.finish_quit();
    assert_eq!(link.quit_outcome(), Some(QuitOutcome::Clean));
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
        assert_eq!(link.quit_outcome(), Some(QuitOutcome::Reaped(exit)));
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
    assert_eq!(link.quit_outcome(), None);
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
        link.quit_outcome(),
        Some(QuitOutcome::Reaped(Exit::Code(3)))
    );
    assert_eq!(first.script().exit, Some(Exit::Code(0)));
}

#[test]
fn a_quit_queued_but_never_written_is_not_clean_on_an_exit_0() {
    let child = Child::default();
    let (link, states) = link(&child, Then::Stalls, refused);
    until(&states, connected);
    let hold = &link.shared.hold_control_writer;
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
        link.quit_outcome(),
        Some(QuitOutcome::Reaped(Exit::Code(0)))
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
    assert_eq!(link.quit_outcome(), Some(QuitOutcome::Unsettled));
    assert!(took < 2 * WAIT + Duration::from_secs(2), "{took:?}");
    let script = child.script();
    assert_eq!((script.exit, script.kills), (None, 1));
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
            Err(LinkError::Rejected)
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
            Err(LinkError::Launch(io::ErrorKind::ResourceBusy.into()))
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
        match (code, result) {
            (75, Err(LinkError::Busy)) | (76, Err(LinkError::Rejected)) => {}
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
    assert_eq!(link.quit_outcome(), Some(QuitOutcome::NotConnected));
}

#[test]
fn an_attempt_that_expires_before_ready_returns_only_once_its_child_is_reaped() {
    let child = Child::default();
    let word = Arc::new(SpawnWord::default());
    let start = Instant::now();
    let result = attempt(&word, &child, Then::NoReady, start + WAIT, honest);
    assert!(matches!(result, Err(LinkError::ConnectExpired)));
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
    for shown in [format!("{error:?}"), error.to_string()] {
        assert!(!shown.contains("SENTINEL"), "as text: {shown}");
        assert!(!shown.contains("83, 69, 78"), "as a byte list: {shown}");
    }
}
