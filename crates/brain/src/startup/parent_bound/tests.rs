use std::io;
use std::sync::{Arc, Weak, mpsc};
use std::time::Duration;

use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::CAPABILITY_LEN;
use tokio::time::timeout;

use super::fixture::{Holding, LoseAt, Script, Scripted, Slot, Start, run_on_fixtures};
use super::*;
use crate::platform::SessionLockError;
use crate::runtime::REFUSE_SPAWN;
use crate::startup::custody::SessionCustody;
use crate::startup::instance::Added;
use crate::test_support::Scratch;

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
    let settle = root.settle(None, ());
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
    let settled = timeout(PATIENCE, Box::pin(root.settle(None, ())))
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
    let settled = Box::pin(root.settle(None, ())).await;
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
    let settled = Box::pin(root.settle(None, ())).await;
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
    let settled = Box::pin(root.settle(None, ())).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Unconfirmed,
        "SQLite opened before the setup failed: {settled:?}"
    );
}

#[test]
fn first_light_is_ready_only_where_parent_bound_is_supported() {
    let expected = if platform::supports(Mode::ParentBound) {
        Ok(())
    } else {
        Err(NotReady)
    };
    assert_eq!(FirstLight::new().ready(), expected);
}

#[test]
fn first_light_allows_one_model_request_and_refuses_the_second() {
    let first_light = FirstLight::new();
    let shared = first_light.budget.clone();
    assert!(first_light.budget.reserve());
    assert!(!first_light.budget.reserve(), "a second round is spent");
    assert!(!shared.reserve(), "a clone shares the count");
    assert_eq!(shared.spent(), 1);
}

/// A pair that records how far it was driven: 1 once keyed, 2 once greeted, 3 once served, when
/// serving ends as `ends`; `end` sets its session cell, as a live session does when it ends.
#[derive(Clone)]
struct FakePair {
    reached: Arc<std::sync::atomic::AtomicU8>,
    ends: Ended,
    cell: Arc<watch::Sender<bool>>,
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
            cell: Arc::new(watch::channel(false).0),
        }
    }

    fn end(&self) {
        self.cell.send_replace(true);
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

    fn cell(greeted: &Self) -> watch::Receiver<bool> {
        greeted.cell.subscribe()
    }

    fn serve(
        greeted: &mut Self,
        _: &mut Brain,
        _: Terminal,
        _: &ShellProcess,
        _: Instant,
    ) -> impl Future<Output = Ended> {
        greeted.reach(3);
        std::future::ready(greeted.ends)
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
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
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
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
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
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
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
    assert_eq!(Box::pin(root.settle(None, ())).await.item, Stage::Confirmed);
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
            Script {
                lose,
                ..Script::default()
            },
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

/// Prerequisites none of whose stages may run: `ready` refuses, or fails the test when even asking
/// is too early, and every later stage fails the test.
struct Unready {
    ask: bool,
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "a stage that runs before readiness fails the test"
)]
impl Prerequisites for Unready {
    fn preflight(&mut self, _: &mut dyn FnMut() -> bool) -> Result<(), ParentBoundError> {
        Ok(())
    }

    fn ready(&self) -> Result<(), NotReady> {
        assert!(self.ask, "readiness is asked only after the entry fence");
        Err(NotReady)
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        panic!("init runs only once ready")
    }

    fn open(&mut self, _: &SessionCustody, _: &mut bool) -> Result<OpenedState, BrainError> {
        panic!("open runs only once ready")
    }

    fn accounts(
        &mut self,
        _: OAuthTuning,
        _: TokenTuning,
        _: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        panic!("accounts start only once ready")
    }

    fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError> {
        panic!("containment is asked only once ready")
    }
}

/// A preflight over `calls` native calls that fires `during` once `after` of them were made, or
/// before it answers when it makes none, as a latched answer does; it refuses the way first
/// light does once `live` says no. `init` records that the stages started, and refuses.
struct Checking<F> {
    calls: usize,
    after: usize,
    during: F,
    made: usize,
    started: bool,
}

impl<F: FnMut()> Checking<F> {
    fn new(calls: usize, after: usize, during: F) -> Self {
        Self {
            calls,
            after,
            during,
            made: 0,
            started: false,
        }
    }
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "a stage past a refused init fails the test"
)]
impl<F: FnMut()> Prerequisites for Checking<F> {
    fn preflight(&mut self, live: &mut dyn FnMut() -> bool) -> Result<(), ParentBoundError> {
        if self.calls == 0 {
            (self.during)();
        }
        for call in 1..=self.calls {
            if !live() {
                return Err(ParentBoundError::Busy);
            }
            self.made = call;
            if call == self.after {
                (self.during)();
            }
        }
        Ok(())
    }

    fn ready(&self) -> Result<(), NotReady> {
        Ok(())
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        self.started = true;
        Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
    }

    fn open(&mut self, _: &SessionCustody, _: &mut bool) -> Result<OpenedState, BrainError> {
        panic!("nothing opens once init refused")
    }

    fn accounts(
        &mut self,
        _: OAuthTuning,
        _: TokenTuning,
        _: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        panic!("no accounts start once init refused")
    }

    fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError> {
        panic!("no containment once init refused")
    }
}

/// Runs a full admission on a fake pair and a token custody, then the stages under `pre`.
async fn staged(
    pair: &FakePair,
    shell: &ShellProcess,
    by: Instant,
    pre: &mut impl Prerequisites,
) -> Result<(), ParentBoundError> {
    Box::pin(run(
        pair.clone(),
        shell,
        by,
        pre,
        || Ok(SessionCustody::fixture(Arc::new(()))),
        || Ok(Slot::default()),
    ))
    .await
}

/// The parent lost, the session ended or the deadline passed while the first call or the last
/// was in flight, or while a latched answer made none: no further call is made, no stage
/// starts, and the start is Refused whatever the preflight answered.
#[tokio::test]
async fn a_preflight_stops_at_a_lost_parent_an_ended_session_or_the_deadline() {
    for (calls, after, made) in [(3, 1, 1), (3, 3, 3), (0, 0, 0)] {
        for signal in ["parent", "session", "deadline"] {
            let shell = live();
            let pair = FakePair::default();
            let soon = Instant::now() + Duration::from_millis(200);
            let by = if signal == "deadline" { soon } else { later() };
            let mut pre = Checking::new(calls, after, || match signal {
                "parent" => assert!(!shell.publish(1), "the claim came first"),
                "session" => pair.end(),
                _ => std::thread::sleep(
                    soon.into_std()
                        .saturating_duration_since(std::time::Instant::now()),
                ),
            });
            let ran = staged(&pair, &shell, by, &mut pre).await;
            let case = format!("{signal} after {after} of {calls}");
            assert_eq!(ran, Err(ParentBoundError::Refused), "{case}");
            assert_eq!(pre.made, made, "{case}: no call after the one in flight");
            assert!(!pre.started, "{case}: no stage started");
            assert_eq!(pair.reached(), 2, "{case}: admitted, never served");
        }
    }
}

/// With nothing fired the same preflight makes every call, after admission, and the stages go
/// on to start.
#[tokio::test]
async fn a_preflight_that_stays_live_makes_every_call_before_the_stages() {
    let pair = FakePair::default();
    let mut pre = Checking::new(3, 3, || {});
    let ran = staged(&pair, &live(), later(), &mut pre).await;
    assert_eq!(ran, Err(ParentBoundError::Refused), "init refused");
    assert_eq!(pre.made, 3);
    assert!(pre.started, "the stages came after the preflight");
    assert_eq!(pair.reached(), 2);
}

#[tokio::test]
async fn unready_prerequisites_refuse_after_a_full_admission_before_any_stage() {
    let item = Slot::default();
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let pair = FakePair::default();
    let result = Box::pin(run(
        pair.clone(),
        &live(),
        later(),
        &mut Unready { ask: true },
        || Ok(SessionCustody::fixture(token)),
        || Ok(item.clone()),
    ))
    .await;
    assert_eq!(result, Err(ParentBoundError::Unsupported));
    assert_eq!(pair.reached(), 2, "admitted, never served");
    assert!(!item.present());
    assert!(custody.upgrade().is_none());
}

/// A committed start whose parent is already lost, or whose clock already ran out, refuses on
/// entry, before it asks whether it's ready.
#[test]
fn the_stage_driver_fences_on_entry_before_asking_whether_its_ready() {
    for lost in [true, false] {
        let shell = live();
        let committed = shell.claim_commit().expect("the one claim");
        let by = if lost {
            assert!(!shell.publish(1), "the claim came first");
            later()
        } else {
            Instant::now()
        };
        let (_session, cell) = watch::channel(false);
        let mut root = Root::<NoItem>::new();
        assert_eq!(
            root.stage(&mut Unready { ask: false }, &shell, by, &cell, &committed),
            Err(ParentBoundError::Refused),
            "lost: {lost}"
        );
    }
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

/// First light's own prerequisites on scratch custody: only a macOS `first-light` build has them.
#[cfg(all(target_os = "macos", feature = "first-light"))]
mod first_light {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use std::sync::OnceLock;

    use kumochi_protocol::constants::{AUTHORITY_FILE, CONFIG_FILE, DATA_DIR, STORE_FILE};
    use objc2_security::{errSecInteractionNotAllowed, errSecItemNotFound, errSecSuccess};

    use super::*;
    use crate::fixture_support::held_accounts_until;
    use crate::platform::credential::legacy_absent;
    use crate::platform::{instance_fixture, instance_fixture_clean};
    use crate::store::StoreError;

    type Legacy = fn(&mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError>;

    /// The legacy check on status answers that find every name absent.
    fn absent(live: &mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError> {
        legacy_absent(&OnceLock::new(), live, |_| Ok(errSecItemNotFound))
    }

    /// First light's preflight, ready, open and containment, with the legacy check on fake status
    /// answers, offline accounts on a fake vault and no process-wide init; it records whether the
    /// authority was there before the accounts, and whether `open` was asked.
    struct Offline {
        inner: FirstLight,
        authority: Option<bool>,
        opened: bool,
    }

    impl Offline {
        fn new() -> Self {
            Self::with(absent)
        }

        fn with(legacy: Legacy) -> Self {
            Self {
                inner: FirstLight {
                    legacy,
                    ..FirstLight::new()
                },
                authority: None,
                opened: false,
            }
        }
    }

    impl Prerequisites for Offline {
        fn preflight(&mut self, live: &mut dyn FnMut() -> bool) -> Result<(), ParentBoundError> {
            self.inner.preflight(live)
        }

        fn ready(&self) -> Result<(), NotReady> {
            self.inner.ready()
        }

        fn init(&mut self) -> Result<ProcessInit, BrainError> {
            Ok(ProcessInit::default())
        }

        fn open(
            &mut self,
            custody: &SessionCustody,
            acquired: &mut bool,
        ) -> Result<OpenedState, BrainError> {
            self.opened = true;
            self.inner.open(custody, acquired)
        }

        fn accounts(
            &mut self,
            _: OAuthTuning,
            _: TokenTuning,
            profile: Arc<SessionLock>,
        ) -> Result<(Accounts, VaultHeld), BrainError> {
            // The fake vault opens with `Missing::Create`, so the open's own creation is checked here.
            self.authority = Some(profile.authority_path().is_file());
            Ok((held_accounts_until(profile, ()), Arc::default()))
        }

        fn containment(&mut self) -> Result<Option<Arc<Job>>, BrainError> {
            self.inner.containment()
        }
    }

    /// The fixture item, which moves custody's root aside for an empty stand-in at its first
    /// delete: in settlement, after the store closed and before custody's last check.
    struct Swapping {
        slot: Slot,
        support: PathBuf,
        swapped: std::sync::atomic::AtomicBool,
    }

    impl InstanceStore for Swapping {
        type Error = ();

        fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ()> {
            self.slot.add(key)
        }

        fn inspect(&self) -> Result<(), ()> {
            self.slot.inspect()
        }

        fn update(&self, key: &[u8; CAPABILITY_LEN]) -> Result<(), ()> {
            self.slot.update(key)
        }

        fn read(&self, into: &mut [u8; CAPABILITY_LEN]) -> Result<(), ()> {
            self.slot.read(into)
        }

        fn delete(&self) -> Result<(), ()> {
            use std::os::unix::fs::DirBuilderExt as _;

            if !self.swapped.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let product = self.support.join(DATA_DIR);
                std::fs::rename(&product, self.support.join("moved")).expect("move the root");
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&product)
                    .expect("a stand-in root");
            }
            self.slot.delete()
        }
    }

    fn entries(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder)
            .expect("the product folder")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        names.sort();
        names
    }

    /// Removes exactly `expected` once the product folder holds exactly that, then the folders;
    /// anything else is left in place, and the assertion names it.
    fn clean(support: &Path, expected: &[&str]) {
        assert_eq!(
            entries(&support.join(DATA_DIR)),
            sorted(expected),
            "the fixture holds only what the run should make"
        );
        instance_fixture_clean(support, expected);
    }

    /// A Quit run on fresh custody in `support`; `item` makes the instance store.
    async fn run_quit<S: InstanceStore>(
        support: &Path,
        pre: &mut Offline,
        item: impl FnOnce() -> Result<S, S::Error>,
    ) -> Result<(), ParentBoundError> {
        timeout(
            PATIENCE,
            Box::pin(run(
                FakePair::ending(Ended::Quit { deadline: later() }),
                &live(),
                later(),
                pre,
                || SessionCustody::fresh_at(support),
                item,
            )),
        )
        .await
        .expect("the run settles")
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "an item made for a refused root fails the test"
    )]
    fn no_item() -> Result<Slot, ()> {
        panic!("a refused root publishes no item")
    }

    fn refused_root(result: &Result<OpenedState, BrainError>) -> bool {
        matches!(
            result,
            Err(BrainError::Store(StoreError::Folder(error)))
                if error.kind() == io::ErrorKind::AlreadyExists
        )
    }

    /// What a Quit run leaves in its root: the lock, the store and the authority, closed.
    const RAN: [&str; 3] = [SESSION_LOCK, STORE_FILE, AUTHORITY_FILE];

    #[tokio::test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    async fn a_fresh_root_serves_a_quit_cleanly_and_a_second_run_on_it_is_refused() {
        let support = instance_fixture("fl-fresh");
        let product = support.join(DATA_DIR);
        let mut first = Offline::new();
        let budget = first.inner.budget.clone();
        assert_eq!(
            run_quit(&support, &mut first, || Ok(Slot::default())).await,
            Ok(())
        );
        assert_eq!(
            first.authority,
            Some(true),
            "the open created the authority"
        );
        assert_eq!(entries(&product), sorted(&RAN), "no file log, nothing else");
        assert_eq!(
            budget.spent(),
            0,
            "admission and serving send no model request"
        );
        let mut second = Offline::new();
        assert_eq!(
            run_quit(&support, &mut second, no_item).await,
            Err(ParentBoundError::Refused)
        );
        assert_eq!(second.authority, None, "no accounts start on a used root");
        clean(&support, &RAN);
    }

    #[tokio::test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    async fn an_existing_empty_root_is_refused_before_anything_opens() {
        let support = instance_fixture("fl-existing");
        drop(SessionCustody::bootstrap_at(&support).expect("an earlier custody made the root"));
        let mut pre = Offline::new();
        assert_eq!(
            run_quit(&support, &mut pre, no_item).await,
            Err(ParentBoundError::Refused)
        );
        assert_eq!(pre.authority, None);
        clean(&support, &[SESSION_LOCK]);
    }

    /// Custody's root moved away before settlement ended: the store and the item settle, but the
    /// run doesn't settle clean.
    #[tokio::test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    async fn a_root_replaced_during_the_run_does_not_settle_clean() {
        let support = instance_fixture("fl-swapped");
        let (product, moved) = (support.join(DATA_DIR), support.join("moved"));
        let item = Swapping {
            slot: Slot::default(),
            support: support.clone(),
            swapped: false.into(),
        };
        assert_eq!(
            run_quit(&support, &mut Offline::new(), || Ok(item)).await,
            Err(ParentBoundError::Incomplete)
        );
        assert!(
            entries(&product).is_empty(),
            "nothing landed in the stand-in"
        );
        std::fs::remove_dir(&product).expect("remove the stand-in");
        std::fs::rename(&moved, &product).expect("restore the root");
        clean(&support, &RAN);
    }

    fn present(live: &mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError> {
        legacy_absent(&OnceLock::new(), live, |_| Ok(errSecSuccess))
    }

    fn unanswered(live: &mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError> {
        legacy_absent(&OnceLock::new(), live, |_| {
            Err(CredentialStoreError::Read(errSecInteractionNotAllowed))
        })
    }

    fn unadmitted(_: &mut dyn FnMut() -> bool) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::Unsupported)
    }

    /// An authenticated, committed start on a token custody whose preflight refuses.
    async fn refused_start(legacy: Legacy) -> (Result<(), ParentBoundError>, FakePair, Offline) {
        let pair = FakePair::ending(Ended::Quit { deadline: later() });
        let mut pre = Offline::with(legacy);
        let ran = timeout(
            PATIENCE,
            Box::pin(run(
                pair.clone(),
                &live(),
                later(),
                &mut pre,
                || Ok(SessionCustody::fixture(Arc::new(()))),
                || Ok(Slot::default()),
            )),
        )
        .await
        .expect("the run settles");
        (ran, pair, pre)
    }

    /// The scan runs only once the pair is authenticated and committed, and refuses before
    /// anything opens: no store, no authority, no vault, no serving.
    #[tokio::test]
    async fn a_legacy_name_present_or_unanswered_is_busy_after_admission_before_any_open() {
        let cases: [(Legacy, &str); 2] = [(present, "present"), (unanswered, "an error")];
        for (legacy, label) in cases {
            let (ran, pair, pre) = refused_start(legacy).await;
            assert_eq!(ran, Err(ParentBoundError::Busy), "{label}");
            assert_eq!(pair.reached(), 2, "{label}: admitted, never served");
            assert!(!pre.opened, "{label}: no store opened");
            assert_eq!(pre.authority, None, "{label}: no accounts or vault");
        }
    }

    #[tokio::test]
    async fn an_unadmitted_store_is_refused_after_admission_before_any_open() {
        let (ran, pair, pre) = refused_start(unadmitted).await;
        assert_eq!(ran, Err(ParentBoundError::Refused));
        assert_eq!(pair.reached(), 2);
        assert!(!pre.opened);
        assert_eq!(pre.authority, None);
    }

    /// In a real fresh root, a legacy refusal leaves custody's lock and nothing else: no store,
    /// authority or settings file.
    #[tokio::test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    async fn a_legacy_refusal_leaves_only_custodys_lock_in_a_fresh_root() {
        let support = instance_fixture("fl-legacy");
        let mut pre = Offline::with(present);
        assert_eq!(
            run_quit(&support, &mut pre, || Ok(Slot::default())).await,
            Err(ParentBoundError::Busy)
        );
        assert_eq!(pre.authority, None);
        clean(&support, &[SESSION_LOCK]);
    }

    /// Every file first light refuses to find in a fresh root, written out by hand.
    fn leftovers() -> Vec<String> {
        let mut names = Vec::new();
        for db in [STORE_FILE, AUTHORITY_FILE] {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                names.push(format!("{db}{suffix}"));
            }
        }
        names.push(CONFIG_FILE.to_owned());
        names.push(format!("{CONFIG_FILE}.tmp"));
        names.push(format!("{STORE_FILE}.purge-old"));
        for copy in [".purge-snapshot", ".purge-clean"] {
            for suffix in ["", "-journal", "-wal", "-shm"] {
                names.push(format!("{STORE_FILE}{copy}{suffix}"));
            }
        }
        names
    }

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    fn a_lone_leftover_in_a_fresh_root_is_refused_for_good_before_the_store_opens() {
        const SENTINEL: &str = "SENTINEL-r2b-7f3a";
        let names = leftovers();
        assert_eq!(names.len(), 19);
        for name in names {
            let support = instance_fixture(SENTINEL);
            let custody = SessionCustody::fresh_at(&support).expect("fresh custody");
            let product = support.join(DATA_DIR);
            let leftover = product.join(&name);
            std::fs::write(&leftover, SENTINEL).expect("plant one leftover");
            let mut pre = FirstLight::new();
            let mut acquired = false;
            let refused = pre.open(&custody, &mut acquired);
            assert!(refused_root(&refused), "{name}");
            assert!(!acquired, "{name}: no store opened");
            let mut expected = vec![name.clone(), SESSION_LOCK.to_owned()];
            expected.sort();
            assert_eq!(entries(&product), expected, "{name}");
            let error = refused.err().expect("refused");
            let shown = format!("{error:?} {error}");
            assert!(!shown.contains(SENTINEL), "{shown}");
            assert!(
                !shown
                    .as_bytes()
                    .windows(SENTINEL.len())
                    .any(|bytes| bytes == SENTINEL.as_bytes())
            );
            std::fs::remove_file(&leftover).expect("remove the leftover");
            assert!(
                refused_root(&pre.open(&custody, &mut acquired)),
                "{name}: refusal is final"
            );
            drop(custody);
            clean(&support, &[SESSION_LOCK]);
        }
    }

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
    fn a_root_replaced_after_custody_created_it_is_refused() {
        use std::os::unix::fs::DirBuilderExt as _;

        let support = instance_fixture("fl-moved");
        let custody = SessionCustody::fresh_at(&support).expect("fresh custody");
        let product = support.join(DATA_DIR);
        let moved = support.join("moved");
        std::fs::rename(&product, &moved).expect("move the created root");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&product)
            .expect("a stand-in root");
        let mut acquired = false;
        assert!(refused_root(
            &FirstLight::new().open(&custody, &mut acquired)
        ));
        assert!(
            entries(&product).is_empty(),
            "nothing opened in the stand-in"
        );
        drop(custody);
        std::fs::remove_dir(&product).expect("remove the stand-in");
        std::fs::rename(&moved, &product).expect("restore the root");
        clean(&support, &[SESSION_LOCK]);
    }

    #[test]
    fn a_token_custody_opens_nothing() {
        let mut acquired = false;
        let opened = FirstLight::new().open(&SessionCustody::fixture(Arc::new(())), &mut acquired);
        assert!(matches!(
            opened,
            Err(BrainError::ProfileLock(error)) if error.kind() == io::ErrorKind::NotFound
        ));
        assert!(!acquired);
    }

    const INIT_CHILD: &str = "KUMOCHI_TEST_FIRST_LIGHT_INIT_CHILD";
    const INITIALIZED: i32 = 77;

    /// Run only by the test below, as a child: init installs process-wide services.
    #[test]
    #[ignore = "subprocess fixture that installs process-wide services"]
    fn init_child() {
        if std::env::var_os(INIT_CHILD).is_none() {
            return;
        }
        let started = FirstLight::new().init().is_ok();
        let quiet = !tracing::dispatcher::has_been_set();
        std::process::exit(if started && quiet && kumochi_scene::deferred::running() {
            INITIALIZED
        } else {
            1
        });
    }

    #[test]
    fn first_light_init_starts_the_services_with_no_file_log_or_subscriber() {
        let scratch = Scratch::new("fl-init");
        std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
        let home: PathBuf = scratch.dir().join("home");
        #[expect(
            clippy::disallowed_methods,
            reason = "this macOS test runs its own fixture child, which inherits no handles"
        )]
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--ignored",
                "--exact",
                "startup::parent_bound::tests::first_light::init_child",
            ])
            .env_clear()
            .env(INIT_CHILD, "1")
            .env("HOME", &home)
            .current_dir(scratch.dir())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the fixture child");
        let by = std::time::Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child's status") {
                break status;
            }
            if std::time::Instant::now() >= by {
                let stopped =
                    child.kill().is_ok() || child.try_wait().expect("the child's status").is_some();
                assert!(stopped, "couldn't stop the init child");
                let reap_by = std::time::Instant::now() + Duration::from_secs(2);
                while child.try_wait().expect("the child's status").is_none() {
                    assert!(
                        std::time::Instant::now() < reap_by,
                        "the init child didn't exit"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("the init child didn't end by its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(INITIALIZED));
        assert_eq!(
            std::fs::read_dir(scratch.dir()).expect("scratch").count(),
            0,
            "no log folder or file"
        );
    }
}
