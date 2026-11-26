//! Kumochi's brain: runs, tools, connections, the store and the helpers' Job Object.
//!
//! The library holds everything; `main.rs` only starts it, so integration tests can reach the
//! platform code.

pub mod bridge;
mod config;
pub mod connections;
mod context;
pub mod coordinator;
pub mod credential;
pub mod dispatch;
pub mod drawing;
pub mod engines;
#[cfg(any(test, all(debug_assertions, feature = "test-support")))]
mod fixture_support;
pub mod ipc;
pub mod issues;
mod jobs;
pub mod logging;
pub mod platform;
mod recall;
mod relaunch;
mod runtime;
#[cfg(any(test, all(debug_assertions, feature = "test-support")))]
#[expect(
    clippy::print_stderr,
    reason = "the shared fixture reports cleanup failures during unwind"
)]
#[path = "../../../tests/support/scratch.rs"]
mod scratch;
pub mod screen;
pub mod speech;
mod startup;
pub mod store;
mod supervise;
mod tap;
#[cfg(test)]
mod test_support;
#[cfg(all(debug_assertions, feature = "test-support"))]
pub use startup::parent_bound::fixture::serve_pair_credit_fixture;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "PR 5 wires speech input; PR 7 wires spoken replies; PR 8 wires voice installation"
    )
)]
mod voice;

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{ModelClient, Tls};
#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
use crate::connections::model::text_observation::Observation;
#[cfg(debug_assertions)]
use crate::engines::fixture::Fixture;
use crate::engines::responses::{AccessHandle, Responses};
use crate::ipc::{Identity, Pipes, Session};
use crate::platform::Job;
use crate::runtime::{Brain, Selected};
use crate::startup::run_admitted;

const STARTUP_LOG_TARGET: &str = module_path!();

/// How long a relaunched shell the brain can't watch gets to connect: past the shell's own
/// 10 s connect deadline, and short enough that three such losses plus the backoff still fit
/// in the one-minute crash window [guess].
const UNWATCHED_GRACE: Duration = Duration::from_secs(15);

/// The argument that starts `kumochid` as the screen worker.
pub const SCREEN_WORKER: &str = "--screen";
/// The argument that starts `kumochid` as the speech worker.
pub const SPEECH_WORKER: &str = "--speech";

/// The argument that starts `kumochid` as Claude Code's MCP bridge, before the launch value.
pub const MCP_BRIDGE: &str = "--mcp-bridge";

pub use kumochi_protocol::constants::PARENT_BOUND;

/// Why the brain stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    /// This build has no native runtime adapter.
    #[error("native brain startup is unsupported")]
    NativeRuntime(#[source] io::Error),
    /// The session id or the executable's path couldn't be read.
    #[error("can't read this process's own details")]
    Process(#[from] io::Error),
    /// The pipes couldn't be created, usually because another brain holds them.
    #[error(transparent)]
    Ipc(#[from] ipc::IpcError),
    /// The OS random source failed.
    #[error("no randomness for the capability")]
    Random(#[from] getrandom::Error),
    /// Credential Manager refused the capability.
    #[error(transparent)]
    Credential(#[from] credential::CredentialError),
    /// Another brain of this user, in another session, holds the profile's writer lock.
    #[error("another session is using this profile")]
    ProfileBusy,
    /// The profile's writer lock couldn't be created or taken.
    #[error("can't take the profile's writer lock")]
    ProfileLock(#[source] io::Error),
    /// The profile's missing credential authority couldn't be created.
    #[error("can't create the credential authority")]
    Authority,
    /// The store couldn't be opened or recovered.
    #[error("can't open the store")]
    Store(#[from] store::StoreError),
    /// The fixture script a dev build was pointed at doesn't load or parse.
    #[error("can't load the fixture script")]
    Fixture(#[source] io::Error),
    /// The store's thread couldn't be started.
    #[error("can't start the store's thread")]
    StoreThread(#[source] io::Error),
    /// The helpers' Job Object couldn't be created.
    #[error("can't start the helpers")]
    Helpers(#[source] io::Error),
    /// The account's credential thread couldn't be started.
    #[error("can't start the credential thread")]
    Accounts(#[source] io::Error),
    /// The model connection's TLS setup failed.
    #[error("can't set up the model connection")]
    Model(#[source] connections::http_client::HttpError),
}

impl BrainError {
    /// Returns the fixed startup diagnostic kind and any raw OS code without formatting the error.
    #[must_use]
    pub fn diagnostic(&self) -> (&'static str, Option<i64>) {
        use ipc::IpcError;
        use store::StoreError;

        let raw = |error: &io::Error| error.raw_os_error().map(i64::from);
        match self {
            Self::NativeRuntime(error) => ("native_runtime", raw(error)),
            Self::Process(error) => ("process", raw(error)),
            Self::Ipc(IpcError::Create { source, .. }) => ("ipc_create", raw(source)),
            Self::Ipc(IpcError::Accept(error)) => ("ipc_accept", raw(error)),
            Self::Random(error) => ("random", error.raw_os_error().map(i64::from)),
            Self::Credential(error) => ("credential", error.raw_os_error().map(i64::from)),
            Self::ProfileBusy => ("profile_busy", None),
            Self::ProfileLock(error) => ("profile_lock", raw(error)),
            Self::Authority => ("authority", None),
            Self::Store(StoreError::Folder(error) | StoreError::Purge(error)) => {
                ("store", raw(error))
            }
            Self::Store(_) => ("store", None),
            Self::Fixture(error) => ("fixture", raw(error)),
            Self::StoreThread(error) => ("store_thread", raw(error)),
            Self::Helpers(error) => ("helpers", raw(error)),
            Self::Accounts(error) => ("accounts", raw(error)),
            Self::Model(_) => ("model", None),
        }
    }
}

/// Ends the runtime [`run`] ran on without waiting for blocking work. Quit and close already
/// waited for a settings write up to their deadline; one still stuck on a slow disk ends with the
/// process, and `config.toml` stays whole because it's replaced, never written in place.
pub fn shut_down(runtime: tokio::runtime::Runtime) {
    end_runtime(
        runtime,
        #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
        Observation::current().map(AsRef::as_ref),
    );
}

fn end_runtime(
    runtime: tokio::runtime::Runtime,
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))] observation: Option<
        &Observation,
    >,
) {
    runtime.shutdown_timeout(Duration::ZERO);
    #[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
    if let Some(observation) = observation {
        observation.finish();
    }
}

/// Runs the brain until the shell quits or turns out to be from another build.
///
/// # Errors
/// Fails when the pipes can't be created (another brain holds them), the capability can't be
/// made or stored, the helpers can't start, a pipe fails outright, or this build has no native
/// runtime adapter.
pub fn run() -> impl Future<Output = Result<(), BrainError>> {
    use kumochi_protocol::constants::{LIVE_CHATGPT_TEXT_ENV, ProductIdentity};

    let admission = platform::require_mode(platform::Mode::Detached)
        .map_err(BrainError::NativeRuntime)
        .and_then(|()| {
            let mode = std::env::var(LIVE_CHATGPT_TEXT_ENV).ok();
            draw_admission(mode.as_deref(), ProductIdentity::ACTIVE)
        });
    run_admitted(
        admission,
        #[cfg(test)]
        || {},
    )
}

/// Why parent-bound startup ended without a clean quit. The executable maps each to an exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParentBoundError {
    /// This build, mode or identity doesn't start parent-bound, or its prerequisites aren't ready.
    #[error("parent-bound startup is unsupported")]
    Unsupported,
    /// Another session holds the instance.
    #[error("another session holds the instance")]
    Busy,
    /// The parent, the inherited pair, the greeting or the commit claim refused.
    #[error("parent-bound startup was refused")]
    Refused,
    /// Settlement couldn't confirm every stage.
    #[error("parent-bound settlement is incomplete")]
    Incomplete,
}

/// Starts a brain under its shell's lifetime, on the pair the executable took from descriptors 3
/// and 4: one admission clock from here, the process hardened, the pair adopted and the parent
/// watched, then admission, the stages and serving on a current-thread runtime. The parent watch
/// drops last, after that runtime.
///
/// # Errors
/// `Unsupported` before it reads `fds` unless this is a supporting build's Dev edition; Gate and
/// Release names would reach another edition's data.
pub fn run_parent_bound(fds: platform::InheritedFds) -> Result<(), ParentBoundError> {
    use kumochi_protocol::constants::ProductIdentity;

    if !platform::admits_parent_bound(ProductIdentity::ACTIVE) {
        return Err(ParentBoundError::Unsupported);
    }
    let by = tokio::time::Instant::now() + startup::parent_bound::ADMISSION;
    platform::harden().map_err(|_| ParentBoundError::Refused)?;
    let pair = fds.adopt().map_err(|_| ParentBoundError::Refused)?;
    let shell = platform::ShellProcess::arm_parent().map_err(|_| ParentBoundError::Refused)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ParentBoundError::Refused)?;
    let ended = runtime.block_on(startup::parent_bound::run_production(pair, &shell, by));
    drop(runtime);
    drop(shell);
    ended
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartMode {
    Normal,
    Draw,
}

fn draw_admission(
    mode: Option<&str>,
    identity: kumochi_protocol::constants::ProductIdentity,
) -> Result<StartMode, BrainError> {
    use kumochi_protocol::constants::ProductIdentity;

    let draw = mode == Some("draw");
    let refusal = if draw
        && !cfg!(all(
            windows,
            any(debug_assertions, feature = "gate-draw-replay")
        )) {
        Some("drawing replay is not compiled in")
    } else if (draw
        || cfg!(all(
            windows,
            feature = "gate-draw-replay",
            not(debug_assertions)
        )))
        && identity != ProductIdentity::GATE
    {
        Some("drawing replay requires Gate identity")
    } else if !draw
        && cfg!(all(
            windows,
            feature = "gate-draw-replay",
            not(debug_assertions)
        ))
    {
        Some("release drawing replay requires draw mode")
    } else {
        None
    };
    if let Some(refusal) = refusal {
        return Err(BrainError::Fixture(io::Error::other(refusal)));
    }
    Ok(if draw {
        StartMode::Draw
    } else {
        StartMode::Normal
    })
}

/// Gives screen holds the brain's one screen worker. One that can't start leaves screen holds
/// `Unsupported` for the session, the way a voice part that can't start is unavailable.
fn start_screen(brain: &mut Brain, exe: PathBuf, jobs: io::Result<std::sync::Arc<Job>>) {
    match jobs.and_then(|jobs| screen::HoldClient::start(exe, jobs)) {
        Ok(client) => brain.attach_screen(std::sync::Arc::new(client)),
        Err(error) => tracing::warn!(kind = ?error.kind(), "the screen worker can't start"),
    }
}

/// Takes the profile's writer lock beside the store at `path`, then opens the store, so a brain
/// refused the lock never opens or creates it.
fn open_profile(
    path: &std::path::Path,
) -> Result<(std::sync::Arc<platform::SessionLock>, store::Store), BrainError> {
    let profile = profile_lock(&path.with_file_name(kumochi_protocol::constants::SESSION_LOCK))?;
    open_held(profile, path)
}

/// Opens the store at `path` under `profile`, which is already held, and creates the authority
/// `profile` names when it's missing.
fn open_held(
    profile: std::sync::Arc<platform::SessionLock>,
    path: &std::path::Path,
) -> Result<(std::sync::Arc<platform::SessionLock>, store::Store), BrainError> {
    use connections::authority::{Authority, Missing};

    let authority = profile.authority_path();
    // Under the writer lock, so no cooperating writer creates or removes either file meanwhile.
    let present = |path: &std::path::Path| match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BrainError::Store(store::StoreError::Folder(error))),
    };
    let has_authority = present(authority)?;
    // A missing store is created unless a purge's old copy shows it's mid-swap (`Retired`).
    let store = if present(path)? {
        store::Store::open_existing(path)?
    } else {
        store::Store::open(path)?
    };
    // Builds before the authority left history without one, so a missing authority starts fresh
    // and signed out; it shares no ids with the store.
    if !has_authority {
        Authority::open(&profile, Missing::Create).map_err(|_| BrainError::Authority)?;
    }
    Ok((profile, store))
}

/// Takes the profile's writer lock at `path` without waiting. Only contention is
/// [`BrainError::ProfileBusy`].
fn profile_lock(
    path: &std::path::Path,
) -> Result<std::sync::Arc<platform::SessionLock>, BrainError> {
    platform::SessionLock::try_acquire(path)
        .map(std::sync::Arc::new)
        .map_err(|error| match error {
            platform::SessionLockError::Busy => BrainError::ProfileBusy,
            platform::SessionLockError::Io(error) => BrainError::ProfileLock(error),
        })
}

/// The account system on the real credential store, browser, clock and network, and the fact that
/// its worker holds the session lock.
fn accounts(
    oauth: config::tuning::OAuthTuning,
    tokens: config::tuning::TokenTuning,
    profile: std::sync::Arc<platform::SessionLock>,
) -> Result<
    (
        runtime::account::Accounts,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ),
    BrainError,
> {
    use std::sync::Arc;

    use connections::account::Worker;
    use connections::clock::SystemClock;
    use connections::http_client::{self, SharedHttp};
    use connections::oauth::SystemBrowser;

    let worker = Worker::system(Arc::new(SystemClock), profile);
    let net = runtime::account::Net {
        http: Arc::new(|| http_client::build().map(SharedHttp::new)),
        launcher: Arc::new(SystemBrowser),
        clock: Arc::new(SystemClock),
    };
    let held = worker.held();
    let accounts = runtime::account::Accounts::spawn(worker, net, oauth, tokens)
        .map_err(BrainError::Accounts)?;
    Ok((accounts, held))
}

#[cfg(all(
    windows,
    any(
        all(test, debug_assertions),
        all(feature = "gate-draw-replay", not(debug_assertions))
    )
))]
fn require_draw_model(config: &config::Config) -> Result<(), BrainError> {
    if !config
        .default_choice()
        .display()
        .ok()
        .flatten()
        .is_some_and(|choice| {
            choice.connection == kumochi_protocol::message::ConnectionKind::ChatGpt
                && choice.model.as_str() == engines::responses::replay::MODEL
                && choice.effort == kumochi_protocol::message::Effort::Low
        })
    {
        return Err(BrainError::Fixture(io::Error::other(
            "release drawing replay needs its fixed model",
        )));
    }
    Ok(())
}

#[cfg(all(test, windows, debug_assertions))]
#[test]
fn draw_opt_in_refuses_a_different_model() {
    use kumochi_protocol::{constants::CONFIG_FILE, message::ThreadId};

    let scratch = crate::test_support::Scratch::new("draw-fixed-model");
    std::fs::create_dir(scratch.dir()).unwrap();
    for (model, effort, connection, accepted) in [
        ("draw-replay", "Low", "ChatGpt", true),
        ("other-model", "Low", "ChatGpt", false),
        ("draw-replay", "High", "ChatGpt", false),
        ("draw-replay", "Low", "AnthropicKey", false),
    ] {
        std::fs::write(
            scratch.dir().join(CONFIG_FILE),
            format!(r#"default_model = {{ model = "{model}", effort = "{effort}", connection = "{connection}" }}"#),
        ).unwrap();
        let config = config::Config::load(Some(scratch.dir().to_owned()), ThreadId(1));
        assert_eq!(require_draw_model(&config).is_ok(), accepted);
    }
}

#[cfg(all(windows, any(debug_assertions, feature = "gate-draw-replay")))]
fn draw_engine() -> Result<Selected, BrainError> {
    let observation = Observation::drawing().ok_or_else(|| {
        BrainError::Fixture(io::Error::other(
            "drawing replay needs a fresh gate receipt",
        ))
    })?;
    let replay =
        engines::responses::replay::Replay::new(observation).map_err(BrainError::Fixture)?;
    Ok(Selected::DrawReplay(Box::new(replay)))
}

#[test]
fn a_profile_another_brain_holds_is_busy_and_any_other_failure_is_not() {
    let scratch = test_support::Scratch::new("profile-lock");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let path = scratch
        .dir()
        .join(kumochi_protocol::constants::SESSION_LOCK);
    let first = profile_lock(&path).expect("the first brain takes the profile");
    assert!(matches!(profile_lock(&path), Err(BrainError::ProfileBusy)));
    drop(first);
    drop(profile_lock(&path).expect("released when the first brain ends"));
    assert!(matches!(
        profile_lock(scratch.dir()),
        Err(BrainError::ProfileLock(_))
    ));
}

#[test]
fn a_brain_refused_the_profile_never_opens_the_store() {
    let scratch = test_support::Scratch::new("profile-store");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let held = profile_lock(
        &scratch
            .db()
            .with_file_name(kumochi_protocol::constants::SESSION_LOCK),
    )
    .expect("another brain holds the profile");
    assert!(matches!(
        open_profile(&scratch.db()),
        Err(BrainError::ProfileBusy)
    ));
    assert!(!scratch.db().exists(), "a refused brain created the store");
    drop(held);
    let (_profile, _store) = open_profile(&scratch.db()).expect("opens once the profile is free");
    assert!(scratch.db().exists());
}

#[cfg(test)]
mod profile_files {
    use std::path::Path;

    use kumochi_protocol::constants::AUTHORITY_FILE;
    use rusqlite::Connection;

    use super::{BrainError, open_profile, store};
    use crate::test_support::{Scratch, files};

    fn version(db: &Path) -> i64 {
        Connection::open(db)
            .expect("raw open")
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("the version")
    }

    /// The generations a fresh authority would hold none of: every account, host and key.
    fn generations(authority: &Path) -> i64 {
        Connection::open(authority)
            .expect("raw open")
            .query_row("SELECT count(*) FROM generations", [], |row| row.get(0))
            .expect("the generations")
    }

    #[test]
    fn a_first_start_creates_both_databases() {
        let scratch = Scratch::new("profile-first-start");
        std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
        let authority = scratch.db().with_file_name(AUTHORITY_FILE);
        drop(open_profile(&scratch.db()).expect("a first start"));
        assert_eq!(version(&scratch.db()), 13);
        assert_eq!(version(&authority), 3);
        drop(open_profile(&scratch.db()).expect("the next start opens both"));
        assert_eq!(version(&scratch.db()), 13);
    }

    #[test]
    fn history_from_before_the_authority_gets_a_fresh_signed_out_one() {
        let scratch = Scratch::new("profile-no-authority");
        let built = store::tests::store_at(scratch.dir(), 11);
        Connection::open(&built)
            .expect("raw open")
            .execute_batch(
                "INSERT INTO threads (title, created_ms, updated_ms) VALUES ('kept', 1, 1);",
            )
            .expect("a thread an older build issued");
        std::fs::rename(&built, scratch.db()).expect("the store's name");
        let authority = scratch.db().with_file_name(AUTHORITY_FILE);
        drop(open_profile(&scratch.db()).expect("the start goes on"));
        assert_eq!(version(&scratch.db()), 13);
        assert_eq!(version(&authority), 3);
        assert_eq!(generations(&authority), 0, "signed out");
        let title: String = Connection::open(scratch.db())
            .expect("raw open")
            .query_row("SELECT title FROM threads WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("the thread stays");
        assert_eq!(title, "kept");
    }

    #[test]
    fn an_authority_without_its_store_gets_an_empty_store_and_stays_as_it_was() {
        let scratch = Scratch::new("profile-no-store");
        std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
        drop(open_profile(&scratch.db()).expect("a first start"));
        let authority = scratch.db().with_file_name(AUTHORITY_FILE);
        std::fs::remove_file(scratch.db()).expect("the store goes");
        let before = std::fs::read(&authority).expect("the authority");
        drop(open_profile(&scratch.db()).expect("the start goes on"));
        assert_eq!(version(&scratch.db()), 13);
        assert_eq!(std::fs::read(&authority).expect("the authority"), before);
    }

    #[test]
    fn a_store_mid_purge_swap_is_refused_and_not_recreated() {
        let scratch = Scratch::new("profile-retired");
        std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
        drop(open_profile(&scratch.db()).expect("a first start"));
        std::fs::remove_file(scratch.db()).expect("the store goes");
        let mut old = scratch.db().into_os_string();
        old.push(".purge-old");
        std::fs::write(&old, b"old copy").expect("a purge's old copy");
        let before = files(scratch.dir());
        assert!(matches!(
            open_profile(&scratch.db()),
            Err(BrainError::Store(store::StoreError::Retired))
        ));
        assert_eq!(files(scratch.dir()), before);
    }
}

#[cfg(all(test, windows, debug_assertions))]
#[tokio::test]
async fn direct_dev_draw_refuses_before_store_or_vault_startup() {
    use kumochi_protocol::constants::ProductIdentity;

    let refused = draw_admission(Some("draw"), ProductIdentity::DEV);
    let Err(BrainError::Fixture(error)) = Box::pin(run_admitted(refused, || {
        panic!("refused draw entered store/vault startup");
    }))
    .await
    else {
        panic!("direct Dev draw did not refuse at startup");
    };
    assert_eq!(error.to_string(), "drawing replay requires Gate identity");
    assert!(matches!(
        draw_admission(Some("1"), ProductIdentity::DEV),
        Ok(StartMode::Normal)
    ));
}

#[cfg(all(test, not(windows)))]
#[tokio::test]
async fn native_library_entries_return_unsupported() {
    assert!(matches!(
        Box::pin(run()).await,
        Err(BrainError::NativeRuntime(error)) if error.kind() == io::ErrorKind::Unsupported
    ));
    assert!(matches!(
        bridge::serve("not a launch id"),
        Err(bridge::BridgeError::NativeRuntime(error)) if error.kind() == io::ErrorKind::Unsupported
    ));
    for serve in [screen::serve, speech::serve] {
        assert!(matches!(
            serve(),
            Err(kumochi_protocol::frame::FrameError::Io(error))
                if error.kind() == io::ErrorKind::Unsupported
        ));
    }
}

/// The engine runs use: in a dev build that names a script in
/// [`kumochi_protocol::constants::FIXTURE_ENV`], that script answers every run; otherwise the
/// account's Responses engine answers within `budget`, once the account is ready.
fn engine(tuning: config::tuning::ModelTuning, budget: Budget) -> Result<Selected, BrainError> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(kumochi_protocol::constants::FIXTURE_ENV) {
        let script = std::fs::read_to_string(path).map_err(BrainError::Fixture)?;
        let fixture = Fixture::parse(&script).map_err(|error| {
            BrainError::Fixture(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        tracing::info!("fixture engine active");
        return Ok(Selected::DevFixture(Box::new(fixture)));
    }
    let (access, asks) = AccessHandle::new();
    let client = ModelClient::new(Tls::build().map_err(BrainError::Model)?).with_tuning(tuning);
    let engine = Responses::new(client, budget, access);
    Ok(Selected::Account {
        engine: Box::new(engine),
        asks,
    })
}

/// Serves shells until one quits or is from another build, relaunching a shell that dies. A quit
/// returns its shutdown deadline.
async fn serve(
    pipes: &mut Pipes,
    identity: &Identity,
    brain: &mut Brain,
) -> Result<Option<tokio::time::Instant>, ipc::IpcError> {
    let mut tap = tap::Tap::from_env();
    let mut launched = None;
    // Dropped when serving ends, which cancels a relaunch still waiting out its backoff.
    let mut relaunches = JoinSet::new();
    loop {
        let session = pipes
            .next_session(identity, brain, &mut tap, &mut launched)
            .await?;
        // A shell connected or the relaunched one died; either way nothing waits on it now.
        launched = None;
        match session {
            Session::Quit { deadline } => {
                tracing::info!("quit");
                return Ok(Some(deadline));
            }
            Session::BuildMismatch => {
                tracing::info!("a shell from another build connected; exiting");
                return Ok(None);
            }
            Session::NoShell => {
                tracing::info!("no shell connected; exiting");
                return Ok(None);
            }
            Session::ShellLost => {
                let plan = brain.crashes.plan(std::time::Instant::now());
                if !brain.clear_before_relaunch(&plan.clear).await {
                    tracing::error!("drawing clears were not confirmed; not relaunching the shell");
                    continue;
                }
                let Some(delay) = plan.delay else {
                    tracing::error!("the shell keeps crashing; not relaunching it");
                    continue;
                };
                tracing::warn!(?delay, "the shell died; relaunching it");
                let (exited, exit) = oneshot::channel();
                launched = Some(exit);
                relaunches.spawn(relaunch(identity.shell_image.clone(), delay, exited));
            }
        }
    }
}

/// Relaunches the shell after `delay`, then reports on `exited` when it ends, so a shell that
/// dies before connecting counts as a crash too. A relaunch that fails drops `exited`, which
/// counts the same way, so the backoff retries it and then gives up.
async fn relaunch(shell: PathBuf, delay: Duration, exited: oneshot::Sender<()>) {
    tokio::time::sleep(delay).await;
    let budgets = connections::model::budget::handed_on(|name| std::env::var_os(name).is_some());
    let mut child = match platform::launch_detached(&shell, budgets) {
        Ok(child) => child,
        Err(error) => {
            tracing::error!(%error, "can't relaunch the shell");
            return;
        }
    };
    tracing::info!(pid = child.id(), "relaunched the shell");
    // A plain thread, not the runtime's blocking pool, so a brain that's done never waits
    // for this shell to end.
    let (ended, end) = oneshot::channel();
    let watcher = thread::Builder::new()
        .name("shell-watch".to_owned())
        .spawn(move || match child.wait() {
            Ok(_) => ended.send(()).unwrap_or_default(),
            Err(error) => tracing::warn!(%error, "lost track of the relaunched shell"),
        });
    if let Err(error) = watcher {
        tracing::warn!(%error, "can't watch the relaunched shell");
    }
    if end.await.is_err() {
        // Unwatched, the shell may still be alive: give it time to connect before counting it
        // lost, which at worst starts a second shell that exits on the mutex.
        tokio::time::sleep(UNWATCHED_GRACE).await;
    }
    if exited.send(()).is_err() {
        tracing::debug!("the relaunched shell had connected");
    }
}
