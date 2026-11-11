//! Helpers shared by the brain's tests.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::constants;
use kumochi_protocol::message::ThreadId;

use crate::config::Config;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::HttpError;
use crate::connections::oauth::{LaunchError, LaunchTarget, Launcher};
use crate::connections::vault::Vault;
use crate::connections::vault::fake::FakeStore;
use crate::engines::fixture::Fixture;
use crate::runtime::Brain;
use crate::runtime::account::{Accounts, Net};
use crate::store::Store;

/// A store file in its own temp folder, removed afterwards.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn trusted(name: &str) -> Self {
        Self::under(
            &crate::platform::local_app_data().expect("local application data"),
            name,
        )
    }

    pub(crate) fn new(name: &str) -> Self {
        Self::under(&std::env::temp_dir(), name)
    }

    fn under(base: &Path, name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "{}-store-{}-{name}-{n}",
            constants::OBJECT_PREFIX,
            std::process::id()
        ));
        // A folder left by an earlier aborted run would make the test start from old data.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("old scratch folder removable");
        }
        Self(dir)
    }

    /// The folder, which nothing creates until a test does.
    pub(crate) fn dir(&self) -> &Path {
        &self.0
    }

    pub(crate) fn db(&self) -> PathBuf {
        self.0.join("store.db")
    }

    /// Settings kept in this folder, so a test never touches the user's, naming the main thread
    /// a fresh store creates.
    pub(crate) fn config(&self) -> Config {
        Config::load(Some(self.0.clone()), ThreadId(1))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A dropped brain's store thread closes SQLite on its own, so the files can stay open
        // for a moment after the test ends.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == ErrorKind::NotFound => return,
                Err(error) if Instant::now() >= deadline => {
                    eprintln!("scratch folder left behind: {error}");
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

/// A brain on a fresh store in `scratch`, playing `script`.
pub(crate) fn brain(scratch: &Scratch, script: &str) -> Brain {
    brain_with(scratch, script, accounts(scratch))
}

/// A brain on a fresh store in `scratch`, playing `script`, on `accounts`.
pub(crate) fn brain_with(scratch: &Scratch, script: &str, accounts: Accounts) -> Brain {
    let fixture = Fixture::parse(script).expect("script");
    brain_on(
        scratch,
        crate::runtime::Selected::Fixture(Box::new(fixture)),
        accounts,
    )
}

/// A brain on a fresh store in `scratch`, answering with `engine`, on `accounts`.
pub(crate) fn brain_on(
    scratch: &Scratch,
    engine: crate::runtime::Selected,
    accounts: Accounts,
) -> Brain {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main_thread = store.main_thread().expect("main thread");
    let high_water = store.high_water().expect("high water");
    let scenes = store.scenes().expect("scenes");
    let config = Config::load(Some(scratch.0.clone()), main_thread);
    Brain::new(
        store,
        main_thread,
        high_water,
        scenes,
        engine,
        config,
        accounts,
    )
    .expect("brain")
}

/// An account system on an in-memory credential store, with its session lock in `scratch`.
pub(crate) fn accounts(scratch: &Scratch) -> Accounts {
    let store = FakeStore::default();
    let lock = scratch.0.join(constants::SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "test-accounts", &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    Accounts::spawn_unconfigured(worker, net()).expect("credential thread")
}

/// What an account system reaches outside the brain through, in tests: it never opens a
/// browser or reaches the network, since launches and requests are refused.
pub(crate) fn net() -> Net {
    struct NoBrowser;
    impl Launcher for NoBrowser {
        fn launch(&self, _: &LaunchTarget) -> Result<(), LaunchError> {
            Err(LaunchError::Launch)
        }
    }
    Net {
        http: Arc::new(|| Err(HttpError::Configuration)),
        launcher: Arc::new(NoBrowser),
        clock: Arc::new(FixedClock(1_800_000_000)),
    }
}

/// Collects what `tracing` writes while it's the default subscriber.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub(crate) fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Sets a subscriber writing here as this thread's default until the guard drops.
    pub(crate) fn install(&self) -> Capturing {
        let captured = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || captured.clone())
            .finish();
        // With one dispatcher registered, tracing asks only the current thread's subscriber
        // about a new callsite, so one first hit on another test's thread would be cached as
        // never. A second keeps it asking every live dispatcher, this one included.
        let second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        Capturing {
            _default: tracing::subscriber::set_default(subscriber),
            _second: second,
        }
    }
}

/// Capture lasts until this drops.
pub(crate) struct Capturing {
    _default: tracing::subscriber::DefaultGuard,
    _second: tracing::Dispatch,
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A label no log line, tap record or error may carry.
pub(crate) const SENTINEL: &str = "zqxsentinellabel";
