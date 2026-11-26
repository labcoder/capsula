//! Parent-bound serving on the inherited pair, driven through the root on the fixture stages:
//! `ShellTuning`, the text-metrics hand-over, holds without voice or screen, the parent's exit and
//! the order the legs close in.

use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream as StdStream;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, mpsc};
use std::task::Poll;
use std::time::Duration;

use kumochi_protocol::message::{
    CAPABILITY_LEN, ControlToBrain, ControlToShell, DataToBrain, IssueSession, RunId, ScreenChoice,
};
use kumochi_scene::display::TextMetricsV1;
use tokio::sync::{mpsc as async_mpsc, watch};
use tokio::time::Instant;

use super::admit_tests::{Item, inherited, parent};
use super::serve;
use super::tests::{SHORT, bounded, by, hello};
use crate::ParentBoundError;
use crate::platform::ShellProcess;
use crate::platform::shell_process::Verdict;
use crate::runtime::{Brain, Ended, Seen, Terminal};
use crate::startup::custody::SessionCustody;
use crate::startup::parent_bound::tests::{Holding, Script, run_on_fixtures};
use crate::startup::parent_bound::{AdmissionPair, Root};
use crate::test_support::Scratch;

/// A pair that hands the test its Brain's observer just before serving, and is otherwise `P`.
pub(crate) struct Observed<P> {
    pub(crate) pair: P,
    pub(crate) seen: mpsc::Sender<async_mpsc::UnboundedReceiver<Seen>>,
}

impl<P: AdmissionPair> AdmissionPair for Observed<P> {
    type Keyed = (P::Keyed, mpsc::Sender<async_mpsc::UnboundedReceiver<Seen>>);
    type Greeted = (
        P::Greeted,
        Option<mpsc::Sender<async_mpsc::UnboundedReceiver<Seen>>>,
    );

    fn key(self, key: &[u8; CAPABILITY_LEN]) -> Option<Self::Keyed> {
        Some((self.pair.key(key)?, self.seen))
    }

    async fn admit(
        (keyed, seen): Self::Keyed,
        shell: &ShellProcess,
        by: Instant,
        reserve: impl FnOnce() -> IssueSession,
    ) -> Option<Self::Greeted> {
        Some((P::admit(keyed, shell, by, reserve).await?, Some(seen)))
    }

    fn cell((greeted, _): &Self::Greeted) -> watch::Receiver<bool> {
        P::cell(greeted)
    }

    fn serve(
        (greeted, seen): &mut Self::Greeted,
        brain: &mut Brain,
        terminal: Terminal,
        shell: &ShellProcess,
        by: Instant,
    ) -> impl Future<Output = Ended> {
        if let Some(seen) = seen.take() {
            seen.send(brain.observe()).expect("the test listens");
        }
        P::serve(greeted, brain, terminal, shell, by)
    }
}

const STAGES: [&str; 4] = ["init", "open", "accounts", "containment"];

/// A valid profile no default matches, so only a hand-over of this exact value passes.
const PROFILE: TextMetricsV1 = TextMetricsV1 {
    advances: [700; 95],
    line_height: 1200,
    range_maxima: [1100, 1600, 2100],
};

fn fixture() -> ShellProcess {
    ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)))
}

fn custody() -> SessionCustody {
    SessionCustody::fixture(Arc::new(()))
}

fn scenes(frame: &ControlToShell) -> bool {
    matches!(frame, ControlToShell::Scenes { .. })
}

/// The served control leg opens with Welcome, then exactly one `ShellTuning` for Welcome's
/// session, then Settings.
fn assert_tuned_first(served: &[ControlToShell]) {
    let [
        ControlToShell::Welcome {
            incarnation,
            issue_epoch,
        },
        ControlToShell::ShellTuning { session, .. },
        ControlToShell::Settings { .. },
        ..,
    ] = served
    else {
        panic!("Welcome, ShellTuning and Settings come first: {served:?}");
    };
    assert_eq!(
        *session,
        IssueSession {
            incarnation: *incarnation,
            epoch: *issue_epoch,
        }
    );
    let tunings = served
        .iter()
        .filter(|frame| matches!(frame, ControlToShell::ShellTuning { .. }))
        .count();
    assert_eq!(tunings, 1);
}

/// Everything the Brain reported, once it has gone.
fn reported(seen: &mpsc::Receiver<async_mpsc::UnboundedReceiver<Seen>>) -> Vec<Seen> {
    let mut seen = seen.try_recv().expect("the Brain's observer");
    let mut reported = Vec::new();
    while let Ok(event) = seen.try_recv() {
        reported.push(event);
    }
    reported
}

#[tokio::test]
async fn text_metrics_are_handed_over_once_and_the_frame_buffered_after_them_is_served() {
    let scratch = Scratch::new("pair-handover");
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let (seen, observer) = mpsc::channel();
    let ((ran, stages), (served, after)) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                Observed { pair, seen },
                &shell,
                by(),
                item.clone(),
                custody(),
                Script::default(),
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                let submit = DataToBrain::TextSubmit {
                    turn: 1,
                    thread: None,
                    screen: ScreenChoice::None,
                    text: "go".into(),
                };
                for message in [
                    DataToBrain::Hello(hello()),
                    DataToBrain::TextMetrics(Some(PROFILE)),
                    submit,
                ] {
                    greeter.send_data(&message).await;
                }
                greeter.send_control(&ControlToBrain::Hello(hello())).await;
                let served = greeter
                    .control_until(|frame| {
                        matches!(frame, ControlToShell::RunState { run: RunId(1), .. })
                    })
                    .await;
                (served, greeter.quit().await)
            },
        )
    })
    .await;
    assert_eq!(ran, Ok(()), "served {served:?}, then {after:?}");
    assert_eq!(stages, STAGES);
    assert_tuned_first(&served);
    assert!(matches!(
        served.first(),
        Some(ControlToShell::Welcome { issue_epoch: 1, .. })
    ));
    let handed: Vec<_> = reported(&observer)
        .into_iter()
        .filter(|event| matches!(event, Seen::TextMetrics(_)))
        .collect();
    assert_eq!(handed, [Seen::TextMetrics(Some(PROFILE))]);
    assert!(item.get().is_none(), "settlement deleted the item");
}

#[tokio::test]
async fn holds_without_voice_or_screen_fail_not_loaded_and_a_quit_exits_cleanly() {
    use kumochi_protocol::message::{
        CaptureFailure, CaptureFor, GestureId, HoldFailure, HoldStatus,
    };

    let scratch = Scratch::new("pair-holds");
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let holds = holds();
    let (seen, observer) = mpsc::channel();
    let ((ran, stages), (started, answers, after)) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                Observed { pair, seen },
                &shell,
                by(),
                item.clone(),
                custody(),
                Script::default(),
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                let started = greeter.control_until(scenes).await;
                let mut answers = Vec::new();
                for hold in &holds {
                    greeter.send_control(hold).await;
                    let last = match hold {
                        ControlToBrain::HoldRecognized { .. } => capture_failed,
                        _ => voice_status,
                    };
                    answers.push(greeter.control_until(last).await);
                }
                (started, answers, greeter.quit().await)
            },
        )
    })
    .await;
    assert!(
        started.iter().any(|frame| matches!(
            frame,
            ControlToShell::Settings { view, .. } if view.hold_to_talk
        )),
        "hold-to-talk is on, so the holds reach the hold path"
    );
    let not_loaded = |gesture| ControlToShell::VoiceHoldStatus {
        gesture: GestureId(gesture),
        status: HoldStatus::Failed(HoldFailure::NotLoaded),
    };
    assert_eq!(
        answers,
        [
            vec![not_loaded(1)],
            vec![
                not_loaded(2),
                ControlToShell::CaptureFailed {
                    capture_for: CaptureFor::Gesture(GestureId(2)),
                    reason: CaptureFailure::Unavailable,
                },
            ],
            vec![not_loaded(3)],
        ]
    );
    assert_eq!(after, [], "nothing was pending at the Quit");
    let left: Vec<_> = reported(&observer)
        .into_iter()
        .filter(|event| matches!(event, Seen::HoldsLeft { .. }))
        .collect();
    let nothing = Seen::HoldsLeft {
        pending: false,
        debt: false,
        capturing: false,
    };
    assert_eq!(
        left,
        [nothing.clone(), nothing.clone(), nothing],
        "each hold left nothing behind"
    );
    assert_eq!(ran, Ok(()));
    assert_eq!(stages, STAGES);
    assert!(item.get().is_none(), "settlement deleted the item");
}

/// A voice hold, a capture hold and a screen hold, one gesture each.
fn holds() -> [ControlToBrain; 3] {
    use kumochi_protocol::geometry::PhysicalPoint;
    use kumochi_protocol::message::{GestureId, LeaseGeneration};

    [
        ControlToBrain::VoiceHoldRecognized {
            gesture: GestureId(1),
            thread: None,
            selection: 1,
        },
        ControlToBrain::HoldRecognized {
            gesture: GestureId(2),
            thread: None,
            selection: 1,
            pointer: PhysicalPoint([0, 0]),
            lease: LeaseGeneration(1),
            t_qpc: 1,
            notice: None,
        },
        ControlToBrain::ScreenHoldRecognized {
            gesture: GestureId(3),
            thread: None,
            selection: 1,
            t_qpc: 2,
            notice: None,
        },
    ]
}

fn voice_status(frame: &ControlToShell) -> bool {
    matches!(frame, ControlToShell::VoiceHoldStatus { .. })
}

fn capture_failed(frame: &ControlToShell) -> bool {
    matches!(frame, ControlToShell::CaptureFailed { .. })
}

#[tokio::test]
async fn a_tuning_whose_deadline_passed_is_never_written_even_to_a_writable_pair() {
    let scratch = Scratch::new("pair-late-tuning");
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let mut root = Root::new();
    let (admitted, mut greeter) = bounded(async {
        tokio::join!(
            Box::pin(root.admit(pair, &shell, by(), || Ok(custody()), || Ok(item.clone()))),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                greeter
            },
        )
    })
    .await;
    let (mut bound, _committed) = admitted.expect("admitted");
    let mut brain = crate::test_support::brain(&scratch, "");
    let terminal = Terminal::new(bound.pair.pair.subscribe(), None);
    let ended = serve(&mut bound, &mut brain, terminal, &shell, Instant::now()).await;
    assert_eq!(ended, Ended::Lost);
    drop(bound);
    assert!(
        bounded(greeter.closed()).await,
        "EOF, and no tuning before it"
    );
    brain.close().await;
    Box::pin(root.settle(None, ())).await;
}

/// Writes until the end's buffer is full; returns how many bytes that took.
fn fill(end: &StdStream) -> usize {
    let mut written = 0;
    for chunk in [&[0; 4096][..], &[0]] {
        loop {
            match (&*end).write(chunk) {
                Ok(n) => written += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("the fill failed: {error}"),
            }
        }
    }
    written
}

/// Reads what the end has buffered; true as well once it reached EOF.
fn drain(end: &StdStream) -> (usize, bool) {
    let mut buffer = [0; 4096];
    let mut read = 0;
    loop {
        match (&*end).read(&mut buffer) {
            Ok(0) => return (read, true),
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return (read, false),
            Err(error) => panic!("the drain failed: {error}"),
        }
    }
}

/// The writer is full at first and gets room only after `by` passed, while the timer, rounded up
/// to the next millisecond, hasn't fired: the tuning still never goes out.
#[tokio::test]
async fn a_writer_that_gets_room_after_the_deadline_writes_no_tuning() {
    let scratch = Scratch::new("pair-late-room");
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let brain_control = pair.control.try_clone().expect("the brain's control end");
    let shell_control = StdStream::from(
        ends[0]
            .as_fd()
            .try_clone_to_owned()
            .expect("the shell's control end"),
    );
    let mut root = Root::new();
    let (admitted, greeter) = bounded(async {
        tokio::join!(
            Box::pin(root.admit(pair, &shell, by(), || Ok(custody()), || Ok(item.clone()))),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                greeter
            },
        )
    })
    .await;
    let (mut bound, _committed) = admitted.expect("admitted");
    let mut brain = crate::test_support::brain(&scratch, "");
    let terminal = Terminal::new(bound.pair.pair.subscribe(), None);
    assert_eq!(drain(&shell_control), (0, false), "the greeting was read");
    let filled = fill(&brain_control);
    tokio::time::pause();
    let late = Instant::now() + Duration::from_micros(1);
    let ended = {
        let mut serving = std::pin::pin!(serve(&mut bound, &mut brain, terminal, &shell, late));
        let first = std::future::poll_fn(|cx| Poll::Ready(serving.as_mut().poll(cx))).await;
        assert_eq!(first, Poll::Pending, "the tuning waits for room");
        tokio::time::advance(Duration::from_micros(2)).await;
        assert_eq!(
            drain(&shell_control),
            (filled, false),
            "room after the deadline"
        );
        serving.await
    };
    assert_eq!(ended, Ended::Lost);
    drop((bound, brain_control));
    assert_eq!(
        drain(&shell_control),
        (0, true),
        "EOF, and no tuning before it"
    );
    drop(greeter);
    brain.close().await;
    Box::pin(root.settle(None, ())).await;
}

#[tokio::test]
async fn the_legs_stay_open_until_the_brain_settles_and_close_before_the_item() {
    let scratch = Scratch::new("pair-close-order");
    let item = Item::default();
    let (deleting, release) = item.hold_delete();
    let shell = fixture();
    let (pair, ends) = inherited();
    let (give, holding) = mpsc::channel();
    let ((ran, _), ()) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                pair,
                &shell,
                by(),
                item.clone(),
                custody(),
                Script {
                    hold: Some(give),
                    ..Script::default()
                },
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                let Holding { mut entered, free } = holding.try_recv().expect("the accounts' hold");
                greeter.hellos().await;
                greeter.metrics().await;
                greeter.control_until(scenes).await;
                greeter.send_control(&ControlToBrain::Quitting).await;
                entered
                    .recv()
                    .await
                    .expect("the accounts wait for their release");
                assert!(
                    greeter.open_for(SHORT).await,
                    "both legs stay open while the Brain settles"
                );
                free.send(()).expect("the release waits");
                let entered_delete =
                    tokio::task::spawn_blocking(move || deleting.recv_timeout(SHORT * 100))
                        .await
                        .expect("the wait ends");
                assert_eq!(entered_delete, Ok(()), "settlement deletes the item");
                assert_eq!(
                    tokio::time::timeout(SHORT * 10, greeter.read_to_end()).await,
                    Ok(()),
                    "both legs reached EOF before the item settled"
                );
                release.send(()).expect("the delete waits");
            },
        )
    })
    .await;
    assert_eq!(ran, Ok(()));
    assert!(item.get().is_none(), "settlement deleted the item");
}

#[tokio::test]
async fn a_parent_exit_while_serving_refuses_and_settles_with_both_peer_ends_open() {
    let scratch = Scratch::new("pair-parent-exit");
    let item = Item::default();
    let (shell, verdict) = ShellProcess::watched(1);
    let token = Arc::new(());
    let held = Arc::downgrade(&token);
    let (pair, ends) = inherited();
    let ((ran, stages), _greeter) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                pair,
                &shell,
                by(),
                item.clone(),
                SessionCustody::fixture(token),
                Script::default(),
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                greeter.control_until(scenes).await;
                assert!(
                    !shell.publish_watched(&verdict, Verdict::Exited),
                    "the claim came first"
                );
                greeter
            },
        )
    })
    .await;
    assert_eq!(ran, Err(ParentBoundError::Refused));
    assert_eq!(stages, STAGES);
    assert!(item.get().is_none(), "settlement deleted the item");
    assert!(held.upgrade().is_none(), "settlement let custody go");
}

#[tokio::test]
async fn a_fatal_refusal_closes_both_legs_while_the_brain_still_settles() {
    let scratch = Scratch::new("pair-fatal-close");
    let item = Item::default();
    let shell = fixture();
    let token = Arc::new(());
    let held = Arc::downgrade(&token);
    let (pair, ends) = inherited();
    let (give, holding) = mpsc::channel();
    let ((ran, stages), ()) = bounded(async {
        tokio::join!(
            run_on_fixtures(
                &scratch,
                pair,
                &shell,
                by(),
                item.clone(),
                SessionCustody::fixture(token),
                Script {
                    hold: Some(give),
                    ..Script::default()
                },
            ),
            async {
                let mut greeter = parent(ends, &item).await;
                let Holding { mut entered, free } = holding.try_recv().expect("the accounts' hold");
                greeter.hellos().await;
                greeter.metrics().await;
                greeter.control_until(scenes).await;
                greeter.forge_data();
                entered
                    .recv()
                    .await
                    .expect("the accounts wait for their release");
                assert_eq!(
                    tokio::time::timeout(SHORT * 10, greeter.read_to_end()).await,
                    Ok(()),
                    "both legs reach EOF while the account close is held"
                );
                free.send(()).expect("the release waits");
            },
        )
    })
    .await;
    assert_eq!(ran, Err(ParentBoundError::Refused));
    assert_eq!(stages, STAGES);
    assert!(item.get().is_none(), "settlement deleted the item");
    assert!(held.upgrade().is_none(), "settlement let custody go");
}
