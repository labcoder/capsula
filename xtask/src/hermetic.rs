//! What keeps a gate tool off the developer's own Kumochi (OSC-680). A gate tool runs only as the
//! gate identity, launches only images it built with that identity right before, gives its brains
//! folders of their own with Claude Code off, runs alone, and at the end deletes what its brains
//! left. See `docs/development/gate-identity.md`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{
    ACCOUNT_MAX_CHUNKS, CONFIG_FILE, ProductIdentity, generation_hex, generation_target,
    instance_target,
};
use rusqlite::{Connection, OpenFlags};

use crate::{live, platform};

/// How a gate tool is started; the feature is named nowhere else.
const COMMAND: &str =
    "cargo run --release --package xtask --features kumochi-protocol/gate-identity -- <task>";
const GATE: ProductIdentity = ProductIdentity::GATE;
/// The brain's credential authority in its data folder (the brain's `authority::FILE`).
const AUTHORITY: &str = "credentials.db";
/// Claude Code off, so the brain starts no `claude.exe` and reads no policy for it.
const SETTINGS: &str = "[claude_code]\nenabled = false\n";
/// How long building the images may take [guess: a cold release shell build on Rez].
const BUILD_LIMIT: Duration = Duration::from_mins(40);

/// Fails unless `active` is the gate identity.
pub(crate) fn require_gate(active: ProductIdentity) -> Result<()> {
    ensure!(
        active == GATE,
        "a gate tool runs only as the gate identity: {COMMAND}"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Debug,
    Release,
}

/// Builds `packages` with the gate identity into `gate` beside this xtask's profile folders, from
/// `checkout`'s workspace or this one, and returns the profile folder holding them. Cargo rebuilds
/// anything built there without the feature, so what this returns is always a gate image. Also
/// returns how long the build took, so a timed run that started right after a full build shows.
pub(crate) fn build(
    packages: &[&str],
    profile: Profile,
    checkout: Option<&Path>,
) -> Result<(PathBuf, Duration)> {
    let started = Instant::now();
    let target = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .context("xtask is not in a Cargo profile folder")?
        .join(if checkout.is_some() {
            "gate-baseline"
        } else {
            "gate"
        });
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo.args([
        "build",
        "--quiet",
        "--locked",
        "--features",
        "kumochi-protocol/gate-identity",
    ]);
    for package in packages {
        cargo.args(["--package", package]);
    }
    cargo.arg("--target-dir").arg(&target);
    if profile == Profile::Release {
        cargo.arg("--release");
    }
    if let Some(checkout) = checkout {
        cargo
            .arg("--manifest-path")
            .arg(checkout.join("Cargo.toml"));
    }
    let built = live::run_bounded(cargo, BUILD_LIMIT, "the gate build")?;
    ensure!(built.success(), "the gate build failed ({built})");
    let profile = match profile {
        Profile::Debug => "debug",
        Profile::Release => "release",
    };
    Ok((target.join(profile), started.elapsed()))
}

/// One launch's own `%LOCALAPPDATA%` and `%APPDATA%`, with Claude Code off in its settings.
#[derive(Debug)]
pub(crate) struct Folders {
    pub(crate) local: PathBuf,
    roaming: PathBuf,
}

impl Folders {
    pub(crate) fn create(dir: &Path) -> Result<Self> {
        let local = dir.join("local");
        let roaming = dir.join("roaming");
        let settings = roaming.join(GATE.data_dir);
        for folder in [&local, &settings] {
            fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
        }
        fs::write(settings.join(CONFIG_FILE), SETTINGS).context("writing the gate settings")?;
        Ok(Self { local, roaming })
    }

    /// Points `command`'s data and settings folders here.
    pub(crate) fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("LOCALAPPDATA", &self.local)
            .env("APPDATA", &self.roaming)
    }
}

/// One gate run, alone, with its launches' folders under a root it created.
#[derive(Debug)]
pub(crate) struct Run {
    root: PathBuf,
    /// Held until the run ends, so another run fails to take it; it names this run's root until
    /// the run has cleaned up, so the next run can clean up after an interrupted one.
    alone: File,
}

impl Run {
    /// Refuses unless this is the gate identity, no other gate run is on and no product process
    /// runs; puts this process in a kill-on-close job, so nothing it starts outlives it or breaks
    /// away from it; cleans up after an interrupted run; and creates `root`, which must be new.
    pub(crate) fn begin(root: PathBuf) -> Result<Self> {
        require_gate(ProductIdentity::ACTIVE)?;
        let mut alone =
            take(&std::env::temp_dir().join(format!("{}-run.lock", GATE.object_prefix)))?;
        live::refuse_running(&platform::processes()?, std::process::id())?;
        platform::Job::kill_on_close()
            .and_then(platform::Job::hold_self)
            .context("putting the run in a job")?;
        claim(&mut alone, &root, &mut CredentialManager)?;
        Ok(Self { root, alone })
    }

    pub(crate) fn folders(&self, name: &str) -> Result<Folders> {
        Folders::create(&self.root.join(name))
    }

    /// Ends the run with `result`. Once no product process is left: deletes what each launch's
    /// own authority names in Credential Manager, then the gate's instance entry for this session
    /// and the gate's folder in the real `LocalAppData`. After a success it also deletes the
    /// run's folders; after a failure it keeps them, with the product logs, until the next run.
    pub(crate) fn finish(self, result: Result<()>) -> Result<()> {
        let kept = format!(
            "the run's folders stay in {} until the next gate run deletes them",
            self.root.display()
        );
        if let Err(running) = live::refuse_running(&platform::processes()?, std::process::id()) {
            return crate::session::combine([result, Err(running.context(kept))]);
        }
        let keep = if result.is_ok() {
            Keep::Nothing
        } else {
            Keep::Folders
        };
        let mut store = CredentialManager;
        let entries = remove_entries(&self.root, &mut store, keep);
        let instance = platform::gate_pipe::session_id()
            .map_err(anyhow::Error::from)
            .and_then(|session| store.delete(&instance_target(GATE.instance_service, session)));
        let known = platform::local_app_data().and_then(|folder| {
            let folder = folder.join(GATE.data_dir);
            if folder.try_exists()? {
                fs::remove_dir_all(&folder)
                    .with_context(|| format!("deleting {}", folder.display()))?;
            }
            Ok(())
        });
        // The lock names the root until its folders are gone, so the next run deletes them.
        let released = match (keep, &entries) {
            (Keep::Nothing, Ok(())) => self.alone.set_len(0).map_err(anyhow::Error::from),
            (Keep::Folders, _) => Err(anyhow!("{kept}")),
            (Keep::Nothing, Err(_)) => Ok(()),
        };
        crate::session::combine([result, entries, instance, known, released])
    }
}

/// Takes the gate-run lock at `path`, or refuses while another run holds it.
fn take(path: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => bail!("another gate run is on; one runs at a time"),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Cleans up the earlier run the held `lock` names, if it didn't finish, then creates `root`,
/// which must be new, so that every folder under it is this run's, and names it in the lock.
fn claim(lock: &mut File, root: &Path, store: &mut impl Credentials) -> Result<()> {
    ensure!(
        !root.try_exists()?,
        "{} already exists; a gate run takes a new folder",
        root.display()
    );
    let named = root.to_str().context("the run's folder isn't Unicode")?;
    let mut earlier = String::new();
    lock.rewind()?;
    lock.read_to_string(&mut earlier)?;
    let earlier = Path::new(&earlier);
    if !earlier.as_os_str().is_empty() && earlier.try_exists()? {
        remove_entries(earlier, store, Keep::Nothing).with_context(|| {
            format!(
                "cleaning up after the interrupted gate run in {}",
                earlier.display()
            )
        })?;
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(root)?;
    lock.set_len(0)?;
    lock.rewind()?;
    lock.write_all(named.as_bytes())?;
    Ok(())
}

/// What the cleanup keeps after deleting the entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Nothing,
    /// The launch folders, with their product logs, after a failed run.
    Folders,
}

/// Credential Manager as the cleanup sees it, so tests use a fake, never the real store.
trait Credentials {
    /// Deletes `target`; one that doesn't exist counts as deleted.
    fn delete(&mut self, target: &str) -> Result<()>;
}

struct CredentialManager;

impl Credentials for CredentialManager {
    fn delete(&mut self, target: &str) -> Result<()> {
        let deleted = keyring::Entry::new_with_target(target, "", "")
            .and_then(|entry| entry.delete_credential());
        match deleted {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!("couldn't delete {target} from Credential Manager"),
        }
    }
}

/// For each launch folder under `root`: deletes the gate names of every generation its own
/// authority records, then, unless `keep` says otherwise, the folder; then `root`. A folder whose
/// authority can't be read keeps its folder and its entries, since nothing else can say which
/// entries are its own.
fn remove_entries(root: &Path, store: &mut impl Credentials, keep: Keep) -> Result<()> {
    let mut kept = Vec::new();
    for launch in fs::read_dir(root)? {
        let launch = launch?.path();
        let authority = launch.join("local").join(GATE.data_dir).join(AUTHORITY);
        match own_entries(&authority) {
            Ok(targets) => {
                for target in &targets {
                    store.delete(target)?;
                }
                if keep == Keep::Nothing {
                    fs::remove_dir_all(&launch)
                        .with_context(|| format!("deleting {}", launch.display()))?;
                }
            }
            Err(_) => kept.push(launch.display().to_string()),
        }
    }
    ensure!(
        kept.is_empty(),
        "kept for a manual cleanup, since their credential authority couldn't be read: {}",
        kept.join("; ")
    );
    if keep == Keep::Folders {
        return Ok(());
    }
    fs::remove_dir(root).with_context(|| format!("deleting {}", root.display()))
}

/// The gate names of every chunk of every generation `authority` records; none when it doesn't
/// exist, since then no brain stored anything.
fn own_entries(authority: &Path) -> Result<Vec<String>> {
    if !authority.try_exists()? {
        return Ok(Vec::new());
    }
    let db = Connection::open_with_flags(authority, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let mut query = db.prepare("SELECT generation, kind FROM generations")?;
    let rows = query.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut targets = Vec::new();
    for row in rows {
        let (generation, kind) = row?;
        let generation: [u8; 16] = generation
            .try_into()
            .map_err(|_| anyhow!("a generation id isn't 16 bytes"))?;
        let hex = generation_hex(&generation);
        targets.extend(
            (0..ACCOUNT_MAX_CHUNKS)
                .map(|index| generation_target(GATE.credential_service, &kind, &hex, index)),
        );
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::scratch;

    struct Fake(BTreeSet<String>);

    impl Credentials for Fake {
        fn delete(&mut self, target: &str) -> Result<()> {
            self.0.remove(target);
            Ok(())
        }
    }

    const REFUSAL: &str = "a gate tool runs only as the gate identity: cargo run --release \
                           --package xtask --features kumochi-protocol/gate-identity -- <task>";

    #[test]
    fn gate_tools_refuse_the_dev_and_release_identities() {
        for identity in [ProductIdentity::DEV, ProductIdentity::RELEASE] {
            let error = require_gate(identity).expect_err("refused");
            assert_eq!(format!("{error:#}"), REFUSAL);
        }
        require_gate(ProductIdentity::GATE).expect("the gate identity runs");
        let root = scratch::dir("gate-refused").join("run");
        let error = Run::begin(root.clone()).expect_err("a test build is never the gate identity");
        assert_eq!(format!("{error:#}"), REFUSAL);
        assert!(!root.exists());
    }

    #[test]
    fn launch_folders_are_their_own_with_claude_code_off() {
        let dir = scratch::dir("gate-folders");
        let folders = Folders::create(&dir).expect("folders");
        assert_eq!(
            fs::read_to_string(dir.join("roaming/Kumochi Gate/config.toml")).expect("settings"),
            "[claude_code]\nenabled = false\n"
        );
        assert!(dir.join("local").is_dir());
        let mut command = Command::new("unused");
        folders.apply(&mut command);
        let mut envs: Vec<_> = command.get_envs().collect();
        envs.sort();
        let (local, roaming) = (dir.join("local"), dir.join("roaming"));
        assert_eq!(
            envs,
            [
                ("APPDATA".as_ref(), Some(roaming.as_os_str())),
                ("LOCALAPPDATA".as_ref(), Some(local.as_os_str())),
            ]
        );
    }

    fn authority(launch: &Path, generations: &[[u8; 16]]) {
        let data = launch.join("local/Kumochi Gate");
        fs::create_dir_all(&data).expect("data folder");
        let db = Connection::open(data.join(AUTHORITY)).expect("authority");
        db.execute_batch("CREATE TABLE generations (generation BLOB NOT NULL, kind TEXT NOT NULL)")
            .expect("table");
        for generation in generations {
            db.execute(
                "INSERT INTO generations VALUES (?1, 'host')",
                [generation.as_slice()],
            )
            .expect("row");
        }
    }

    #[test]
    fn cleanup_deletes_only_what_the_runs_own_authority_names() {
        let root = scratch::dir("gate-cleanup");
        authority(&root.join("app"), &[[0x07; 16]]);
        Folders::create(&root.join("idle")).expect("a launch that stored nothing");
        let ours = "Kumochi Gate/gen/host/07070707070707070707070707070707";
        let others = [
            "Kumochi Gate/gen/host/09090909090909090909090909090909/0",
            "Kumochi Dev/gen/host/07070707070707070707070707070707/0",
            "Kumochi/gen/host/07070707070707070707070707070707/0",
            "Kumochi Gate/gen/host/07070707070707070707070707070707/64",
        ];
        let mut store = Fake(
            [format!("{ours}/0"), format!("{ours}/63")]
                .into_iter()
                .chain(others.map(str::to_owned))
                .collect(),
        );
        remove_entries(&root, &mut store, Keep::Nothing).expect("cleaned up");
        assert_eq!(store.0, others.map(str::to_owned).into());
        assert!(!root.exists());
    }

    #[test]
    fn a_failed_run_deletes_its_entries_but_keeps_its_folders() {
        let root = scratch::dir("gate-failed");
        authority(&root.join("app"), &[[0x07; 16]]);
        let mut store =
            Fake(["Kumochi Gate/gen/host/07070707070707070707070707070707/0".to_owned()].into());
        remove_entries(&root, &mut store, Keep::Folders).expect("entries deleted");
        assert_eq!(store.0, BTreeSet::new());
        assert!(
            root.join("app/local/Kumochi Gate")
                .join(AUTHORITY)
                .is_file()
        );
    }

    #[test]
    fn a_second_gate_run_is_refused_while_the_first_holds_the_lock() {
        let lock = scratch::dir("gate-lock").join("run.lock");
        fs::write(&lock, "C:/the/first/run").expect("the first run's folder");
        let first = take(&lock).expect("the first run takes it");
        let error = take(&lock).expect_err("the second is refused");
        assert_eq!(
            format!("{error:#}"),
            "another gate run is on; one runs at a time"
        );
        drop(first);
        assert_eq!(
            fs::read_to_string(&lock).expect("lock"),
            "C:/the/first/run"
        );
        take(&lock).expect("free again once the first ends");
    }

    #[test]
    fn a_run_cleans_up_after_an_interrupted_one_and_takes_only_a_new_folder() {
        let dir = scratch::dir("gate-claim");
        let earlier = dir.join("earlier");
        authority(&earlier.join("app"), &[[0x07; 16]]);
        let ours = "Kumochi Gate/gen/host/07070707070707070707070707070707/0";
        let mut store = Fake([ours.to_owned()].into());
        let path = dir.join("run.lock");
        fs::write(&path, earlier.to_str().expect("Unicode")).expect("an interrupted run's lock");
        let mut lock = take(&path).expect("lock");
        let root = dir.join("now");
        claim(&mut lock, &root, &mut store).expect("claimed");
        assert_eq!(store.0, BTreeSet::new());
        assert!(!earlier.exists());
        assert!(root.is_dir());
        drop(lock);
        assert_eq!(
            fs::read_to_string(&path).expect("lock"),
            root.to_str().expect("Unicode")
        );
        let existing = dir.join("existing");
        fs::create_dir(&existing).expect("a folder another run made");
        fs::write(&path, "").expect("a finished run's lock");
        let mut lock = take(&path).expect("lock");
        let error = claim(&mut lock, &existing, &mut store).expect_err("not new");
        drop(lock);
        assert_eq!(
            format!("{error:#}"),
            format!(
                "{} already exists; a gate run takes a new folder",
                existing.display()
            )
        );
        assert_eq!(fs::read_to_string(&path).expect("lock"), "");
    }

    #[test]
    fn cleanup_keeps_a_launch_whose_authority_it_cannot_read() {
        let root = scratch::dir("gate-unreadable");
        let data = root.join("app/local/Kumochi Gate");
        fs::create_dir_all(&data).expect("data folder");
        fs::write(
            data.join(AUTHORITY),
            b"not a database, but long enough to be read as one",
        )
        .expect("authority");
        let held = "Kumochi Gate/gen/host/07070707070707070707070707070707/0";
        let mut store = Fake([held.to_owned()].into());
        let error = remove_entries(&root, &mut store, Keep::Nothing).expect_err("kept");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "kept for a manual cleanup, since their credential authority couldn't be read: {}",
                root.join("app").display()
            )
        );
        assert_eq!(store.0, [held.to_owned()].into());
        assert!(data.join(AUTHORITY).is_file());
    }
}
