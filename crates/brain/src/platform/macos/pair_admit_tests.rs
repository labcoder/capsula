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

/// The one item in a fake keychain, shared so a test still sees it after the attempt.
#[derive(Clone, Default)]
struct Item(Arc<Mutex<Option<[u8; CAPABILITY_LEN]>>>);

impl Item {
    fn get(&self) -> Option<[u8; CAPABILITY_LEN]> {
        *self.0.lock().expect("item")
    }
}

impl InstanceStore for Item {
    type Error = ();

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ()> {
        let mut item = self.0.lock().expect("item");
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
        *self.0.lock().expect("item") = Some(*key);
        Ok(())
    }

    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), ()> {
        *into = self.get().ok_or(())?;
        Ok(())
    }

    fn delete(&self) -> Result<(), ()> {
        self.0.lock().expect("item").take().map(drop).ok_or(())
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
fn inherited() -> (InheritedPair, [UnixStream; 2]) {
    let (control, shell_control) = sockets();
    let (data, shell_data) = sockets();
    let pair = adopt(fd(control), fd(data)).expect("adopted");
    let shell = [shell_control, shell_data].map(|end| {
        end.set_nonblocking(true).expect("non-blocking");
        UnixStream::from_std(end).expect("tokio")
    });
    (pair, shell)
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

/// The shell's greeting side of a bound pair.
struct Greeter {
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
    async fn hellos(&mut self) -> ControlToShell {
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

    async fn metrics(&mut self) {
        let metrics = frame::encode(&DataToBrain::TextMetrics(None), Pipe::Data).expect("encodes");
        self.data.write_frame(&metrics).await.expect("text metrics");
    }

    /// True once the brain closed the data leg with nothing written on it.
    async fn data_closed(&mut self) -> bool {
        matches!(self.data_in.read(&mut [0; 1]).await, Ok(0))
    }

    /// True once the brain closed the control leg with nothing more written.
    async fn closed(&mut self) -> bool {
        ipc::read::<ControlToShell>(&mut self.from_control, Pipe::Control)
            .await
            .is_ok_and(|frame| frame.is_none())
    }
}

/// The parent shell: READY, then the item READY vouches for, then the handshake on its secret.
async fn parent(mut ends: [UnixStream; 2], item: &Item) -> Greeter {
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
    assert_eq!(Box::pin(root.settle()).await.item, Stage::Confirmed);
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
    assert_eq!(Box::pin(root.settle()).await.item, Stage::Confirmed);
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
    assert_eq!(Box::pin(root.settle()).await.item, Stage::Confirmed);
}

/// Admits on a fresh pair with `shell` by `by`, where neither factory may run; returns the
/// result and whether READY reached the shell.
async fn refused_before_custody(shell: &ShellProcess, by: Instant) -> (bool, bool) {
    let (pair, [control, _data]) = inherited();
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
    (refused, ready_seen(&control))
}

#[tokio::test]
async fn an_already_lost_shell_is_refused_before_custody_with_no_ready() {
    let shell = fixture();
    assert!(shell.publish(1));
    assert_eq!(refused_before_custody(&shell, by()).await, (true, false));
}

#[tokio::test]
async fn a_passed_deadline_is_refused_before_custody_with_no_ready() {
    let passed = Instant::now();
    assert_eq!(
        refused_before_custody(&fixture(), passed).await,
        (true, false)
    );
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
