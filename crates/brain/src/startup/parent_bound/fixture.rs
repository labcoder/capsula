#[cfg(test)]
use std::io;
#[cfg(all(debug_assertions, feature = "test-support"))]
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[cfg(all(debug_assertions, feature = "test-support"))]
use kumochi_protocol::Secret;
use kumochi_protocol::message::CAPABILITY_LEN;
use tokio::sync::mpsc as async_mpsc;
#[cfg(all(debug_assertions, feature = "test-support"))]
use zeroize::Zeroizing;

use super::{
    Accounts, AdmissionPair, BrainError, InstanceStore, Instant, Job, NotReady, OAuthTuning,
    OpenedState, ParentBoundError, Prerequisites, ProcessInit, SessionCustody, SessionLock,
    ShellProcess, TokenTuning, VaultHeld, run,
};
#[cfg(test)]
use crate::connections::error::VaultError;
use crate::engines::fixture::Fixture;
use crate::fixture_support::{Scratch, held_accounts_until};
#[cfg(test)]
use crate::platform::credential::CredentialStoreError;
use crate::runtime::Selected;
use crate::startup::instance::Added;
use crate::startup::open_state_held;
#[cfg(test)]
use crate::test_support::failing_accounts;

const PATIENCE: Duration = Duration::from_secs(60);

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
pub(crate) struct Holding {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only unit tests hold credential release")
    )]
    pub(crate) entered: async_mpsc::UnboundedReceiver<()>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only unit tests hold credential release")
    )]
    pub(crate) free: mpsc::Sender<()>,
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
pub(super) enum Start {
    /// Held accounts on the profile lock.
    Held,
    /// A startup load that finds native credentials unsupported.
    #[cfg(test)]
    Unsupported,
    /// A startup load that panics.
    #[cfg(test)]
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
pub(super) struct Scripted<'a> {
    scratch: &'a Scratch,
    start: Start,
    script: &'static str,
    hold: Option<(async_mpsc::UnboundedSender<()>, mpsc::Receiver<()>)>,
    pub(super) calls: Vec<&'static str>,
    lose: Option<(LoseAt, ShellProcess)>,
}

impl<'a> Scripted<'a> {
    pub(super) fn new(scratch: &'a Scratch, start: Start) -> (Self, Holding) {
        let (parts, holding) = hold();
        let pre = Self {
            scratch,
            start,
            script: "",
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
        #[cfg(test)]
        {
            let logs = crate::logging::file_subscriber(&self.scratch.dir().join("logs"), "brain")
                .map_err(|_| BrainError::Process(io::ErrorKind::Other.into()))?;
            Ok(ProcessInit {
                _scoped: Some(tracing::subscriber::set_default(logs)),
            })
        }
        #[cfg(not(test))]
        Ok(ProcessInit::default())
    }

    fn open(
        &mut self,
        custody: &SessionCustody,
        acquired: &mut bool,
    ) -> Result<OpenedState, BrainError> {
        self.calls.push("open");
        let replay = Some(Selected::Fixture(Box::new(
            Fixture::parse(self.script).expect("fixture script"),
        )));
        // A token custody has no lock to lend, so its profile is a lock on the scratch store.
        let opened = match custody.lend() {
            Some(profile) => open_state_held(profile, replay, acquired),
            None => OpenedState::load_marking(
                &self.scratch.db(),
                || Some(self.scratch.dir().to_owned()),
                replay,
                acquired,
            ),
        };
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
            #[cfg(test)]
            Start::Unsupported => failing_accounts(hold, || {
                Err(VaultError::Store(CredentialStoreError::Unsupported))
            })?,
            #[cfg(test)]
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

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "the vault's open panics on purpose"
)]
fn panicking_open()
-> Result<crate::connections::vault::Vault<crate::connections::vault::fake::FakeStore>, VaultError>
{
    panic!("the vault's open panics")
}

/// How a fixture run goes beyond its defaults.
#[derive(Default)]
pub(crate) struct Script {
    pub(crate) fixture: &'static str,
    /// Where the parent is lost, if anywhere.
    pub(crate) lose: Option<LoseAt>,
    /// Takes the accounts' release hold, so the test decides when the credential thread lets go;
    /// without it the release never waits.
    pub(crate) hold: Option<mpsc::Sender<Holding>>,
}

/// Runs a parent-bound start on `pair` with the fixture stages, `custody` and `item`, as
/// `script` says; returns its result and the stages it ran. The platforms' own pair tests use it
/// too.
pub(crate) async fn run_on_fixtures<S: InstanceStore, P: AdmissionPair>(
    scratch: &Scratch,
    pair: P,
    shell: &ShellProcess,
    by: Instant,
    item: S,
    custody: SessionCustody,
    script: Script,
) -> (Result<(), ParentBoundError>, Vec<&'static str>) {
    let (mut pre, holding) = Scripted::new(scratch, Start::Held);
    pre.script = script.fixture;
    pre.lose = script.lose.map(|at| (at, shell.clone()));
    match script.hold {
        Some(hold) => hold.send(holding).expect("the test takes the hold"),
        None => drop(holding),
    }
    let result = Box::pin(run(pair, shell, by, &mut pre, || Ok(custody), || Ok(item))).await;
    (result, pre.calls)
}

/// Where a held add says it entered, and what releases it.
#[cfg(test)]
type HeldAdd = Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>;

/// A fake item whose next add can be held: the test hears it enter and releases it.
#[derive(Clone, Default)]
pub(super) struct Slot {
    item: Arc<std::sync::Mutex<Option<[u8; CAPABILITY_LEN]>>>,
    #[cfg(test)]
    hold: Arc<std::sync::Mutex<HeldAdd>>,
    #[cfg(all(debug_assertions, feature = "test-support"))]
    seed: Option<mpsc::Sender<Seed>>,
}

impl Slot {
    pub(super) fn present(&self) -> bool {
        self.item.lock().expect("item").is_some()
    }

    #[cfg(test)]
    pub(super) fn hold_add(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        *self.hold.lock().expect("hold") = Some((entered, released));
        (started, release)
    }
}

impl InstanceStore for Slot {
    type Error = ();

    fn add(&self, key: &[u8; CAPABILITY_LEN]) -> Result<Added, ()> {
        #[cfg(test)]
        if let Some((entered, released)) = self.hold.lock().expect("hold").take() {
            entered.send(()).expect("the test listens");
            released.recv_timeout(PATIENCE).expect("the test releases");
        }
        #[cfg(all(debug_assertions, feature = "test-support"))]
        if let Some(seed) = &self.seed {
            seed.send(Secret::new(Zeroizing::new(*key)))
                .map_err(|_| ())?;
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

#[cfg(all(debug_assertions, feature = "test-support"))]
type Seed = Secret<Zeroizing<[u8; CAPABILITY_LEN]>>;

/// Serve the real inherited pair on the offline startup fixture. The synthetic capability is
/// published to the peer through `seed`; no product namespace or native route is opened.
///
/// # Errors
/// Refused or incomplete admission, serving or settlement, including expiry of `by`, or no
/// durable interruption of the fixture's run.
#[cfg(all(debug_assertions, feature = "test-support"))]
pub fn serve_pair_credit_fixture(
    fds: crate::platform::InheritedFds,
    seed: mpsc::Sender<Seed>,
    by: std::time::Instant,
) -> Result<crate::store::Interruption, ParentBoundError> {
    use crate::store::Interruption;
    use rusqlite::{Connection, OpenFlags};

    let pair = fds.adopt().map_err(|_| ParentBoundError::Refused)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ParentBoundError::Refused)?;
    let by = Instant::from_std(by);
    let scratch = Scratch::new("pair-credit");
    let shell = ShellProcess::fixture(1, Arc::new(AtomicU8::new(0)));
    let custody = SessionCustody::fixture(Arc::new(()));
    let item = Slot {
        seed: Some(seed),
        ..Slot::default()
    };
    let script = Script {
        fixture: "0 text a\n0 text b\n0 text c\n0 text d\n0 text e\n0 text f\n10000 text z",
        ..Script::default()
    };
    runtime.block_on(async {
        tokio::time::timeout_at(
            by,
            crate::runtime::credit_fixture::SATURATE.scope(
                (),
                run_on_fixtures(&scratch, pair, &shell, by, item, custody, script),
            ),
        )
        .await
        .map_err(|_| ParentBoundError::Refused)?
        .0
    })?;
    if Instant::now() >= by {
        return Err(ParentBoundError::Incomplete);
    }
    let db = Connection::open_with_flags(scratch.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| ParentBoundError::Incomplete)?;
    db.busy_timeout(Duration::ZERO)
        .map_err(|_| ParentBoundError::Incomplete)?;
    let (state, reason): (String, Option<String>) = db
        .query_row("SELECT state, reason FROM runs WHERE id = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|_| ParentBoundError::Incomplete)?;
    if Instant::now() >= by {
        return Err(ParentBoundError::Incomplete);
    }
    match (state.as_str(), reason.as_deref()) {
        ("interrupted", Some("stop")) => Ok(Interruption::Stop),
        ("interrupted", Some("view_stalled")) => Ok(Interruption::ViewStalled),
        _ => Err(ParentBoundError::Incomplete),
    }
}
