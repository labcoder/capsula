//! What keeps a gate tool off the developer's own Kumochi (OSC-680). A gate tool runs only as the
//! gate identity, uses freshly built or pinned Gate images, and gives its brains folders of their
//! own with Claude Code off. It runs alone and deletes what its brains left at the end. See
//! `docs/development/gate-identity.md`.

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io::{Read as _, Seek as _, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{
    ACCOUNT_MAX_CHUNKS, AUTHORITY_FILE, BRAIN_EXE, CONFIG_FILE, LOCAL_APPDATA_OVERRIDE,
    ProductIdentity, ROAMING_APPDATA_OVERRIDE, SHELL_EXE, anthropic_key_target,
    anthropic_staging_target, generation_hex, generation_target, instance_target,
};
use kumochi_storage::checked::{Kind, Trusted, open_checked};
use rusqlite::{Connection, OpenFlags};
use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

use crate::platform::Share;
use crate::{live, platform};

/// How a gate tool is started; the feature is named nowhere else.
const COMMAND: &str =
    "cargo run --release --package xtask --features kumochi-protocol/gate-identity -- <task>";
const GATE: ProductIdentity = ProductIdentity::GATE;
/// The file in a run's root that holds its run id, so recovery deletes only a gate run's folder.
const MARKER: &str = "gate-run";
/// The lock byte a run holds from start to end, so one runs at a time.
const EXCLUSIVE: u32 = 0;
/// The lock byte a run holds once its own line is written, until it ends.
const PUBLISHED: u32 = 1;
/// The lock's one line: this, the run id, a space, the run's root and a newline.
const LOCK_LINE: &str = "kumochi-gate-run ";
/// Onboarding finished, so no shell opens the onboarding window and takes the foreground; and
/// Claude Code off, so the brain starts no `claude.exe` and reads no policy for it.
const SETTINGS: &str = "onboarded = true\n\n[claude_code]\nenabled = false\n";
/// The same settings with onboarding still owed, for a run that measures the onboarding window.
const OWING_ONBOARDING: &str = "[claude_code]\nenabled = false\n";
/// How long building the images may take [guess: a cold release shell build on Rez].
const BUILD_LIMIT: Duration = Duration::from_mins(40);
/// How long the run's surviving processes get to exit once they're ended.
const END_LIMIT: Duration = Duration::from_secs(10);
/// What the shell and the brain each log at start, so an image names its role.
pub(crate) const SHELL_ROLE: &str = "shell started";
pub(crate) const BRAIN_ROLE: &str = "brain started";

/// Refuses the image `exe` unless it carries the Gate identity, `role` and both folder override
/// names, and not the release identity; a `Release` image mustn't carry the dev identity either,
/// which every debug build links. An image from before the override would follow the launch's
/// `LOCALAPPDATA`, which a gate tool no longer sets, so it never launches.
pub(crate) fn require_compatible(exe: &Path, role: &str, profile: Profile) -> Result<()> {
    let name = exe.file_name().unwrap_or(exe.as_os_str()).display();
    let bytes = fs::read(exe).with_context(|| format!("{name} is unavailable"))?;
    let contains = |marker: &str| {
        bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    };
    // shortcut: existing UTF-8 strings catch mix-ups, not debug Gate or tampering; use attestation if provenance is needed.
    ensure!(
        contains(GATE.instance_service)
            && contains(role)
            && contains(LOCAL_APPDATA_OVERRIDE)
            && contains(ROAMING_APPDATA_OVERRIDE)
            && !contains(ProductIdentity::RELEASE.instance_service)
            && (profile == Profile::Debug || !contains(ProductIdentity::DEV.instance_service)),
        "{name} does not have the expected Gate identity, role and folder override; rebuild it"
    );
    Ok(())
}

/// Fails unless `active` is the gate identity.
pub(crate) fn require_gate(active: ProductIdentity) -> Result<()> {
    ensure!(
        active == GATE,
        "a gate tool runs only as the gate identity: {COMMAND}"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Prebuilt {
    pub(crate) shell_sha256: String,
    pub(crate) brain_sha256: String,
}

impl Prebuilt {
    pub(crate) fn parse(args: &mut impl Iterator<Item = String>) -> Result<Self> {
        let (Some(shell), Some(brain)) = (args.next(), args.next()) else {
            bail!("--prebuilt needs the shell and brain SHA-256 pins");
        };
        ensure!(
            [&shell, &brain]
                .iter()
                .all(|pin| pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit())),
            "--prebuilt pins must be 64 hexadecimal digits"
        );
        Ok(Self {
            shell_sha256: shell.to_ascii_lowercase(),
            brain_sha256: brain.to_ascii_lowercase(),
        })
    }

    pub(crate) fn release(&self) -> Result<PathBuf> {
        require_gate(ProductIdentity::ACTIVE)?;
        ensure!(
            !cfg!(debug_assertions),
            "--prebuilt requires a release Gate driver"
        );
        let exe = std::env::current_exe()?;
        let dir = exe.parent().context("missing prebuilt image directory")?;
        self.verify(dir)?;
        Ok(dir.to_path_buf())
    }

    pub(crate) fn stage(&self, release: &Path, folders: &Folders) -> Result<PathBuf> {
        let bin = folders
            .local
            .parent()
            .context("missing owned launch directory")?
            .join("bin");
        self.stage_into(release, &bin)?;
        Ok(bin)
    }

    pub(crate) fn stage_into(&self, release: &Path, bin: &Path) -> Result<()> {
        fs::create_dir(bin)?;
        for name in [SHELL_EXE, BRAIN_EXE] {
            fs::copy(release.join(name), bin.join(name))
                .with_context(|| format!("staging prebuilt {name}"))?;
        }
        self.verify(bin)
    }

    pub(crate) fn verify(&self, dir: &Path) -> Result<()> {
        for (name, expected, role) in [
            (SHELL_EXE, &self.shell_sha256, SHELL_ROLE),
            (BRAIN_EXE, &self.brain_sha256, BRAIN_ROLE),
        ] {
            let bytes = fs::read(dir.join(name))
                .with_context(|| format!("prebuilt {name} is unavailable; no build fallback"))?;
            let actual = platform::gate_capture::sha256_through(bytes.as_slice(), std::io::sink())
                .with_context(|| format!("hashing prebuilt {name}"))?;
            ensure!(
                actual == *expected,
                "prebuilt {name} SHA-256 differs from its pin"
            );
            require_compatible(&dir.join(name), role, Profile::Release)?;
        }
        Ok(())
    }
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
/// Refuses what it built unless each image is compatible with its role, so a baseline checkout
/// from before the folder override never launches.
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
    drop_run_variables(&mut cargo, std::env::vars_os().map(|(name, _)| name));
    if profile == Profile::Release {
        cargo.arg("--release");
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask is in the workspace")?;
    cargo
        .arg("--manifest-path")
        .arg(checkout.unwrap_or(workspace).join("Cargo.toml"));
    let built = live::run_bounded(cargo, BUILD_LIMIT, "the gate build")?;
    ensure!(built.success(), "the gate build failed ({built})");
    let folder = target.join(match profile {
        Profile::Debug => "debug",
        Profile::Release => "release",
    });
    require_built(&folder, profile, packages)?;
    Ok((folder, started.elapsed()))
}

/// Refuses the images `packages` built with `profile` into `folder` unless each is compatible with
/// its role.
fn require_built(folder: &Path, profile: Profile, packages: &[&str]) -> Result<()> {
    for package in packages {
        let (exe, role) = match *package {
            "kumochi-shell" => (SHELL_EXE, SHELL_ROLE),
            "kumochi-brain" => (BRAIN_EXE, BRAIN_ROLE),
            other => bail!("{other} is not a Gate image"),
        };
        require_compatible(&folder.join(exe), role, profile)?;
    }
    Ok(())
}

/// Removes from `cargo` each of `names` that `cargo run` gives xtask about its own package:
/// ring's build script reruns when any of them changes, so keeping them would rebuild a prebuilt
/// image inside a timed run.
fn drop_run_variables(cargo: &mut Command, names: impl IntoIterator<Item = std::ffi::OsString>) {
    for name in names {
        if name.to_str().is_some_and(|name| {
            name.starts_with("CARGO_PKG_") || name.starts_with("CARGO_MANIFEST_")
        }) {
            cargo.env_remove(name);
        }
    }
}

/// One launch's own local and roaming folders, with Claude Code off in its settings.
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

    /// Leaves onboarding owed, so the shell opens the onboarding window at start.
    pub(crate) fn owe_onboarding(&self) -> Result<()> {
        fs::write(
            self.roaming.join(GATE.data_dir).join(CONFIG_FILE),
            OWING_ONBOARDING,
        )
        .context("writing the gate settings")
    }

    /// Makes `model` on `connection` the default, at low effort.
    pub(crate) fn choose_model(&self, connection: &str, model: &str) -> Result<()> {
        fs::write(
            self.roaming.join(GATE.data_dir).join(CONFIG_FILE),
            format!(
                "default_model = {{ connection = \"{connection}\", model = \"{model}\", effort = \"Low\" }}\n{SETTINGS}"
            ),
        )
        .context("writing the gate settings")
    }

    /// Points `command`'s data and settings folders here through the override pair. It leaves
    /// `LOCALAPPDATA` and `APPDATA` alone: a Gate image ignores them once the pair is set.
    pub(crate) fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env(LOCAL_APPDATA_OVERRIDE, &self.local)
            .env(ROAMING_APPDATA_OVERRIDE, &self.roaming)
    }
}

/// The gate's own file `name` beside its run lock, in the real `LocalAppData`: no build moves or
/// deletes it, and gate cleanup leaves it.
pub(crate) fn gate_file(name: &str) -> Result<PathBuf> {
    Ok(platform::local_app_data()?.join(format!("{}-{name}", GATE.object_prefix)))
}

/// One gate run, alone, with its launches' folders under a root it created.
#[derive(Debug)]
pub(crate) struct Run {
    root: PathBuf,
    id: String,
    /// The chain from the volume root down to the run root, held until the run ends.
    _held: Vec<File>,
    /// Held until the run ends, so another run fails to take it; it names this run's root until
    /// the run has cleaned up, so the next run can clean up after an interrupted one.
    alone: File,
    /// The kill-on-close job holding this process and everything it started.
    job: platform::Held,
}

impl Run {
    /// Refuses unless this is the gate identity, no other gate run is on and no product process
    /// runs; puts this process in a kill-on-close job, so nothing it starts outlives it or breaks
    /// away from it; cleans up after an interrupted run; and creates the new root
    /// `<real LocalAppData>\kumochi-gate-runs\<label>-<stamp>` under a held chain.
    pub(crate) fn begin(label: &str) -> Result<Self> {
        require_gate(ProductIdentity::ACTIVE)?;
        let anchor = platform::local_app_data()?;
        let mut held = kumochi_storage::hold_chain(&anchor)
            .context("holding the local application data folder")?;
        let lock = gate_file("run.lock")?;
        let mut alone = take(&lock)?;
        live::refuse_running(&platform::processes()?, std::process::id())?;
        let job = platform::Job::kill_on_close()
            .and_then(platform::Job::hold_self)
            .context("putting the run in a job")?;
        let (runs, admitted) = runs_folder(&anchor)?;
        held.push(admitted);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = runs.join(format!("{label}-{stamp}"));
        let (admitted, id) = claim(&mut alone, &lock, &runs, &root, &mut CredentialManager)?;
        held.push(admitted);
        Ok(Self {
            root,
            id,
            _held: held,
            alone,
            job,
        })
    }

    /// The run's root, which holds its launch folders.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The run id in the root's marker, which a process the run starts checks with [`admit_root`].
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The job this run holds itself and everything it starts in.
    pub(crate) fn job(&self) -> platform::Held {
        self.job
    }

    pub(crate) fn folders(&self, name: &str) -> Result<Folders> {
        Folders::create(&self.root.join(name))
    }

    /// Ends the run with `result`. First ends every process the run started that is still
    /// running, whichever started it; once they're gone, deletes the gate's two key entries and
    /// what each launch's own authority names in Credential Manager, then the gate's instance
    /// entry for this session. After a success it also deletes the run's folders; after a failure
    /// it keeps them, with the product logs, until the next run. Nothing outside the run's root
    /// is deleted.
    pub(crate) fn finish(self, result: Result<()>) -> Result<()> {
        self.finish_with(result, Keep::Folders)
    }

    /// Ends an offline fixture run without keeping its application data on failure.
    pub(crate) fn finish_discard(self, result: Result<()>) -> Result<()> {
        self.finish_with(result, Keep::Nothing)
    }

    fn finish_with(self, result: Result<()>, on_failure: Keep) -> Result<()> {
        if let Err(running) = self.job.end_others(END_LIMIT) {
            return crate::session::combine([
                result,
                Err(anyhow!(
                    "{running:#}; so nothing was deleted: the gate's key entries may stay in \
                     Credential Manager until the next gate run deletes them, and {}",
                    kept(&self.root)
                )),
            ]);
        }
        let mut store = CredentialManager;
        let ended = end(&self.root, &self.alone, result, &mut store, on_failure);
        let instance = platform::gate_pipe::session_id()
            .map_err(anyhow::Error::from)
            .and_then(|session| store.delete(&instance_target(GATE.instance_service, session)));
        crate::session::combine([ended, instance])
    }
}

fn kept(root: &Path) -> String {
    format!(
        "the run's folders stay in {} until the next gate run deletes them",
        root.display()
    )
}

/// Deletes what the run's launches stored, keeping folders only on request after failure.
/// Empties the held lock only once they're gone, so the next run deletes whatever is left.
fn end(
    root: &Path,
    lock: &File,
    result: Result<()>,
    store: &mut impl Credentials,
    on_failure: Keep,
) -> Result<()> {
    let keep = if result.is_ok() {
        Keep::Nothing
    } else {
        on_failure
    };
    let entries = remove_entries(root, store, keep);
    let released = match (keep, &entries) {
        (Keep::Nothing, Ok(())) => lock.set_len(0).map_err(anyhow::Error::from),
        (Keep::Folders, _) => Err(anyhow!("{}", kept(root))),
        (Keep::Nothing, Err(_)) => Ok(()),
    };
    crate::session::combine([result, entries, released])
}

/// Admits the gate's runs folder in `anchor`, the real `LocalAppData`, whose chain the caller
/// holds: an existing one is used only if it passes the checks, and a link there is refused.
fn runs_folder(anchor: &Path) -> Result<(PathBuf, File)> {
    let runs = anchor.join(format!("{}-runs", GATE.object_prefix));
    let held =
        kumochi_storage::owner_only_dir(&runs).context("admitting the gate's runs folder")?;
    Ok((runs, held))
}

/// Takes the gate-run lock at `path`, whose folder's chain the caller holds, or refuses while
/// another run holds it. A link or a file with another name there is refused and never followed.
fn take(path: &Path) -> Result<File> {
    kumochi_storage::check_leaf(path)
        .with_context(|| format!("{} isn't a plain file of its own", path.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        platform::try_lock_run(&lock, EXCLUSIVE, Share::Exclusive)?,
        "another gate run is on; one runs at a time"
    );
    Ok(lock)
}

/// Cleans up the earlier run the held `lock` (at `path`) names, if it didn't finish and its root
/// is a checked folder directly in `runs`, then creates `root` in `runs`, which must be new, so
/// that every folder under it is this run's; marks it with a new run id; names both in the lock;
/// and returns the root's checked handle and the run id.
fn claim(
    lock: &mut File,
    path: &Path,
    runs: &Path,
    root: &Path,
    store: &mut impl Credentials,
) -> Result<(File, String)> {
    ensure!(
        !root.try_exists()?,
        "{} already exists; a gate run takes a new folder",
        root.display()
    );
    let named = root.to_str().context("the run's folder isn't Unicode")?;
    let mut earlier = String::new();
    lock.rewind()?;
    lock.read_to_string(&mut earlier)?;
    if !earlier.is_empty() {
        let line = earlier.trim_end();
        let folder = line.splitn(3, ' ').nth(2).unwrap_or(line);
        let unmade = || {
            format!(
                "{} names {folder}, which no gate run can be shown to have made, so nothing was \
                 deleted",
                path.display()
            )
        };
        let (id, earlier) = lock_line(&earlier).with_context(unmade)?;
        let _earlier = recorded(runs, &earlier).with_context(|| {
            format!(
                "{} names {folder}, which isn't a checked gate run folder in {}, so nothing was \
                 deleted",
                path.display(),
                runs.display()
            )
        })?;
        if fs::read_to_string(earlier.join(MARKER)).ok().as_deref() != Some(id) {
            bail!(unmade());
        }
        remove_entries(&earlier, store, Keep::Nothing).with_context(|| {
            format!(
                "cleaning up after the interrupted gate run in {folder}, which {} names",
                path.display()
            )
        })?;
        lock.set_len(0)?;
    }
    let held = kumochi_storage::new_owner_only_dir(root).context("creating the run's folder")?;
    let id = run_id();
    fs::write(root.join(MARKER), &id)?;
    publish(lock, &id, named)?;
    Ok((held, id))
}

/// Names the run `id` in `root` as the held `lock`'s line, then takes its published byte, which
/// it keeps until the run ends: only then may a process the run started be admitted.
fn publish(lock: &mut File, id: &str, root: &str) -> Result<()> {
    lock.set_len(0)?;
    lock.rewind()?;
    lock.write_all(format!("{LOCK_LINE}{id} {root}\n").as_bytes())?;
    ensure!(
        platform::try_lock_run(lock, PUBLISHED, Share::Exclusive)?,
        "another process holds this gate run's published lock"
    );
    Ok(())
}

/// Admits `root` for a process a gate run started, such as slice2's inner copy: it must be a
/// folder directly in `runs` whose chain holds, its marker must carry `id`, and a live run must
/// hold the gate-run lock at `lock` and have published a line naming exactly `id` and `root`, so a
/// run still recovering an earlier one admits nothing. Returns the held chain. A stale or foreign root is refused before anything is written in it.
pub(crate) fn admit_root(runs: &Path, lock: &Path, root: &Path, id: &str) -> Result<Vec<File>> {
    ensure!(
        root.parent() == Some(runs),
        "{} isn't a gate run's folder",
        root.display()
    );
    let held = kumochi_storage::hold_chain(root).context("holding the run's root")?;
    ensure!(
        fs::read_to_string(root.join(MARKER)).ok().as_deref() == Some(id),
        "{} isn't this gate run's folder",
        root.display()
    );
    let lock = File::open(lock).context("no gate run is on")?;
    ensure!(
        !platform::try_lock_run(&lock, PUBLISHED, Share::Shared)?,
        "no gate run is on"
    );
    let mut line = String::new();
    (&lock).take(4096).read_to_string(&mut line)?;
    ensure!(
        lock_line(&line) == Some((id, root.to_path_buf())),
        "{} isn't the live gate run's folder",
        root.display()
    );
    Ok(held)
}

/// The gate's runs folder in the real `LocalAppData`.
pub(crate) fn runs_path() -> Result<PathBuf> {
    gate_file("runs")
}

/// The run id and folder a gate run's lock `line` names, only when the line is whole: a truncated
/// or malformed line names nothing. Nothing is read at the folder.
fn lock_line(line: &str) -> Option<(&str, PathBuf)> {
    let (id, root) = line
        .strip_prefix(LOCK_LINE)?
        .strip_suffix('\n')?
        .split_once(' ')?;
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) || root.contains('\n') {
        return None;
    }
    let root = PathBuf::from(root);
    root.is_absolute().then_some((id, root))
}

/// The recorded root of an earlier run, opened without following a link, only when it's a folder
/// directly in `runs` that passes the checks.
fn recorded(runs: &Path, root: &Path) -> Option<File> {
    if root.parent() != Some(runs) {
        return None;
    }
    let trusted = Trusted::new().ok()?;
    open_checked(root, Kind::Folder, &trusted).ok()
}

/// A fresh 128-bit run id in hex; it tells this run's folder apart, and needn't be secret.
fn run_id() -> String {
    let half = || {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos()),
        );
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
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
/// authority records, then, unless `keep` says otherwise, the folder; then `root`. A launch that
/// fails keeps its folder, and the others still run. A folder whose authority can't be read keeps
/// its entries too, since nothing else can say which entries are its own.
fn remove_entries(root: &Path, store: &mut impl Credentials, keep: Keep) -> Result<()> {
    let mut kept = Vec::new();
    // The shell stages a pasted key under a fixed gate name that no authority records.
    for target in [
        anthropic_staging_target(GATE.credential_service),
        anthropic_key_target(GATE.credential_service),
    ] {
        if let Err(error) = store.delete(&target) {
            kept.push(format!("{error:#}"));
        }
    }
    for launch in fs::read_dir(root)? {
        let launch = launch?.path();
        if launch.file_name() == Some(MARKER.as_ref()) {
            continue;
        }
        if let Err(error) = remove_launch(&launch, store, keep) {
            kept.push(format!("{}: {error:#}", launch.display()));
        }
    }
    ensure!(
        kept.is_empty(),
        "kept for a manual cleanup: {}",
        kept.join("; ")
    );
    if keep == Keep::Folders {
        return Ok(());
    }
    let marker = root.join(MARKER);
    if marker.try_exists()? {
        fs::remove_file(marker)?;
    }
    fs::remove_dir(root).with_context(|| format!("deleting {}", root.display()))
}

/// Deletes the entries one launch's own authority names, then, unless kept, its folders, which
/// must hold only the gate identity's data: anything else is the only sign of what ran there.
fn remove_launch(launch: &Path, store: &mut impl Credentials, keep: Keep) -> Result<()> {
    let local = launch.join("local");
    let targets = own_entries(&local.join(GATE.data_dir).join(AUTHORITY_FILE))
        .map_err(|_| anyhow!("its credential authority couldn't be read"))?;
    for target in &targets {
        store.delete(target)?;
    }
    if keep == Keep::Folders {
        return Ok(());
    }
    for entry in fs::read_dir(&local)? {
        let name = entry?.file_name();
        ensure!(
            name == GATE.data_dir,
            "its data folder holds {}, which no gate image writes",
            name.display()
        );
    }
    fs::remove_dir_all(launch).with_context(|| format!("deleting {}", launch.display()))
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
pub(crate) use tests::{assert_claim_after_refusal, image};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::scratch;

    #[test]
    fn the_gate_build_drops_only_what_cargo_run_says_about_xtask() {
        let mut cargo = Command::new("cargo");
        drop_run_variables(
            &mut cargo,
            [
                "CARGO_PKG_NAME",
                "CARGO_PKG_VERSION_MAJOR",
                "CARGO_MANIFEST_DIR",
                "CARGO_MANIFEST_PATH",
                "CARGO_HOME",
                "CARGO_TARGET_DIR",
                "CARGO",
                "PATH",
            ]
            .map(std::ffi::OsString::from),
        );
        let mut dropped: Vec<String> = cargo
            .get_envs()
            .map(|(name, value)| {
                assert_eq!(value, None, "only removals");
                name.to_string_lossy().into_owned()
            })
            .collect();
        dropped.sort_unstable();
        assert_eq!(
            dropped,
            [
                "CARGO_MANIFEST_DIR",
                "CARGO_MANIFEST_PATH",
                "CARGO_PKG_NAME",
                "CARGO_PKG_VERSION_MAJOR"
            ]
        );
    }

    struct Fake(BTreeSet<String>);

    impl Credentials for Fake {
        fn delete(&mut self, target: &str) -> Result<()> {
            self.0.remove(target);
            Ok(())
        }
    }

    pub(crate) fn assert_claim_after_refusal(dir: &Path, body: impl FnOnce(&Path) -> Result<()>) {
        let path = dir.join("run.lock");
        let first = dir.join("first");
        let mut lock = take(&path).unwrap();
        let mut store = Fake(BTreeSet::new());
        claim(&mut lock, &path, dir, &first, &mut store).unwrap();
        let error = body(&first).expect_err("the first run must be refused");
        let expected = format!("{error:#}");
        let error = end(&first, &lock, Err(error), &mut store, Keep::Nothing).unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert!(
            !first.exists(),
            "failure must remove app data before another claim"
        );
        drop(lock);

        let mut lock = take(&path).unwrap();
        let next = dir.join("next");
        claim(&mut lock, &path, dir, &next, &mut store)
            .expect("next gate run can claim and recover");
        assert!(!first.exists());
        end(&next, &lock, Ok(()), &mut store, Keep::Nothing).unwrap();
    }

    /// Refuses to delete one target, as a locked Credential Manager entry would.
    struct Refusing(BTreeSet<String>, &'static str);

    impl Credentials for Refusing {
        fn delete(&mut self, target: &str) -> Result<()> {
            ensure!(
                target != self.1,
                "couldn't delete {target} from Credential Manager"
            );
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
        let error = Run::begin("refused").expect_err("a test build is never the gate identity");
        assert_eq!(format!("{error:#}"), REFUSAL);
    }

    /// An image's strings: `role`, `identity`'s instance service and the override names.
    pub(crate) fn image(role: &str, identity: ProductIdentity) -> String {
        format!(
            "{role}|{}|KUMOCHI_GATE_LOCALAPPDATA|KUMOCHI_GATE_APPDATA",
            identity.instance_service
        )
    }

    #[test]
    fn an_image_without_the_override_names_or_its_role_is_refused() {
        let dir = scratch::dir("gate-image");
        let exe = dir.join(BRAIN_EXE);
        fs::write(&exe, image("brain started", ProductIdentity::GATE)).expect("image");
        for profile in [Profile::Debug, Profile::Release] {
            require_compatible(&exe, BRAIN_ROLE, profile).expect("a current brain");
        }
        let refusal = format!(
            "{BRAIN_EXE} does not have the expected Gate identity, role and folder override; \
             rebuild it"
        );
        let with = |identity: ProductIdentity| {
            format!(
                "{}|{}",
                image("brain started", ProductIdentity::GATE),
                identity.instance_service
            )
        };
        fs::write(&exe, with(ProductIdentity::DEV)).expect("image");
        require_compatible(&exe, BRAIN_ROLE, Profile::Debug)
            .expect("a debug Gate image links the dev identity");
        let error = require_compatible(&exe, BRAIN_ROLE, Profile::Release)
            .expect_err("a release image never carries the dev identity");
        assert_eq!(format!("{error:#}"), refusal);
        for (case, bytes) in [
            ("release identity", with(ProductIdentity::RELEASE)),
            ("old", "brain started|Kumochi Gate instance".to_owned()),
            (
                "no roaming name",
                "brain started|Kumochi Gate instance|KUMOCHI_GATE_LOCALAPPDATA".to_owned(),
            ),
            (
                "no local name",
                "brain started|Kumochi Gate instance|KUMOCHI_GATE_APPDATA".to_owned(),
            ),
            ("shell role", image("shell started", ProductIdentity::GATE)),
            ("dev identity", image("brain started", ProductIdentity::DEV)),
        ] {
            fs::write(&exe, bytes).expect("image");
            for profile in [Profile::Debug, Profile::Release] {
                let error = require_compatible(&exe, BRAIN_ROLE, profile).expect_err(case);
                assert_eq!(format!("{error:#}"), refusal, "{case} {profile:?}");
            }
        }
    }

    #[test]
    fn a_built_pair_or_brain_alone_is_checked_for_each_role() {
        let profile = scratch::dir("gate-built");
        fs::write(
            profile.join(BRAIN_EXE),
            image("brain started", ProductIdentity::GATE),
        )
        .expect("brain");
        require_built(&profile, Profile::Release, &["kumochi-brain"])
            .expect("slice2's brain alone");
        let error = require_built(
            &profile,
            Profile::Release,
            &["kumochi-shell", "kumochi-brain"],
        )
        .expect_err("no shell was built");
        assert_eq!(error.to_string(), format!("{SHELL_EXE} is unavailable"));
        fs::write(
            profile.join(SHELL_EXE),
            image("shell started", ProductIdentity::GATE),
        )
        .expect("shell");
        require_built(
            &profile,
            Profile::Release,
            &["kumochi-shell", "kumochi-brain"],
        )
        .expect("the current pair");
        fs::write(
            profile.join(BRAIN_EXE),
            format!(
                "{}|{}",
                image("brain started", ProductIdentity::GATE),
                ProductIdentity::DEV.instance_service
            ),
        )
        .expect("a debug brain");
        require_built(&profile, Profile::Debug, &["kumochi-brain"])
            .expect("a debug brain links the dev identity");
        require_built(&profile, Profile::Release, &["kumochi-brain"])
            .expect_err("a release brain never does");
        fs::write(
            profile.join(BRAIN_EXE),
            "brain started|Kumochi Gate instance",
        )
        .expect("a baseline brain from before the override");
        for packages in [&["kumochi-brain"][..], &["kumochi-shell", "kumochi-brain"]] {
            let error =
                require_built(&profile, Profile::Release, packages).expect_err("an old brain");
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "{BRAIN_EXE} does not have the expected Gate identity, role and folder \
                     override; rebuild it"
                )
            );
        }
    }

    #[test]
    fn the_runs_folder_is_admitted_once_made_and_refused_as_a_link() {
        let anchor = scratch::local("gate-runs");
        let (runs, held) = runs_folder(&anchor).expect("a new runs folder");
        assert_eq!(runs, anchor.join("kumochi-gate-runs"));
        drop(held);
        runs_folder(&anchor).expect("the same folder, admitted again");

        let anchor = scratch::local("gate-runs-linked");
        let target = scratch::local("gate-runs-target");
        let link = anchor.join("kumochi-gate-runs");
        fs::create_dir(&link).expect("an empty folder");
        kumochi_storage::set_junction(&link, &target).expect("a junction");
        let error = runs_folder(&anchor).expect_err("a linked runs folder");
        assert_eq!(
            format!("{error:#}"),
            "admitting the gate's runs folder: a path component is a reparse point or resolves \
             elsewhere"
        );
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
    }

    #[test]
    fn launch_folders_are_their_own_onboarded_with_claude_code_off() {
        let dir = scratch::dir("gate-folders");
        let folders = Folders::create(&dir).expect("folders");
        assert_eq!(
            fs::read_to_string(dir.join("roaming/Kumochi Gate/config.toml")).expect("settings"),
            "onboarded = true\n\n[claude_code]\nenabled = false\n"
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
                ("KUMOCHI_GATE_APPDATA".as_ref(), Some(roaming.as_os_str())),
                (
                    "KUMOCHI_GATE_LOCALAPPDATA".as_ref(),
                    Some(local.as_os_str())
                ),
            ]
        );
        folders
            .choose_model("AnthropicKey", "claude-haiku-5-5")
            .expect("a default model");
        assert_eq!(
            fs::read_to_string(dir.join("roaming/Kumochi Gate/config.toml")).expect("settings"),
            "default_model = { connection = \"AnthropicKey\", model = \"claude-haiku-5-5\", \
             effort = \"Low\" }\nonboarded = true\n\n[claude_code]\nenabled = false\n"
        );
        folders.owe_onboarding().expect("owing onboarding");
        assert_eq!(
            fs::read_to_string(dir.join("roaming/Kumochi Gate/config.toml")).expect("settings"),
            "[claude_code]\nenabled = false\n"
        );
    }

    /// A launch's authority with the brain's `generations` table (`connections/authority.rs`).
    fn authority(launch: &Path, generations: &[[u8; 16]]) {
        let data = launch.join("local/Kumochi Gate");
        fs::create_dir_all(&data).expect("data folder");
        let db = Connection::open(data.join(AUTHORITY_FILE)).expect("authority");
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
        let dir = scratch::dir("gate-cleanup");
        let root = dir.join("run");
        authority(&root.join("app"), &[[0x07; 16]]);
        Folders::create(&root.join("idle")).expect("a launch that stored nothing");
        let ours = "Kumochi Gate/gen/host/07070707070707070707070707070707";
        let others = [
            "Kumochi Gate/gen/host/09090909090909090909090909090909/0",
            "Kumochi Dev/gen/host/07070707070707070707070707070707/0",
            "Kumochi/gen/host/07070707070707070707070707070707/0",
            "Kumochi Gate/gen/host/07070707070707070707070707070707/64",
            "anthropic-key-staged.Kumochi Dev",
            "anthropic-key.Kumochi",
        ];
        let mut store = Fake(
            [
                format!("{ours}/0"),
                format!("{ours}/63"),
                "anthropic-key-staged.Kumochi Gate".to_owned(),
                "anthropic-key.Kumochi Gate".to_owned(),
            ]
            .into_iter()
            .chain(others.map(str::to_owned))
            .collect(),
        );
        remove_entries(&root, &mut store, Keep::Nothing).expect("cleaned up");
        assert_eq!(store.0, others.map(str::to_owned).into());
        assert!(!root.exists());
    }

    #[test]
    fn a_failed_staging_delete_still_deletes_the_saved_key_and_names_what_is_left() {
        let root = scratch::dir("gate-refused-delete");
        Folders::create(&root.join("app")).expect("a launch that stored nothing");
        let staged = "anthropic-key-staged.Kumochi Gate";
        let mut store = Refusing(
            [staged.to_owned(), "anthropic-key.Kumochi Gate".to_owned()].into(),
            staged,
        );
        let error = remove_entries(&root, &mut store, Keep::Nothing).expect_err("one is left");
        assert_eq!(store.0, [staged.to_owned()].into());
        assert_eq!(
            format!("{error:#}"),
            "kept for a manual cleanup: couldn't delete anthropic-key-staged.Kumochi Gate from \
             Credential Manager"
        );
        assert!(
            root.is_dir(),
            "the root stays, so the next run deletes what is left"
        );
    }

    #[test]
    fn a_failed_run_deletes_its_entries_but_keeps_its_folders() {
        let root = scratch::dir("gate-failed");
        authority(&root.join("app"), &[[0x07; 16]]);
        let mut store = Fake(
            [
                "Kumochi Gate/gen/host/07070707070707070707070707070707/0".to_owned(),
                "anthropic-key-staged.Kumochi Gate".to_owned(),
            ]
            .into(),
        );
        remove_entries(&root, &mut store, Keep::Folders).expect("entries deleted");
        assert_eq!(store.0, BTreeSet::new());
        assert!(
            root.join("app/local/Kumochi Gate")
                .join(AUTHORITY_FILE)
                .is_file()
        );
    }

    #[test]
    fn the_lock_is_emptied_only_after_a_success_whose_cleanup_finished() {
        let line = "kumochi-gate-run 0123456789abcdef0123456789abcdef C:/run\n";
        let held = |dir: &Path| {
            let path = dir.join("run.lock");
            fs::write(&path, line).expect("a run's lock");
            take(&path).expect("lock")
        };
        let length = |lock: &File| lock.metadata().expect("lock").len();
        let whole = u64::try_from(line.len()).expect("a short line");
        let dir = scratch::dir("gate-end-success");
        let root = dir.join("run");
        authority(&root.join("app"), &[[0x07; 16]]);
        let mut store = Fake([HELD.to_owned()].into());
        let lock_dir = scratch::dir("gate-end-success-lock");
        let lock = held(&lock_dir);
        end(&root, &lock, Ok(()), &mut store, Keep::Folders).expect("ended");
        assert_eq!(length(&lock), 0);
        assert!(!root.exists());
        assert_eq!(store.0, BTreeSet::new());

        let root = scratch::dir("gate-end-failed");
        authority(&root.join("app"), &[[0x07; 16]]);
        let mut store = Fake([HELD.to_owned()].into());
        let lock_dir = scratch::dir("gate-end-failed-lock");
        let lock = held(&lock_dir);
        let error = end(
            &root,
            &lock,
            Err(anyhow!("an item failed")),
            &mut store,
            Keep::Folders,
        )
        .expect_err("the run failed");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "an item failed; the run's folders stay in {} until the next gate run deletes \
                 them",
                root.display()
            )
        );
        assert_eq!(length(&lock), whole);
        assert!(
            root.join("app/local/Kumochi Gate")
                .join(AUTHORITY_FILE)
                .is_file()
        );
        assert_eq!(store.0, BTreeSet::new());

        let root = scratch::dir("gate-end-unclean");
        authority(&root.join("app"), &[[0x07; 16]]);
        fs::create_dir(root.join("app/local/Kumochi")).expect("another identity's folder");
        let mut store = Fake([HELD.to_owned()].into());
        let lock_dir = scratch::dir("gate-end-unclean-lock");
        let lock = held(&lock_dir);
        let error =
            end(&root, &lock, Ok(()), &mut store, Keep::Folders).expect_err("cleanup failed");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "kept for a manual cleanup: {}: its data folder holds Kumochi, which no gate image \
                 writes",
                root.join("app").display()
            )
        );
        assert_eq!(length(&lock), whole);
        assert!(root.join("app").is_dir());
    }

    #[test]
    fn a_second_gate_run_is_refused_while_the_first_holds_the_lock() {
        let dir = scratch::dir("gate-lock");
        let lock = dir.join("run.lock");
        fs::write(&lock, "C:/the/first/run").expect("the first run's folder");
        let first = take(&lock).expect("the first run takes it");
        let error = take(&lock).expect_err("the second is refused");
        assert_eq!(
            format!("{error:#}"),
            "another gate run is on; one runs at a time"
        );
        drop(first);
        assert_eq!(fs::read_to_string(&lock).expect("lock"), "C:/the/first/run");
        take(&lock).expect("free again once the first ends");
    }

    #[test]
    fn a_run_cleans_up_after_an_interrupted_one_and_takes_only_a_new_folder() {
        let dir = scratch::local("gate-claim");
        let (earlier, line) = interrupted_run(&dir);
        let mut store = Fake([HELD.to_owned()].into());
        let path = dir.join("run.lock");
        fs::write(&path, line).expect("an interrupted run's lock");
        let mut lock = take(&path).expect("lock");
        let root = dir.join("now");
        claim(&mut lock, &path, &dir, &root, &mut store).expect("claimed");
        assert_eq!(store.0, BTreeSet::new());
        assert!(!earlier.exists());
        drop(lock);
        let id = fs::read_to_string(root.join(MARKER)).expect("the new run's id");
        assert_eq!(id.len(), 32);
        assert_ne!(id, EARLIER_ID);
        assert_eq!(
            fs::read_to_string(&path).expect("lock"),
            format!("kumochi-gate-run {id} {}\n", root.display())
        );
        let existing = dir.join("existing");
        fs::create_dir(&existing).expect("a folder another run made");
        fs::write(&path, "").expect("a finished run's lock");
        let mut lock = take(&path).expect("lock");
        let error = claim(&mut lock, &path, &dir, &existing, &mut store).expect_err("not new");
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
    fn a_linked_or_hard_linked_run_lock_is_refused_and_its_target_kept() {
        let dir = scratch::local("gate-lock-linked");
        let target = dir.join("target");
        fs::write(&target, "kept").expect("a target file");
        let lock = dir.join("run.lock");
        fs::hard_link(&target, &lock).expect("a hard link");
        let error = take(&lock).expect_err("a hard-linked lock");
        assert_eq!(
            error.to_string(),
            format!("{} isn't a plain file of its own", lock.display())
        );
        fs::remove_file(&lock).expect("the hard link removed");
        // A file symlink needs Developer Mode or the privilege; a junction is the same reparse leaf.
        if std::os::windows::fs::symlink_file(&target, &lock).is_ok() {
            eprintln!("run lock witness: symlink");
        } else {
            eprintln!("run lock witness: junction");
            fs::create_dir(&lock).expect("an empty folder");
            kumochi_storage::set_junction(&lock, &dir).expect("a junction");
        }
        let error = take(&lock).expect_err("a linked lock");
        assert_eq!(
            error.to_string(),
            format!("{} isn't a plain file of its own", lock.display())
        );
        assert_eq!(fs::read_to_string(&target).expect("target"), "kept");
    }

    #[test]
    fn recovery_keeps_a_recorded_run_outside_the_runs_folder_or_behind_a_link() {
        let dir = scratch::local("gate-recovery-located");
        let runs = dir.join("runs");
        fs::create_dir(&runs).expect("a runs folder");
        let (elsewhere, line) = interrupted_run(&dir);
        let linked = runs.join("linked");
        fs::create_dir(&linked).expect("an empty folder");
        kumochi_storage::set_junction(&linked, &elsewhere).expect("a junction");
        let through = format!("kumochi-gate-run {EARLIER_ID} {}\n", linked.display());
        let unmarked = dir.join("unmarked");
        fs::create_dir(&unmarked).expect("a folder no run marked");
        let unread = format!("kumochi-gate-run {EARLIER_ID} {}\n", unmarked.display());
        for (case, line, named) in [
            ("outside", line, &elsewhere),
            ("outside-unmarked", unread, &unmarked),
            ("linked", through, &linked),
        ] {
            let mut store = Fake([HELD.to_owned()].into());
            let path = dir.join("run.lock");
            fs::write(&path, &line).expect("lock");
            let mut lock = take(&path).expect("lock");
            let root = runs.join("now");
            let error = claim(&mut lock, &path, &runs, &root, &mut store).expect_err(case);
            drop(lock);
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "{} names {}, which isn't a checked gate run folder in {}, so nothing was \
                     deleted",
                    path.display(),
                    named.display(),
                    runs.display()
                ),
                "{case}"
            );
            assert_eq!(store.0, [HELD.to_owned()].into(), "{case}");
            assert!(
                elsewhere
                    .join("app/local/Kumochi Gate")
                    .join(AUTHORITY_FILE)
                    .is_file(),
                "{case}"
            );
            assert!(!root.exists(), "{case}");
            assert_eq!(fs::read_to_string(&path).expect("lock"), line, "{case}");
        }
    }

    #[test]
    fn a_started_process_admits_only_the_live_runs_own_root() {
        let dir = scratch::local("gate-admit-root");
        let runs = dir.join("runs");
        let (old, live) = (runs.join("a"), runs.join("b"));
        for (root, id) in [(&old, EARLIER_ID), (&live, LIVE_ID)] {
            fs::create_dir_all(root).expect("a run root");
            fs::write(root.join(MARKER), id).expect("its marker");
        }
        let lock = dir.join("run.lock");
        fs::write(
            &lock,
            format!("{LOCK_LINE}{EARLIER_ID} {}\n", old.display()),
        )
        .expect("the interrupted run's line");
        let other = dir.join("other");
        fs::create_dir(&other).expect("a folder elsewhere");
        fs::write(other.join(MARKER), EARLIER_ID).expect("a marker");
        let refused = |root: &Path, id: &str| {
            admit_root(&runs, &lock, root, id)
                .expect_err("refused")
                .to_string()
        };
        assert_eq!(
            refused(&other, EARLIER_ID),
            format!("{} isn't a gate run's folder", other.display())
        );
        assert_eq!(
            refused(&live, EARLIER_ID),
            format!("{} isn't this gate run's folder", live.display())
        );
        assert_eq!(refused(&old, EARLIER_ID), "no gate run is on");
        let observer = File::open(&lock).expect("another observer");
        assert!(platform::try_lock_run(&observer, PUBLISHED, Share::Shared).expect("a probe"));
        assert_eq!(
            refused(&old, EARLIER_ID),
            "no gate run is on",
            "while another observer probes"
        );
        drop(observer);
        let mut held = take(&lock).expect("the next run's lock");
        assert_eq!(
            refused(&old, EARLIER_ID),
            "no gate run is on",
            "while the next run may still be recovering it"
        );
        publish(&mut held, LIVE_ID, live.to_str().expect("Unicode")).expect("published");
        assert_eq!(
            refused(&old, EARLIER_ID),
            format!("{} isn't the live gate run's folder", old.display())
        );
        admit_root(&runs, &lock, &live, LIVE_ID).expect("the live run's own root");
        drop(held);
        assert_eq!(refused(&live, LIVE_ID), "no gate run is on");
        for root in [&old, &live] {
            let mut names: Vec<_> = fs::read_dir(root)
                .expect("lists")
                .map(|entry| entry.expect("entry").file_name())
                .collect();
            names.sort();
            assert_eq!(names, [MARKER], "nothing was written in {}", root.display());
        }
    }

    const LIVE_ID: &str = "fedcba9876543210fedcba9876543210";

    const HELD: &str = "Kumochi Gate/gen/host/07070707070707070707070707070707/0";
    const EARLIER_ID: &str = "0123456789abcdef0123456789abcdef";

    /// An interrupted run's folder in `dir`, marked and holding one generation, and its lock line.
    fn interrupted_run(dir: &Path) -> (PathBuf, String) {
        let earlier = dir.join("earlier");
        authority(&earlier.join("app"), &[[0x07; 16]]);
        fs::write(earlier.join(MARKER), EARLIER_ID).expect("marker");
        let line = format!("kumochi-gate-run {EARLIER_ID} {}\n", earlier.display());
        (earlier, line)
    }

    #[test]
    fn recovery_deletes_nothing_unless_the_whole_lock_and_the_folders_mark_agree() {
        type Spoil = fn(&Path, &mut String);
        // Each case, the bytes it cuts from the folder's name, and how it spoils the run.
        let cases: [(&str, usize, Spoil); 4] = [
            ("no-newline", 0, |_, line| line.truncate(line.len() - 1)),
            ("cut-path", 3, |_, line| line.truncate(line.len() - 4)),
            ("unmarked", 0, |earlier, _| {
                fs::remove_file(earlier.join(MARKER)).expect("no mark");
            }),
            ("another-id", 0, |earlier, _| {
                fs::write(earlier.join(MARKER), "fedcba9876543210fedcba9876543210")
                    .expect("another run's id");
            }),
        ];
        for (case, cut, spoil) in cases {
            let dir = scratch::local(&format!("gate-recovery-{case}"));
            let (earlier, mut line) = interrupted_run(&dir);
            spoil(&earlier, &mut line);
            let mut store = Fake([HELD.to_owned()].into());
            let path = dir.join("run.lock");
            fs::write(&path, &line).expect("lock");
            let mut lock = take(&path).expect("lock");
            let root = dir.join("now");
            let error = claim(&mut lock, &path, &dir, &root, &mut store).expect_err(case);
            drop(lock);
            let named = earlier.display().to_string();
            let named = &named[..named.len() - cut];
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "{} names {named}, which no gate run can be shown to have made, so nothing \
                     was deleted",
                    path.display()
                ),
                "{case}"
            );
            assert_eq!(store.0, [HELD.to_owned()].into(), "{case}");
            assert!(
                earlier
                    .join("app/local/Kumochi Gate")
                    .join(AUTHORITY_FILE)
                    .is_file()
            );
            assert!(!root.exists(), "{case}");
            assert_eq!(fs::read_to_string(&path).expect("lock"), line, "{case}");
        }
    }

    #[test]
    fn a_recovered_run_is_forgotten_even_when_the_new_folder_cannot_be_made() {
        let dir = scratch::local("gate-recovered");
        let (earlier, line) = interrupted_run(&dir);
        let mut store = Fake([HELD.to_owned()].into());
        let path = dir.join("run.lock");
        fs::write(&path, line).expect("an interrupted run's lock");
        fs::write(dir.join("file"), "").expect("a file where a folder would go");
        let mut lock = take(&path).expect("lock");
        claim(&mut lock, &path, &dir, &dir.join("file/now"), &mut store)
            .expect_err("no folder in a file");
        assert_eq!(store.0, BTreeSet::new());
        assert!(!earlier.exists());
        drop(lock);
        assert_eq!(fs::read_to_string(&path).expect("lock"), "");
        let mut lock = take(&path).expect("the next run's lock");
        claim(&mut lock, &path, &dir, &dir.join("now"), &mut store)
            .expect("the next claim goes through");
    }

    #[test]
    fn an_empty_lock_names_no_earlier_run_so_nothing_is_deleted() {
        let dir = scratch::local("gate-recovery-empty");
        let (earlier, _) = interrupted_run(&dir);
        let mut store = Fake([HELD.to_owned()].into());
        let path = dir.join("run.lock");
        fs::write(&path, "").expect("lock");
        let mut lock = take(&path).expect("lock");
        claim(&mut lock, &path, &dir, &dir.join("now"), &mut store).expect("claimed");
        assert_eq!(store.0, [HELD.to_owned()].into());
        assert!(
            earlier
                .join("app/local/Kumochi Gate")
                .join(AUTHORITY_FILE)
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(earlier.join(MARKER)).expect("mark"),
            EARLIER_ID
        );
    }

    #[test]
    fn cleanup_keeps_a_launch_whose_authority_it_cannot_read() {
        let root = scratch::dir("gate-unreadable");
        let data = root.join("app/local/Kumochi Gate");
        fs::create_dir_all(&data).expect("data folder");
        fs::write(
            data.join(AUTHORITY_FILE),
            b"not a database, but long enough to be read as one",
        )
        .expect("authority");
        authority(&root.join("good"), &[[0x08; 16]]);
        let mut store = Fake(
            [
                HELD.to_owned(),
                "Kumochi Gate/gen/host/08080808080808080808080808080808/0".to_owned(),
            ]
            .into(),
        );
        let error = remove_entries(&root, &mut store, Keep::Nothing).expect_err("kept");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "kept for a manual cleanup: {}: its credential authority couldn't be read",
                root.join("app").display()
            )
        );
        assert_eq!(store.0, [HELD.to_owned()].into());
        assert!(data.join(AUTHORITY_FILE).is_file());
        assert!(!root.join("good").exists());
    }

    #[test]
    fn cleanup_keeps_a_launch_whose_data_folder_holds_another_identity() {
        let root = scratch::dir("gate-foreign");
        authority(&root.join("app"), &[[0x07; 16]]);
        fs::create_dir(root.join("app/local/Kumochi")).expect("another identity's folder");
        let mut store = Fake([HELD.to_owned()].into());
        let error = remove_entries(&root, &mut store, Keep::Nothing).expect_err("kept");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "kept for a manual cleanup: {}: its data folder holds Kumochi, which no gate image \
                 writes",
                root.join("app").display()
            )
        );
        assert_eq!(store.0, BTreeSet::new());
        assert!(root.join("app/local/Kumochi").is_dir());
    }
}
