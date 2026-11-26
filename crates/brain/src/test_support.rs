//! Helpers shared by the brain's tests.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kumochi_protocol::constants;

use crate::config::Config;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::vault::Vault;
use crate::connections::vault::fake::FakeStore;
use crate::engines::fixture::Fixture;
#[cfg(windows)]
use crate::fixture_support::held_accounts_until;
pub(crate) use crate::fixture_support::{Scratch, net};
use crate::runtime::account::Accounts;
use crate::runtime::{Brain, IssueEpochs};
use crate::store::Store;

pub(crate) const SCRATCH_PUBLICATION_UNCERTAIN: i32 = 78;

/// Keep the parent's scratch guard from deleting a child's uncertain publication.
pub(crate) fn propagate_scratch_publication(status: std::process::ExitStatus) {
    if status.code() == Some(SCRATCH_PUBLICATION_UNCERTAIN) {
        std::process::exit(SCRATCH_PUBLICATION_UNCERTAIN);
    }
}

/// A writer lock of its own beside `store`, for a brain or store thread under test. Each name is
/// new, so it never meets a profile lock the test takes or a store thread still ending.
pub(crate) fn custody(store: &Store) -> Arc<crate::platform::SessionLock> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("custody-{}.lock", NEXT.fetch_add(1, Ordering::Relaxed));
    Arc::new(
        crate::platform::SessionLock::try_acquire(&store.path().with_file_name(name))
            .expect("custody lock"),
    )
}

pub(crate) fn has_picture_marker(bytes: &[u8], jpeg: &[u8]) -> bool {
    let marker = &jpeg[3..jpeg.len() - 2];
    let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|part| part == needle);
    if contains(marker) || contains(format!("{marker:?}").trim_matches(['[', ']']).as_bytes()) {
        return true;
    }
    (0..3).any(|alignment| {
        let mut padded = vec![0; alignment];
        padded.extend_from_slice(marker);
        let encoded = STANDARD.encode(padded);
        let start = if alignment == 0 { 0 } else { 4 };
        let end = (alignment + marker.len()) / 3 * 4;
        contains(&encoded.as_bytes()[start..end])
    })
}

pub(crate) fn fixture_has_picture_marker(path: &Path, jpeg: &[u8]) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if crate::platform::test_is_link(&meta) => false,
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path)
            .expect("owned test directory")
            .any(|entry| fixture_has_picture_marker(&entry.expect("fixture entry").path(), jpeg)),
        Ok(meta) if meta.is_file() => {
            has_picture_marker(&std::fs::read(path).expect("owned test file"), jpeg)
        }
        Ok(_) => false,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("fixture cannot be inspected: {error}"),
    }
}

/// A brain on a fresh store in `scratch`, playing `script`.
pub(crate) fn brain(scratch: &Scratch, script: &str) -> Brain {
    brain_with(scratch, script, accounts(scratch))
}

pub(crate) fn assert_mcp_draft_schema(description: &serde_json::Value, schema: &serde_json::Value) {
    use serde_json::json;

    assert_eq!(
        description,
        "Save a local issue draft for user review when asked to file, open or report a GitHub issue. Supply only a nonblank title on one line (at most 256 Unicode characters) and a literal Markdown body. Keep all serialized arguments within 16 KiB, including JSON escaping. Do not include hidden format characters. This tool cannot select a repository, file an issue, authorize filing or contact GitHub."
    );
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let mut required: Vec<_> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field.as_str().unwrap())
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["body", "title"]);
    assert_eq!(
        schema["properties"],
        json!({"title":{"type":"string"}, "body":{"type":"string"}})
    );
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
    brain_counting(scratch, engine, accounts, IssueEpochs::default())
}

/// [`brain_on`], taking over issue-session `epochs` something already reserved from.
pub(crate) fn brain_counting(
    scratch: &Scratch,
    engine: crate::runtime::Selected,
    accounts: Accounts,
    epochs: IssueEpochs,
) -> Brain {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    let main_thread = store.main_thread().expect("main thread");
    let high_water = store.high_water().expect("high water");
    let scenes = store.scenes().expect("scenes");
    let config = Config::load(Some(scratch.dir().to_owned()), main_thread);
    let custody = crate::test_support::custody(&store);
    Brain::new(
        store,
        custody,
        main_thread,
        high_water,
        scenes,
        engine,
        config,
        accounts,
        epochs,
    )
    .expect("brain")
}

/// An account system on an in-memory credential store, with its session lock in `scratch`.
pub(crate) fn accounts(scratch: &Scratch) -> Accounts {
    let store = FakeStore::default();
    let lock = scratch.dir().join(constants::SESSION_LOCK);
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "test-accounts", &lock)),
        Arc::new(FixedClock(1_800_000_000)),
    );
    Accounts::spawn_unconfigured(worker, net()).expect("credential thread")
}

/// [`accounts`] under a profile lock the caller already holds, as the brain's credential thread
/// opens its vault.
#[cfg(windows)]
pub(crate) fn held_accounts(profile: Arc<crate::platform::SessionLock>) -> Accounts {
    held_accounts_until(profile, ())
}

/// An account system whose vault opens with `open`, which fails or panics; its credential
/// thread drops `hold` after it stops and before it reports the release.
pub(crate) fn failing_accounts(
    hold: impl Send + 'static,
    open: fn() -> Result<Vault<FakeStore>, crate::connections::error::VaultError>,
) -> std::io::Result<Accounts> {
    let worker = Worker::<FakeStore>::new(
        Box::new(move || {
            let _hold = &hold;
            open()
        }),
        Arc::new(FixedClock(1_800_000_000)),
    );
    Accounts::spawn_unconfigured(worker, net())
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

/// One folder entry as a test witness sees it, without following a link.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// A file's bytes, or a link's target.
    pub(crate) bytes: Vec<u8>,
    permissions: std::fs::Permissions,
    owner: Option<(u32, u32)>,
}

/// Every entry in `dir` by name: its bytes or link target, permissions and owner.
pub(crate) fn files(dir: &Path) -> std::collections::BTreeMap<std::ffi::OsString, Entry> {
    std::fs::read_dir(dir)
        .expect("scratch listing")
        .map(|entry| {
            let path = entry.expect("scratch entry").path();
            let metadata = std::fs::symlink_metadata(&path).expect("scratch metadata");
            let bytes = if metadata.is_symlink() {
                let target = std::fs::read_link(&path).expect("scratch link");
                target.into_os_string().into_encoded_bytes()
            } else {
                std::fs::read(&path).expect("scratch file")
            };
            #[cfg(unix)]
            let owner = {
                use std::os::unix::fs::MetadataExt;
                Some((metadata.uid(), metadata.gid()))
            };
            #[cfg(not(unix))]
            let owner = None;
            let name = path.file_name().expect("scratch name").to_owned();
            let permissions = metadata.permissions();
            (
                name,
                Entry {
                    bytes,
                    permissions,
                    owner,
                },
            )
        })
        .collect()
}
