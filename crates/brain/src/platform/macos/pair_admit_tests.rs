//! Parent-bound admission on the inherited pair: READY's fences, the raced handshake and greeting,
//! and the root's custody, item and claim.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    CAPABILITY_LEN, ControlToBrain, ControlToShell, DataToBrain, IssueSession,
};
use kumochi_session::Session;
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::Instant;

use super::super::stream::{RecordReader, RecordWriter};
use super::legs::ON_DATA;
use super::legs::PairLegs;
use super::tests::{SECRET, bounded, brain, by, ends, fd, hello, shell, sockets};
use super::{InheritedPair, KeyedPair, PairError, RegisteredPair, admit, adopt, channel, respond};
use crate::ParentBoundError;
use crate::ipc::{self, Legs as _};
use crate::platform::ShellProcess;
use crate::platform::shell_process::Verdict;
use crate::runtime::Stage;
use crate::startup::custody::{CustodyError, SessionCustody};
use crate::startup::instance::{Added, InstanceStore};
use crate::startup::parent_bound::{BEFORE_CLAIM, Root};

/// Where a held delete says it entered, and what releases it.
type HeldDelete = Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>;

/// The one item in a fake keychain, shared so a test still sees it after the attempt; its next
/// delete can be held.
#[derive(Clone, Default)]
pub(super) struct Item {
    key: Arc<Mutex<Option<[u8; CAPABILITY_LEN]>>>,
    hold: Arc<Mutex<HeldDelete>>,
}

impl Item {
    pub(super) fn get(&self) -> Option<[u8; CAPABILITY_LEN]> {
        *self.key.lock().expect("item")
    }

    /// Holds the next delete until the returned sender releases it; the receiver hears it enter.
    pub(super) fn hold_delete(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        *self.hold.lock().expect("hold") = Some((entered, released));
        (started, release)
    }
}

impl InstanceStore for Item {
    type Error = ();

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ()> {
        let mut item = self.key.lock().expect("item");
        if item.is_some() {
            return Ok(Added::Duplicate);
        }
        *item = Some(*key);
        Ok(Added::Fresh)
    }

    fn inspect(&self) -> Result<(), ()> {
        self.get().map(drop).ok_or(())
    }

    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), ()> {
        *self.key.lock().expect("item") = Some(*key);
        Ok(())
    }

    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), ()> {
        *into = self.get().ok_or(())?;
        Ok(())
    }

    fn delete(&self) -> Result<(), ()> {
        if let Some((entered, released)) = self.hold.lock().expect("hold").take() {
            entered.send(()).expect("the test listens");
            released
                .recv_timeout(Duration::from_secs(10))
                .expect("the test releases");
        }
        self.key.lock().expect("item").take().map(drop).ok_or(())
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "admission takes a custody that can refuse"
)]
fn custody() -> Result<SessionCustody, CustodyError> {
    Ok(SessionCustody::fixture(Arc::new(())))
}

fn fixture() -> ShellProcess {
    ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)))
}

/// An adopted pair that tokio doesn't hold yet, and the shell's ends.
pub(super) fn inherited() -> (InheritedPair, [UnixStream; 2]) {
    let (control, shell_control) = sockets();
    let (data, shell_data) = sockets();
    let pair = adopt(fd(control), fd(data)).expect("adopted");
    let shell = [shell_control, shell_data].map(|end| {
        end.set_nonblocking(true).expect("non-blocking");
        UnixStream::from_std(end).expect("tokio")
    });
    (pair, shell)
}

/// Before READY the shell writes nothing: an ended leg, or a byte on either, shows as ended.
#[tokio::test]
async fn the_pair_shows_ended_for_a_closed_leg_or_an_early_byte() {
    use crate::startup::parent_bound::AdmissionPair as _;

    let (pair, [control, data]) = inherited();
    assert!(!pair.ended(), "both legs open and quiet");
    drop(data);
    assert!(pair.ended(), "the data leg ended");
    drop(control);

    let (pair, [control, _data]) = inherited();
    control.writable().await.expect("room for a byte");
    assert_eq!(control.try_write(b"x").expect("an early byte"), 1);
    assert!(pair.ended(), "a byte before READY");
}

/// Fills the brain's control buffer to the last byte, so READY has to wait for room. Sleeps first,
/// so the reactor takes registration's writable event and the fill's `WouldBlock` clears it.
async fn fill(pair: &RegisteredPair) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    while pair.control.try_write(&[0; 4096]).is_ok() {}
    while pair.control.try_write(&[0]).is_ok() {}
}

/// Reads whatever the brain has written so far; true when READY is in it.
fn ready_seen(end: &UnixStream) -> bool {
    let mut buffer = [0; 4096];
    let mut seen = false;
    loop {
        match end.try_read(&mut buffer) {
            Ok(0) => return seen,
            Ok(read) => seen |= buffer[..read].contains(&0x52),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return seen,
            Err(error) => panic!("the shell's read failed: {error}"),
        }
    }
}

fn never() -> IssueSession {
    panic!("no session is reserved")
}

/// The shell's side of a bound pair, greeting and then serving.
pub(super) struct Greeter {
    control: RecordWriter<OwnedWriteHalf>,
    data: RecordWriter<OwnedWriteHalf>,
    from_control: RecordReader<OwnedReadHalf>,
    data_in: OwnedReadHalf,
}

impl Greeter {
    fn new([control, data]: [UnixStream; 2], session: Session) -> Self {
        let (control_in, control_out) = control.into_split();
        let (data_in, data_out) = data.into_split();
        let (cell, _) = watch::channel(false);
        Self {
            control: RecordWriter::new(
                control_out,
                session.control.sealer,
                Pipe::Control,
                cell.clone(),
            ),
            data: RecordWriter::new(data_out, session.data.sealer, Pipe::Data, cell.clone()),
            from_control: RecordReader::new(control_in, session.control.opener, cell),
            data_in,
        }
    }

    /// Sends the control greeting and returns the reply, then sends the data greeting.
    pub(super) async fn hellos(&mut self) -> ControlToShell {
        let control =
            frame::encode(&ControlToBrain::Hello(hello()), Pipe::Control).expect("encodes");
        self.control
            .write_frame(&control)
            .await
            .expect("control hello");
        let reply = ipc::read(&mut self.from_control, Pipe::Control)
            .await
            .expect("a whole frame")
            .expect("a reply");
        let data = frame::encode(&DataToBrain::Hello(hello()), Pipe::Data).expect("encodes");
        self.data.write_frame(&data).await.expect("data hello");
        reply
    }

    pub(super) async fn metrics(&mut self) {
        self.send_data(&DataToBrain::TextMetrics(None)).await;
    }

    pub(super) async fn send_data(&mut self, message: &DataToBrain) {
        let bytes = frame::encode(message, Pipe::Data).expect("encodes");
        self.data.write_frame(&bytes).await.expect("a data frame");
    }

    pub(super) async fn send_control(&mut self, message: &ControlToBrain) {
        let bytes = frame::encode(message, Pipe::Control).expect("encodes");
        self.control
            .write_frame(&bytes)
            .await
            .expect("a control frame");
    }

    /// Reads control frames, answering pings, until `done` takes one; returns every other frame
    /// read on the way and that one last.
    pub(super) async fn control_until(
        &mut self,
        done: impl Fn(&ControlToShell) -> bool,
    ) -> Vec<ControlToShell> {
        let mut frames = Vec::new();
        loop {
            match ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control).await {
                Ok(Some(ControlToShell::Ping)) => self.send_control(&ControlToBrain::Pong).await,
                Ok(Some(frame)) => {
                    let last = done(&frame);
                    frames.push(frame);
                    if last {
                        return frames;
                    }
                }
                other => panic!(
                    "the control leg ended after {frames:?}: {:?}",
                    other.map(|_| ())
                ),
            }
        }
    }

    /// Quits, then reads the control leg to its end; returns what came after the Quit.
    pub(super) async fn quit(&mut self) -> Vec<ControlToShell> {
        self.send_control(&ControlToBrain::Quitting).await;
        let mut frames = Vec::new();
        while let Some(frame) = ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control)
            .await
            .expect("whole frames until the end")
        {
            frames.push(frame);
        }
        frames
    }

    /// True once the brain closed the data leg with nothing written on it.
    async fn data_closed(&mut self) -> bool {
        matches!(self.data_in.read(&mut [0; 1]).await, Ok(0))
    }

    /// Writes a record envelope no opener accepts on the data leg, so the brain refuses the pair.
    pub(super) fn forge_data(&self) {
        use std::io::Write as _;
        use std::os::fd::AsFd as _;

        let end = self
            .data_in
            .as_ref()
            .as_fd()
            .try_clone_to_owned()
            .expect("the shell's data end");
        std::os::unix::net::UnixStream::from(end)
            .write_all(&[0; 2])
            .expect("a forged envelope");
    }

    /// True when neither leg reaches EOF within `quiet`, reading whatever was already written.
    pub(super) async fn open_for(&mut self, quiet: Duration) -> bool {
        loop {
            match tokio::time::timeout(
                quiet,
                ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control),
            )
            .await
            {
                Err(_) => break,
                Ok(Ok(Some(_))) => {}
                Ok(_) => return false,
            }
        }
        let mut buffer = [0; 4096];
        loop {
            match tokio::time::timeout(quiet, self.data_in.read(&mut buffer)).await {
                Err(_) => return true,
                Ok(Ok(read)) if read > 0 => {}
                Ok(_) => return false,
            }
        }
    }

    /// Reads both legs to their ends.
    pub(super) async fn read_to_end(&mut self) {
        while ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control)
            .await
            .expect("whole frames until the end")
            .is_some()
        {}
        self.data_in
            .read_to_end(&mut Vec::new())
            .await
            .expect("the data leg reads to its end");
    }

    /// True once the brain closed the control leg with nothing more written.
    pub(super) async fn closed(&mut self) -> bool {
        ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control)
            .await
            .is_ok_and(|frame| frame.is_none())
    }
}

/// The parent shell: READY, then the item READY vouches for, then the handshake on its secret.
pub(super) async fn parent(mut ends: [UnixStream; 2], item: &Item) -> Greeter {
    let mut ready = [0; 1];
    ends[0].read_exact(&mut ready).await.expect("READY");
    assert_eq!(ready, [0x52]);
    let secret = item.get().expect("the item is published before READY");
    let [control, data] = &mut ends;
    let session = shell(control, data, &secret).await;
    Greeter::new(ends, session)
}

#[tokio::test]
async fn a_loss_while_ready_waits_for_room_ends_it_and_writes_nothing() {
    let (pair, [control, _data]) = ends();
    fill(&pair).await;
    let (shell, verdict) = ShellProcess::watched(1);
    let by = Instant::now() + Duration::from_secs(3);
    let (ready, ()) = bounded(async {
        tokio::join!(pair.ready(&shell, by), async {
            tokio::task::yield_now().await;
            assert!(shell.publish_watched(&verdict, Verdict::Exited));
        })
    })
    .await;
    assert_eq!(ready, Err(PairError::Lost));
    assert!(!ready_seen(&control));
}

#[tokio::test]
async fn room_that_comes_after_a_loss_still_writes_no_ready() {
    let (pair, [control, _data]) = ends();
    fill(&pair).await;
    let shell = fixture();
    let mut seen = false;
    let (ready, ()) = bounded(async {
        tokio::join!(pair.ready(&shell, by()), async {
            tokio::task::yield_now().await;
            assert!(shell.publish(1));
            seen |= ready_seen(&control);
        })
    })
    .await;
    assert_eq!(ready, Err(PairError::Lost));
    assert!(!seen && !ready_seen(&control));
}

#[tokio::test]
async fn a_loss_mid_handshake_refuses_at_once() {
    let (pair, [mut control, _data]) = ends();
    let (shell, verdict) = ShellProcess::watched(1);
    let keyed = KeyedPair {
        pair,
        brain: brain(),
    };
    let started = Instant::now();
    let (admitted, ()) = bounded(async {
        tokio::join!(Box::pin(admit(keyed, &shell, by(), never)), async {
            let mut ready = [0; 1];
            control.read_exact(&mut ready).await.expect("READY");
            assert!(shell.publish_watched(&verdict, Verdict::Exited));
        })
    })
    .await;
    assert!(matches!(admitted, Err(PairError::Lost)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "before the phase cap"
    );
}

#[tokio::test]
async fn a_loss_during_the_text_metrics_read_refuses_and_closes_the_pair() {
    let (pair, [mut control, mut data]) = ends();
    let (watched, verdict) = ShellProcess::watched(1);
    let keyed = KeyedPair {
        pair,
        brain: brain(),
    };
    let mut epochs = crate::runtime::IssueEpochs::default();
    let reserve = || epochs.reserve(kumochi_protocol::message::Incarnation(3));
    let losing = watched.clone();
    ON_DATA.set(Some(Box::new(move || {
        assert!(losing.publish_watched(&verdict, Verdict::Exited));
    })));
    let (admitted, mut greeter) = bounded(async {
        tokio::join!(Box::pin(admit(keyed, &watched, by(), reserve)), async {
            let mut ready = [0; 1];
            control.read_exact(&mut ready).await.expect("READY");
            let session = shell(&mut control, &mut data, &SECRET).await;
            let mut greeter = Greeter::new([control, data], session);
            assert!(matches!(
                greeter.hellos().await,
                ControlToShell::Welcome { .. }
            ));
            greeter
        })
    })
    .await;
    assert!(ON_DATA.take().is_none(), "the text-metrics read started");
    assert!(matches!(admitted, Err(PairError::Lost)));
    assert!(bounded(greeter.closed()).await, "the control leg is closed");
    assert!(
        bounded(greeter.data_closed()).await,
        "the data leg is closed"
    );
}

#[tokio::test]
async fn pair_legs_write_a_control_frame_only_after_the_control_greeting() {
    let (pair, [mut control, mut data]) = ends();
    let (pair, _session) = bounded(async {
        tokio::join!(
            Box::pin(respond(pair, brain(), by())),
            shell(&mut control, &mut data, &SECRET),
        )
    })
    .await;
    let mut pair = pair.expect("bound");
    let shell = fixture();
    let welcome = ControlToShell::Welcome {
        incarnation: kumochi_protocol::message::Incarnation(3),
        issue_epoch: 1,
    };
    let frame = frame::encode(&welcome, Pipe::Control).expect("encodes");
    let written = PairLegs::new(&mut pair, &shell, by())
        .write_control(&frame)
        .await;
    assert_eq!(
        written.map_err(|error| error.kind()),
        Err(io::ErrorKind::InvalidData)
    );
    assert!(*pair.pair.borrow(), "the early write refused the pair");
}

#[tokio::test]
async fn admission_publishes_before_ready_and_claims_after_the_greeting() {
    let item = Item::default();
    let shell = fixture();
    let (pair, ends) = inherited();
    let mut root = Root::new();
    let (admitted, welcome) = bounded(async {
        tokio::join!(
            Box::pin(root.admit(pair, &shell, by(), custody, || Ok(item.clone()))),
            async {
                let mut greeter = parent(ends, &item).await;
                let welcome = greeter.hellos().await;
                greeter.metrics().await;
                (welcome, greeter)
            },
        )
    })
    .await;
    let (greeted, _committed) = admitted.expect("admitted");
    assert!(matches!(
        welcome.0,
        ControlToShell::Welcome { issue_epoch: 1, .. }
    ));
    assert!(
        shell.claim_commit().is_none(),
        "admission took the one claim"
    );
    drop(greeted);
    assert!(item.get().is_some());
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
    assert!(item.get().is_none());
}

#[tokio::test]
async fn a_loss_between_the_last_fence_and_the_claim_refuses() {
    let item = Item::default();
    let shell = fixture();
    let losing = shell.clone();
    BEFORE_CLAIM.set(Some(Box::new(move || assert!(losing.publish(1)))));
    let (pair, ends) = inherited();
    let mut root = Root::new();
    let (admitted, _greeter) = bounded(async {
        tokio::join!(
            Box::pin(root.admit(pair, &shell, by(), custody, || Ok(item.clone()))),
            async {
                let mut greeter = parent(ends, &item).await;
                greeter.hellos().await;
                greeter.metrics().await;
                greeter
            },
        )
    })
    .await;
    assert!(matches!(admitted, Err(ParentBoundError::Refused)));
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
    assert!(item.get().is_none());
}

#[tokio::test]
async fn busy_custody_refuses_before_any_item_call_or_ready() {
    let called = AtomicBool::new(false);
    let (pair, [control, _data]) = inherited();
    let mut root = Root::<Item>::new();
    let admitted = Box::pin(root.admit(
        pair,
        &fixture(),
        by(),
        || Err(CustodyError::Busy),
        || {
            called.store(true, Ordering::SeqCst);
            Ok(Item::default())
        },
    ))
    .await;
    assert!(matches!(admitted, Err(ParentBoundError::Busy)));
    assert!(!called.load(Ordering::SeqCst), "no item was made");
    assert!(!ready_seen(&control));
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
}

/// Admits on a fresh pair with `shell` by `by`, where neither factory may run; returns whether
/// it refused and whether the shell's first read of the control leg was EOF, so no READY was sent
/// and the pair was closed.
async fn refused_before_custody(shell: &ShellProcess, by: Instant) -> (bool, bool) {
    let (pair, [mut control, _data]) = inherited();
    let mut root = Root::<Item>::new();
    let admitted = Box::pin(root.admit(
        pair,
        shell,
        by,
        || panic!("no custody is taken"),
        || panic!("no item is made"),
    ))
    .await;
    let refused = matches!(admitted, Err(ParentBoundError::Refused));
    let first = bounded(control.read(&mut [0; 1])).await;
    (refused, matches!(first, Ok(0)))
}

#[tokio::test]
async fn an_already_lost_shell_is_refused_before_custody_with_no_ready() {
    let shell = fixture();
    assert!(shell.publish(1));
    assert_eq!(refused_before_custody(&shell, by()).await, (true, true));
}

#[tokio::test]
async fn a_passed_deadline_is_refused_before_custody_with_no_ready() {
    let passed = Instant::now();
    assert_eq!(
        refused_before_custody(&fixture(), passed).await,
        (true, true)
    );
}

#[tokio::test]
async fn custody_outlives_an_admission_dropped_after_ready_until_settlement() {
    let item = Item::default();
    let shell = fixture();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let (pair, [mut control, _data]) = inherited();
    let mut root = Root::new();
    let mut ready = [0; 1];
    {
        let mut admitting = Box::pin(root.admit(
            pair,
            &shell,
            by(),
            || Ok(SessionCustody::fixture(token)),
            || Ok(item.clone()),
        ));
        bounded(async {
            tokio::select! {
                _ = &mut admitting => panic!("admission waits for the handshake"),
                read = control.read_exact(&mut ready) => read.expect("READY"),
            }
        })
        .await;
    }
    assert_eq!(ready, [0x52]);
    assert!(
        custody.upgrade().is_some(),
        "custody is held after the dropped admission"
    );
    assert!(
        item.get().is_some(),
        "the item READY vouched for is still there"
    );
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
    assert!(item.get().is_none(), "settlement retired the item");
    assert!(custody.upgrade().is_none(), "settlement let custody go");
}

#[test]
fn only_an_installed_or_a_development_edition_has_a_session_channel() {
    use kumochi_protocol::constants::ProductIdentity;
    use kumochi_session::Channel;

    assert!(matches!(
        channel(ProductIdentity::RELEASE),
        Ok(Channel::Release)
    ));
    assert!(matches!(channel(ProductIdentity::DEV), Ok(Channel::Dev)));
    assert!(matches!(
        channel(ProductIdentity::GATE),
        Err(PairError::Edition)
    ));
}
