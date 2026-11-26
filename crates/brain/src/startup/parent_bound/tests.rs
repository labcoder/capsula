use std::io;
use std::sync::{Arc, Weak, mpsc};
use std::time::Duration;

use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::CAPABILITY_LEN;
use tokio::sync::mpsc as async_mpsc;
use tokio::time::timeout;

use super::*;
use crate::connections::error::VaultError;
use crate::engines::fixture::Fixture;
use crate::platform::SessionLockError;
use crate::platform::credential::CredentialStoreError;
use crate::runtime::{REFUSE_SPAWN, Selected};
use crate::startup::custody::SessionCustody;
use crate::startup::instance::Added;
use crate::test_support::{Scratch, failing_accounts, held_accounts_until};

const PATIENCE: Duration = Duration::from_secs(60);

/// An idle attempt settles without an item call, so none is made.
struct NoItem;

impl InstanceStore for NoItem {
    type Error = std::convert::Infallible;
    fn add(&self, _: &[u8; CAPABILITY_LEN]) -> Result<Added, Self::Error> {
        unreachable!("an idle attempt adds no item")
    }
    fn inspect(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt inspects no item")
    }
    fn update(&self, _: &[u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt updates no item")
    }
    fn read(&self, _: &mut [u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt reads no item")
    }
    fn delete(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt deletes no item")
    }
}

/// Signals when the credential thread drops it, after the vault and before the release, then
/// holds that thread until the test frees it, for at most `PATIENCE`.
struct Hold {
    entered: async_mpsc::UnboundedSender<()>,
    free: mpsc::Receiver<()>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.entered.send(()).ok();
        self.free.recv_timeout(PATIENCE).ok();
    }
}

/// The test's ends of a `Hold`.
struct Holding {
    entered: async_mpsc::UnboundedReceiver<()>,
    free: mpsc::Sender<()>,
}

/// A `Hold`'s parts, unarmed, so a fixture dropped before its accounts start never blocks.
fn hold() -> (
    (async_mpsc::UnboundedSender<()>, mpsc::Receiver<()>),
    Holding,
) {
    let (entered_tx, entered) = async_mpsc::unbounded_channel();
    let (free, free_rx) = mpsc::channel();
    ((entered_tx, free_rx), Holding { entered, free })
}

/// How the fixture's accounts start.
enum Start {
    /// Held accounts on the profile lock.
    Held,
    /// A startup load that finds native credentials unsupported.
    Unsupported,
    /// A startup load that panics.
    Panics,
}

/// Where a scripted start publishes the parent's loss.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoseAt {
    /// Once the prerequisites are ready, before anything starts.
    Ready,
    /// Once the store is open.
    Open,
}

/// A scratch store, accounts whose release waits on the hold, and a subscriber scoped to this
/// thread; it records each stage it ran and can publish a loss at one of them.
struct Scripted<'a> {
    scratch: &'a Scratch,
    start: Start,
    hold: Option<(async_mpsc::UnboundedSender<()>, mpsc::Receiver<()>)>,
    calls: Vec<&'static str>,
    lose: Option<(LoseAt, ShellProcess)>,
}

impl<'a> Scripted<'a> {
    fn new(scratch: &'a Scratch, start: Start) -> (Self, Holding) {
        let (parts, holding) = hold();
        let pre = Self {
            scratch,
            start,
            hold: Some(parts),
            calls: Vec::new(),
            lose: None,
        };
        (pre, holding)
    }
}

impl Prerequisites for Scripted<'_> {
    fn ready(&self) -> Result<(), NotReady> {
        self.lose_at(LoseAt::Ready);
        Ok(())
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        self.calls.push("init");
        let logs = crate::logging::file_subscriber(&self.scratch.dir().join("logs"), "brain")
            .map_err(|_| BrainError::Process(io::ErrorKind::Other.into()))?;
        Ok(ProcessInit {
            _scoped: Some(tracing::subscriber::set_default(logs)),
        })
    }

    fn open(&mut self, acquired: &mut bool) -> Result<OpenedState, BrainError> {
        self.calls.push("open");
        let opened = OpenedState::load_marking(
            &self.scratch.db(),
            || Some(self.scratch.dir().to_owned()),
            Some(Selected::Fixture(Box::new(
                Fixture::parse("").expect("empty fixture"),
            ))),
            acquired,
        );
        self.lose_at(LoseAt::Open);
        opened
    }

    fn accounts(
        &mut self,
        _: OAuthTuning,
        _: TokenTuning,
        profile: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        self.calls.push("accounts");
        let (entered, free) = self.hold.take().expect("one start");
        let hold = Hold { entered, free };
        let accounts = match self.start {
            Start::Held => held_accounts_until(profile, hold),
            Start::Unsupported => failing_accounts(hold, || {
                Err(VaultError::Store(CredentialStoreError::Unsupported))
            })?,
            Start::Panics => failing_accounts(hold, panicking_open)?,
        };
        Ok((accounts, Arc::default()))
    }

    fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError> {
        self.calls.push("containment");
        Ok(None)
    }
}

impl Scripted<'_> {
    /// Publishes the loss if this is where the script puts it; the claim always came first.
    fn lose_at(&self, at: LoseAt) {
        if let Some((lose, shell)) = &self.lose
            && *lose == at
        {
            assert!(!shell.publish(1), "the claim came before the loss");
        }
    }
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "the vault's open panics on purpose"
)]
fn panicking_open()
-> Result<crate::connections::vault::Vault<crate::connections::vault::fake::FakeStore>, VaultError>
{
    panic!("the vault's open panics")
}

/// A root whose attempt holds a token custody, and a watch on that custody.
fn root() -> (Root<NoItem>, Weak<()>) {
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let mut root = Root::new();
    root.instance = Some(InstanceAttempt::new(
        SessionCustody::fixture(token),
        Arc::new(NoItem),
    ));
    (root, custody)
}

/// Fails the start at `stop`, if any.
fn fail_at(
    stop: Option<Checkpoint>,
) -> impl FnMut(Checkpoint, &mut Stages) -> Result<(), BrainError> {
    move |point, _| {
        if Some(point) == stop {
            Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
        } else {
            Ok(())
        }
    }
}

const SETTLED: Settled = Settled {
    runtime: Shutdown {
        quit: None,
        store: Stage::Unconfirmed,
        settings: Stage::Confirmed,
        claude: Stage::Confirmed,
        account: Stage::Confirmed,
    },
    item: Stage::Confirmed,
};

/// Fails a root's start after its accounts started, at `stop` or else in `Brain::new`, and
/// checks that settling waits for the credential thread's release before custody drops.
async fn assert_settles_after_the_release(name: &str, stop: Option<Checkpoint>) {
    let scratch = Scratch::new(name);
    let lock = scratch.dir().join(SESSION_LOCK);
    let (mut root, custody) = root();
    let (mut pre, mut holding) = Scripted::new(&scratch, Start::Held);
    REFUSE_SPAWN.set(stop.is_none());
    let Err(error) = root.start(&mut pre, fail_at(stop)) else {
        panic!("the injected failure must stop the start");
    };
    match stop {
        Some(_) => assert!(
            matches!(&error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted),
            "{error:?}"
        ),
        None => assert!(
            matches!(&error, BrainError::StoreThread(error) if error.kind() == io::ErrorKind::Other),
            "{error:?}"
        ),
    }
    assert!(root.stages.brain.is_none(), "no Brain was built");
    let settle = root.settle(None);
    tokio::pin!(settle);
    timeout(PATIENCE, async {
        tokio::select! {
            biased;
            settled = &mut settle => panic!("settled before the credential thread let go: {settled:?}"),
            entered = holding.entered.recv() => entered.expect("the credential thread ends"),
        }
    })
    .await
    .expect("the credential thread reaches its release");
    assert!(
        timeout(Duration::ZERO, &mut settle).await.is_err(),
        "settling waits for the held credential thread"
    );
    assert!(
        matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
        "a contender is refused while the release is held"
    );
    assert!(custody.upgrade().is_some(), "custody outlives the release");
    holding
        .free
        .send(())
        .expect("the credential thread waits for its release");
    let settled = timeout(PATIENCE, settle)
        .await
        .expect("settling ends once the release is free");
    assert_eq!(
        settled, SETTLED,
        "the accounts close in time and the raw store stays unconfirmed"
    );
    assert!(custody.upgrade().is_none(), "custody drops at settlement");
    drop(SessionLock::try_acquire(&lock).expect("the profile is free once settled"));
}

#[tokio::test]
async fn a_failed_accounts_checkpoint_closes_the_accounts_before_custody_drops() {
    Box::pin(assert_settles_after_the_release(
        "root-accounts-checkpoint",
        Some(Checkpoint::Accounts),
    ))
    .await;
}

#[tokio::test]
async fn a_failed_brain_constructor_returns_the_accounts_to_close_before_custody_drops() {
    Box::pin(assert_settles_after_the_release("root-brain-new", None)).await;
}

/// The worker starts and its load fails as `start` makes it; the start returns only once that
/// worker has let go, so nothing it held is left for settlement to miss.
async fn assert_start_waits_for_the_worker(name: &str, start: Start, kind: io::ErrorKind) {
    let scratch = Scratch::new(name);
    let lock = scratch.dir().join(SESSION_LOCK);
    let (mut root, custody) = root();
    let (mut pre, holding) = Scripted::new(&scratch, start);
    let (returned_tx, returned) = mpsc::channel();
    let watcher = std::thread::spawn({
        let Holding { mut entered, free } = holding;
        let lock = lock.clone();
        move || {
            entered.blocking_recv().expect("the credential thread ends");
            assert!(
                matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
                "a contender is refused while the worker is held"
            );
            assert!(custody.upgrade().is_some(), "custody outlives the worker");
            assert_eq!(
                returned.recv_timeout(Duration::from_millis(500)),
                Err(mpsc::RecvTimeoutError::Timeout),
                "the start waits for the held worker"
            );
            free.send(()).expect("the worker waits for its release");
            custody
        }
    });
    let started = root.start(&mut pre, |_, _| Ok(()));
    returned_tx.send(()).ok();
    let Err(error) = started else {
        panic!("a failed load must stop the start");
    };
    assert!(
        matches!(&error, BrainError::Process(error) if error.kind() == kind),
        "{error:?}"
    );
    let custody = watcher.join().expect("the watcher's checks hold");
    let settled = timeout(PATIENCE, Box::pin(root.settle(None)))
        .await
        .expect("settling has nothing to wait for");
    assert_eq!(
        settled, SETTLED,
        "no accounts are left to close, and the opened store stays unconfirmed"
    );
    assert!(custody.upgrade().is_none(), "custody drops at settlement");
    drop(SessionLock::try_acquire(&lock).expect("the profile is free once settled"));
}

#[tokio::test]
async fn a_refused_startup_load_returns_only_after_its_worker_let_go() {
    Box::pin(assert_start_waits_for_the_worker(
        "root-unsupported-load",
        Start::Unsupported,
        io::ErrorKind::Unsupported,
    ))
    .await;
}

/// The worker's own unwind drops the vault before the start hears it stopped.
#[tokio::test]
async fn a_panicked_startup_load_returns_only_after_its_worker_let_go() {
    Box::pin(assert_start_waits_for_the_worker(
        "root-panicked-load",
        Start::Panics,
        io::ErrorKind::Other,
    ))
    .await;
}

/// An open refused before it holds a store leaves nothing to confirm.
#[tokio::test]
async fn an_open_refused_before_the_store_settles_it_confirmed() {
    let scratch = Scratch::new("root-open-busy");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let held = SessionLock::try_acquire(&scratch.dir().join(SESSION_LOCK)).expect("the lock");
    let (mut root, _) = root();
    let (mut pre, _holding) = Scripted::new(&scratch, Start::Held);
    let Err(error) = root.start(&mut pre, fail_at(None)) else {
        panic!("a busy profile must stop the start");
    };
    assert!(matches!(error, BrainError::ProfileBusy), "{error:?}");
    let settled = Box::pin(root.settle(None)).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Confirmed,
        "no store was opened: {settled:?}"
    );
    drop(held);
}

/// An open that fails after SQLite opened has dropped a raw store, which nothing confirms.
#[tokio::test]
async fn an_open_failed_after_the_store_settles_it_unconfirmed() {
    let scratch = Scratch::new("root-open-recovery");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    drop(crate::store::Store::open(&scratch.db()).expect("a first open"));
    rusqlite::Connection::open(scratch.db())
        .expect("raw open")
        .execute_batch("DROP TABLE threads;")
        .expect("recovery's table goes");
    let (mut root, _) = root();
    let (mut pre, _holding) = Scripted::new(&scratch, Start::Held);
    let Err(error) = root.start(&mut pre, fail_at(None)) else {
        panic!("a failed recovery must stop the start");
    };
    assert!(matches!(error, BrainError::Store(_)), "{error:?}");
    let settled = Box::pin(root.settle(None)).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Unconfirmed,
        "the store opened before recovery failed: {settled:?}"
    );
}

/// A store SQLite opened but whose schema is newer than this build's is still a raw store.
#[tokio::test]
async fn an_open_refused_during_the_stores_setup_settles_it_unconfirmed() {
    let scratch = Scratch::new("root-open-schema");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    drop(crate::store::Store::open(&scratch.db()).expect("a first open"));
    rusqlite::Connection::open(scratch.db())
        .expect("raw open")
        .execute_batch("PRAGMA user_version = 999;")
        .expect("a newer build's schema");
    let (mut root, _) = root();
    let (mut pre, _holding) = Scripted::new(&scratch, Start::Held);
    let Err(error) = root.start(&mut pre, fail_at(None)) else {
        panic!("a newer schema must stop the start");
    };
    assert!(matches!(error, BrainError::Store(_)), "{error:?}");
    let settled = Box::pin(root.settle(None)).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Unconfirmed,
        "SQLite opened before the setup failed: {settled:?}"
    );
}

#[test]
fn this_builds_prerequisites_are_never_ready() {
    assert_eq!(Pending.ready(), Err(NotReady));
}

/// A pair that records how far it was driven: 1 once keyed, 2 once greeted, 3 once served, when
/// serving ends as `ends`.
#[derive(Clone)]
struct FakePair {
    reached: Arc<std::sync::atomic::AtomicU8>,
    ends: Ended,
}

impl Default for FakePair {
    fn default() -> Self {
        Self::ending(Ended::Lost)
    }
}

impl FakePair {
    fn ending(ends: Ended) -> Self {
        Self {
            reached: Arc::default(),
            ends,
        }
    }

    fn reached(&self) -> u8 {
        self.reached.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn reach(&self, step: u8) {
        self.reached
            .store(step, std::sync::atomic::Ordering::SeqCst);
    }
}

impl AdmissionPair for FakePair {
    type Keyed = Self;
    type Greeted = Self;

    fn key(self, _: &[u8; CAPABILITY_LEN]) -> Option<Self> {
        self.reach(1);
        Some(self)
    }

    fn admit(
        keyed: Self,
        _: &ShellProcess,
        _: Instant,
        reserve: impl FnOnce() -> IssueSession,
    ) -> impl Future<Output = Option<Self>> {
        reserve();
        keyed.reach(2);
        std::future::ready(Some(keyed))
    }

    fn cell(_: &Self) -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    fn serve(
        greeted: Self,
        _: &mut Brain,
        _: Terminal,
        _: &ShellProcess,
        _: Instant,
    ) -> impl Future<Output = Ended> {
        greeted.reach(3);
        std::future::ready(greeted.ends)
    }
}

/// Where a held add says it entered, and what releases it.
type HeldAdd = Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>;

/// A fake item whose next add can be held: the test hears it enter and releases it.
#[derive(Clone, Default)]
struct Slot {
    item: Arc<std::sync::Mutex<Option<[u8; CAPABILITY_LEN]>>>,
    hold: Arc<std::sync::Mutex<HeldAdd>>,
}

impl Slot {
    fn present(&self) -> bool {
        self.item.lock().expect("item").is_some()
    }

    fn hold_add(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        *self.hold.lock().expect("hold") = Some((entered, released));
        (started, release)
    }
}

impl InstanceStore for Slot {
    type Error = ();

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ()> {
        if let Some((entered, released)) = self.hold.lock().expect("hold").take() {
            entered.send(()).expect("the test listens");
            released.recv_timeout(PATIENCE).expect("the test releases");
        }
        *self.item.lock().expect("item") = Some(*key);
        Ok(Added::Fresh)
    }

    fn inspect(&self) -> Result<(), ()> {
        self.present().then_some(()).ok_or(())
    }

    fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), ()> {
        *self.item.lock().expect("item") = Some(*key);
        Ok(())
    }

    fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), ()> {
        *into = self.item.lock().expect("item").ok_or(())?;
        Ok(())
    }

    fn delete(&self) -> Result<(), ()> {
        self.item.lock().expect("item").take().map(drop).ok_or(())
    }
}

fn live() -> ShellProcess {
    ShellProcess::fixture(1, Arc::new(std::sync::atomic::AtomicU8::new(0)))
}

fn later() -> Instant {
    Instant::now() + PATIENCE
}

#[tokio::test]
async fn admission_fences_again_after_custody_before_the_item() {
    let shell = live();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let pair = FakePair::default();
    let mut root = Root::<Slot>::new();
    let admitted = Box::pin(root.admit(
        pair.clone(),
        &shell,
        later(),
        || {
            assert!(shell.publish(1), "the loss lands while custody is taken");
            Ok(SessionCustody::fixture(token))
        },
        || panic!("no item is made after a loss"),
    ))
    .await;
    assert!(matches!(admitted, Err(ParentBoundError::Refused)));
    assert_eq!(pair.reached(), 0, "no READY");
    assert!(custody.upgrade().is_none(), "the unused custody is let go");
    assert_eq!(Box::pin(root.settle(None)).await.item, Stage::Confirmed);
}

#[tokio::test]
async fn a_second_admission_after_a_refusal_refuses_without_running_anything() {
    let lost = live();
    assert!(lost.publish(1));
    let mut root = Root::<Slot>::new();
    let first = Box::pin(root.admit(
        FakePair::default(),
        &lost,
        later(),
        || panic!("no custody after a loss"),
        || panic!("no item after a loss"),
    ))
    .await;
    assert!(matches!(first, Err(ParentBoundError::Refused)));
    let pair = FakePair::default();
    let second = Box::pin(root.admit(
        pair.clone(),
        &live(),
        later(),
        || panic!("a second admission takes no custody"),
        || panic!("a second admission makes no item"),
    ))
    .await;
    assert!(matches!(second, Err(ParentBoundError::Refused)));
    assert_eq!(pair.reached(), 0);
    assert_eq!(Box::pin(root.settle(None)).await.item, Stage::Confirmed);
}

/// Starts an admission whose add the slot holds, waits for that add to enter, then drops it.
async fn dropped_mid_publish(
    root: &mut Root<Slot>,
    slot: &Slot,
    token: Arc<()>,
) -> mpsc::Sender<()> {
    let (entered, release) = slot.hold_add();
    let shell = live();
    let mut admitting = Box::pin(root.admit(
        FakePair::default(),
        &shell,
        later(),
        || Ok(SessionCustody::fixture(token)),
        || Ok(slot.clone()),
    ));
    let waiting = tokio::task::spawn_blocking(move || entered.recv_timeout(PATIENCE));
    timeout(PATIENCE, async {
        tokio::select! {
            _ = &mut admitting => panic!("the add is held"),
            heard = waiting => heard.expect("the listener").expect("the add entered"),
        }
    })
    .await
    .expect("the add enters in time");
    release
}

#[tokio::test]
async fn custody_outlives_an_admission_dropped_during_publish_until_settlement() {
    let slot = Slot::default();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let mut root = Root::new();
    let release = dropped_mid_publish(&mut root, &slot, token).await;
    assert!(
        custody.upgrade().is_some(),
        "custody is held while the add is out"
    );
    release.send(()).expect("the add waits");
    assert_eq!(Box::pin(root.settle(None)).await.item, Stage::Confirmed);
    assert!(!slot.present(), "settlement deleted what the add wrote");
    assert!(custody.upgrade().is_none(), "settlement let custody go");
}

#[tokio::test]
async fn a_second_admission_after_a_cancelled_one_leaves_its_attempt_alone() {
    let slot = Slot::default();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let mut root = Root::new();
    let release = dropped_mid_publish(&mut root, &slot, token).await;
    let pair = FakePair::default();
    let second = Box::pin(root.admit(
        pair.clone(),
        &live(),
        later(),
        || panic!("a second admission takes no custody"),
        || panic!("a second admission makes no item"),
    ))
    .await;
    assert!(matches!(second, Err(ParentBoundError::Refused)));
    assert_eq!(pair.reached(), 0);
    assert!(
        custody.upgrade().is_some(),
        "the first attempt still holds custody"
    );
    release.send(()).expect("the add waits");
    assert_eq!(Box::pin(root.settle(None)).await.item, Stage::Confirmed);
    assert!(!slot.present());
    assert!(custody.upgrade().is_none());
}

/// What a full run on the fixtures left: its result, the stages it ran, the item and custody.
struct Ran {
    result: Result<(), ParentBoundError>,
    calls: Vec<&'static str>,
    pair: FakePair,
    item: Slot,
    custody: Weak<()>,
}

/// Admits a fake pair on a live shell, runs the scripted stages, serves until `ends`, then settles.
async fn run_to(name: &str, ends: Ended, lose: Option<LoseAt>) -> Ran {
    let scratch = Scratch::new(name);
    let shell = live();
    let item = Slot::default();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let pair = FakePair::ending(ends);
    let (result, calls) = timeout(
        PATIENCE,
        run_on_fixtures(
            &scratch,
            pair.clone(),
            &shell,
            later(),
            item.clone(),
            SessionCustody::fixture(token),
            lose,
        ),
    )
    .await
    .expect("the run settles");
    Ran {
        result,
        calls,
        pair,
        item,
        custody,
    }
}

const STAGES: [&str; 4] = ["init", "open", "accounts", "containment"];

#[tokio::test]
async fn a_served_quit_settles_every_stage_and_exits_cleanly() {
    let ran = run_to("pb-quit", Ended::Quit { deadline: later() }, None).await;
    assert_eq!(ran.result, Ok(()));
    assert_eq!(ran.calls, STAGES);
    assert_eq!(ran.pair.reached(), 3, "served");
    assert!(!ran.item.present(), "settlement deleted the item");
    assert!(ran.custody.upgrade().is_none(), "settlement let custody go");
}

#[tokio::test]
async fn a_lost_shell_is_refused_even_after_a_clean_close() {
    let ran = run_to("pb-lost", Ended::Lost, None).await;
    assert_eq!(ran.result, Err(ParentBoundError::Refused));
    assert_eq!(ran.calls, STAGES);
    assert_eq!(ran.pair.reached(), 3, "served");
    assert!(!ran.item.present());
    assert!(ran.custody.upgrade().is_none());
}

#[tokio::test]
async fn a_loss_once_ready_stops_before_any_stage_starts() {
    let ran = run_to("pb-lose-ready", Ended::Lost, Some(LoseAt::Ready)).await;
    assert_eq!(ran.result, Err(ParentBoundError::Refused));
    assert!(ran.calls.is_empty(), "{:?}", ran.calls);
    assert_eq!(ran.pair.reached(), 2, "admitted, never served");
    assert!(!ran.item.present());
    assert!(ran.custody.upgrade().is_none());
}

/// The open store is dropped unconfirmed, which outranks the refusal.
#[tokio::test]
async fn a_loss_after_the_claim_stops_at_the_next_checkpoint() {
    let ran = run_to("pb-lose-open", Ended::Lost, Some(LoseAt::Open)).await;
    assert_eq!(ran.result, Err(ParentBoundError::Incomplete));
    assert_eq!(ran.calls, ["init", "open"]);
    assert_eq!(ran.pair.reached(), 2, "admitted, never served");
    assert!(!ran.item.present());
    assert!(ran.custody.upgrade().is_none());
}

#[tokio::test]
async fn this_builds_prerequisites_refuse_after_a_full_admission() {
    let item = Slot::default();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let pair = FakePair::default();
    let result = Box::pin(run(
        pair.clone(),
        &live(),
        later(),
        &mut Pending,
        || Ok(SessionCustody::fixture(token)),
        || Ok(item.clone()),
    ))
    .await;
    assert_eq!(result, Err(ParentBoundError::Unsupported));
    assert_eq!(pair.reached(), 2, "admitted, never served");
    assert!(!item.present());
    assert!(custody.upgrade().is_none());
}

#[test]
fn the_exit_follows_the_served_end_unless_a_stage_is_short_of_confirmed() {
    let clean = Settled {
        runtime: Shutdown {
            quit: None,
            store: Stage::Confirmed,
            settings: Stage::Confirmed,
            claude: Stage::Confirmed,
            account: Stage::Confirmed,
        },
        item: Stage::Confirmed,
    };
    let quit = |quit| Settled {
        runtime: Shutdown {
            quit: Some(quit),
            ..clean.runtime
        },
        ..clean
    };
    let quitting = Ok(Ended::Quit { deadline: later() });
    assert_eq!(exit(&quitting, quit(Quit::Complete)), Ok(()));
    assert_eq!(
        exit(&quitting, quit(Quit::Incomplete)),
        Err(ParentBoundError::Incomplete)
    );
    let unconfirmed = Settled {
        item: Stage::Unconfirmed,
        ..quit(Quit::Complete)
    };
    assert_eq!(
        exit(&quitting, unconfirmed),
        Err(ParentBoundError::Incomplete)
    );
    for ended in [
        Ended::Lost,
        Ended::Fatal(crate::runtime::Fatal::Refused),
        Ended::Broken("broken"),
        Ended::Unavailable,
    ] {
        assert_eq!(
            exit(&Ok(ended), clean),
            Err(ParentBoundError::Refused),
            "{ended:?}"
        );
    }
    for refused in [
        ParentBoundError::Unsupported,
        ParentBoundError::Busy,
        ParentBoundError::Refused,
    ] {
        assert_eq!(exit(&Err(refused), clean), Err(refused));
        let short = Settled {
            runtime: Shutdown {
                store: Stage::Unconfirmed,
                ..clean.runtime
            },
            ..clean
        };
        assert_eq!(
            exit(&Err(refused), short),
            Err(ParentBoundError::Incomplete),
            "{refused:?}"
        );
    }
}

/// Runs a parent-bound start on `pair` with the fixture stages, `custody` and `item`, losing
/// the parent at `lose` if set; returns its result and the stages it ran. The platforms' own
/// pair tests use it too.
pub(crate) async fn run_on_fixtures<S: InstanceStore, P: AdmissionPair>(
    scratch: &Scratch,
    pair: P,
    shell: &ShellProcess,
    by: Instant,
    item: S,
    custody: SessionCustody,
    lose: Option<LoseAt>,
) -> (Result<(), ParentBoundError>, Vec<&'static str>) {
    let (mut pre, _) = Scripted::new(scratch, Start::Held);
    pre.lose = lose.map(|at| (at, shell.clone()));
    let result = Box::pin(run(pair, shell, by, &mut pre, || Ok(custody), || Ok(item))).await;
    (result, pre.calls)
}
