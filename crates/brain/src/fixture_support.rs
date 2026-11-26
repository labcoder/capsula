use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use kumochi_protocol::message::ThreadId;

#[cfg(test)]
use crate::config::Config;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::HttpError;
use crate::connections::oauth::{LaunchError, LaunchTarget, Launcher};
use crate::connections::vault::Vault;
use crate::connections::vault::fake::FakeStore;
use crate::runtime::account::{Accounts, Net};
use crate::scratch;

/// A store file in its own temp folder, removed afterwards.
pub(crate) struct Scratch {
    // Teardown uses the pathname shortcut documented in tests/support/scratch.rs.
    _guard: scratch::Scratch,
    case: PathBuf,
}

impl Scratch {
    /// Fixture root for path-sensitive Windows tests and ordinary scratch elsewhere.
    #[cfg(test)]
    pub(crate) fn trusted(name: &str) -> Self {
        #[cfg(windows)]
        {
            Self::with_guard(scratch::Scratch::under(
                &crate::platform::local_app_data().expect("local application data"),
                name,
            ))
        }
        #[cfg(not(windows))]
        {
            Self::new(name)
        }
    }

    pub(crate) fn new(name: &str) -> Self {
        let guard = scratch::dir(name);
        // The temp folder may let other accounts write, and the product refuses such a folder.
        #[cfg(all(test, windows))]
        crate::platform::set_dacl(guard.path(), |user| format!("D:P(A;OICI;FA;;;{user})"));
        Self::with_guard(guard)
    }

    #[cfg(test)]
    pub(crate) fn under(base: &Path, name: &str) -> Self {
        Self::with_guard(scratch::Scratch::under(base, name))
    }

    fn with_guard(guard: scratch::Scratch) -> Self {
        let case = guard.join("case");
        Self {
            _guard: guard,
            case,
        }
    }

    /// The folder, which nothing creates until a test does.
    pub(crate) fn dir(&self) -> &Path {
        &self.case
    }

    pub(crate) fn db(&self) -> PathBuf {
        self.case.join("store.db")
    }

    /// Settings kept in this folder, so a test never touches the user's, naming the main thread
    /// a fresh store creates.
    #[cfg(test)]
    pub(crate) fn config(&self) -> Config {
        Config::load(Some(self.case.clone()), ThreadId(1))
    }
}

/// [`held_accounts`], whose credential thread drops `hold` after the vault and before it
/// reports the release.
pub(crate) fn held_accounts_until(
    profile: Arc<crate::platform::SessionLock>,
    hold: impl Send + 'static,
) -> Accounts {
    let store = FakeStore::default();
    let worker = Worker::new(
        Box::new(move || {
            let _hold = &hold;
            Vault::with_lock(
                store.clone(),
                "test-accounts",
                Arc::clone(&profile),
                crate::connections::authority::Missing::Create,
            )
        }),
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
