//! `cargo xtask live anthropic-key`: Anthropic API key chat end to end on the owner's two keys,
//! as the gate identity, so neither key ever lands under the dev or installed credential names. The
//! owner pastes key A and key B into the real row and types the prompts this prints: the check never
//! reads the clipboard, types or pastes.
//!
//! The approval is six requests: three model-list GETs and three Messages POSTs, no retries. They
//! are fixed slices, one per launch of the real gate shell. The supervisor starts each launch's
//! debug brain itself with exactly its slice in the transport's budget variables, so a request
//! past it is refused before connecting, and starts the shell with both at zero, so any brain the
//! shell starts, a replacement for one that died included, can't connect at all. The ledger, beside
//! the gate's run lock in `LocalAppData`, records each slice before its launch and is never
//! rewritten: a run refuses to start while an earlier run's ledger exists, so a restart never gets
//! a slice back.
//! The supervisor watches the brain's pipe tap, which keeps only content-free control frames, and
//! stops at the first state the plan doesn't expect. See `docs/development/live-checks.md`.

use std::fs::File;
use std::io::{BufReader, ErrorKind, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{
    BRAIN_EXE, LIVE_KEY_CATALOG_BUDGET_ENV, LIVE_KEY_MESSAGES_BUDGET_ENV, LIVE_RUNS_DIR,
    PIPE_TAP_ENV, SHELL_EXE,
};
use kumochi_protocol::frame::{Pipe, decode};
use kumochi_protocol::message::{ConnectionsView, ControlToShell, KeyView, Phase, RunId, ThreadId};
use kumochi_protocol::tap::{self, Direction, Record};

use crate::hermetic::{self, Folders, Profile, Run};
use crate::platform;

/// The cheapest current model the key route serves with tools.
const MODEL: &str = "claude-haiku-5-5";
/// A read-only local tool with no screen capture, so nothing but text leaves the machine.
const TOOL_ASK: &str =
    "Use the list_tasks tool once, then say in one short sentence how many tasks it listed.";
const SECOND_ASK: &str = "In one short sentence: what colour is a clear daytime sky?";
const RETIRED_ASK: &str = "Say hello in one word.";
/// The ledger's name beside the gate's run lock; only a person deletes it.
const LEDGER: &str = "anthropic-key.ledger";
/// How long the owner gets for one step [guess].
const STEP: Duration = Duration::from_mins(10);
/// How long the final launch must stay without a key, a check or a run.
const QUIET: Duration = Duration::from_secs(30);
/// How long the shell gets to exit after its window is closed.
const QUIT: Duration = Duration::from_secs(60);
/// How long the brain gets to open its pipe before the shell starts.
const LISTEN: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slice {
    catalog: u32,
    messages: u32,
}

/// The whole approval.
const TOTAL: Slice = Slice {
    catalog: 3,
    messages: 3,
};

/// What a step waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// A pasted key is staged, then accepted.
    Pasted,
    /// The saved key is checked at start and accepted.
    Started,
    /// A run ends `Done`; with `same`, in the first answered run's thread.
    Answered { same: bool },
    /// The key is gone.
    Removed,
    /// A submit with no key is refused, so nothing is sent.
    Refused,
    /// No key, no check and no run for [`QUIET`].
    Quiet,
}

#[derive(Debug)]
struct Step {
    say: &'static str,
    ask: Option<&'static str>,
    expect: Expect,
}

#[derive(Debug)]
struct Launch {
    slice: Slice,
    steps: &'static [Step],
}

const fn step(say: &'static str, ask: Option<&'static str>, expect: Expect) -> Step {
    Step { say, ask, expect }
}

const PLAN: [Launch; 3] = [
    Launch {
        slice: Slice {
            catalog: 1,
            messages: 0,
        },
        steps: &[step(
            "Paste key A into Settings > Accounts > Anthropic API key.",
            None,
            Expect::Pasted,
        )],
    },
    Launch {
        slice: Slice {
            catalog: 2,
            messages: 3,
        },
        steps: &[
            step(
                "Wait while the saved key is checked.",
                None,
                Expect::Started,
            ),
            step(
                "In Kumo's chat (not a task), with the screen off, type exactly:",
                Some(TOOL_ASK),
                Expect::Answered { same: false },
            ),
            step(
                "Paste key B into the Anthropic API key row.",
                None,
                Expect::Pasted,
            ),
            step(
                "In the same chat, type exactly:",
                Some(SECOND_ASK),
                Expect::Answered { same: true },
            ),
            step(
                "Remove the key: Remove key..., then confirm.",
                None,
                Expect::Removed,
            ),
            step(
                "In the same chat, type exactly (it must not send):",
                Some(RETIRED_ASK),
                Expect::Refused,
            ),
        ],
    },
    Launch {
        slice: Slice {
            catalog: 0,
            messages: 0,
        },
        steps: &[step("Wait: nothing is sent.", None, Expect::Quiet)],
    },
];

// The slices add up to the approval, and the final restart has none.
const _: () = {
    let (mut catalog, mut messages, mut index) = (0, 0, 0);
    while index < PLAN.len() {
        catalog += PLAN[index].slice.catalog;
        messages += PLAN[index].slice.messages;
        index += 1;
    }
    assert!(
        catalog == TOTAL.catalog && messages == TOTAL.messages,
        "the slices add up to the approval"
    );
    let last = PLAN[PLAN.len() - 1].slice;
    assert!(
        last.catalog == 0 && last.messages == 0,
        "the final restart has no allowance"
    );
};

pub(super) fn run() -> Result<()> {
    let live = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .context("xtask is not in a Cargo profile folder")?
        .join(LIVE_RUNS_DIR);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let run = Run::begin(live.join("anthropic-key").join(stamp.to_string()))?;
    let checked = (|| {
        let (debug, took) =
            hermetic::build(&["kumochi-shell", "kumochi-brain"], Profile::Debug, None)?;
        println!("build_seconds={:.1}", took.as_secs_f64());
        let folders = run.folders("key")?;
        folders.choose_model("AnthropicKey", MODEL)?;
        let taps = folders
            .local
            .parent()
            .context("a launch folder has a parent")?
            .to_owned();
        let mut ledger = Ledger::create(&ledger_path()?)?;
        for (index, launch) in PLAN.iter().enumerate() {
            let number = index + 1;
            ledger.grant(number, launch.slice)?;
            println!(
                "launch {number}: {} model-list and {} Messages requests at most",
                launch.slice.catalog, launch.slice.messages
            );
            let tap = taps.join(format!("tap-{number}.bin"));
            supervise(
                brain_command(&debug.join(BRAIN_EXE), &folders, &tap, launch.slice),
                shell_command(&debug.join(SHELL_EXE), &folders),
                &tap,
                launch.steps,
            )
            .with_context(|| format!("launch {number}"))?;
        }
        Ok(())
    })();
    match &checked {
        Ok(()) => println!("live anthropic-key: every step passed"),
        Err(error) => println!("live anthropic-key: stopped: {error:#}"),
    }
    run.finish(checked)
}

/// The launch's one brain, which the supervisor starts itself: exactly `slice` as its transport
/// budgets and a new tap, and no other Kumochi variable from this environment.
fn brain_command(brain: &Path, folders: &Folders, tap: &Path, slice: Slice) -> Command {
    let mut command = clean(brain, folders);
    command
        .env(LIVE_KEY_CATALOG_BUDGET_ENV, slice.catalog.to_string())
        .env(LIVE_KEY_MESSAGES_BUDGET_ENV, slice.messages.to_string())
        .env(PIPE_TAP_ENV, tap)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The gate shell, with both key budgets at zero: a brain it starts, such as one replacing a
/// brain that died, inherits them and can't open a single key connection.
fn shell_command(shell: &Path, folders: &Folders) -> Command {
    let mut command = clean(shell, folders);
    command
        .env(LIVE_KEY_CATALOG_BUDGET_ENV, "0")
        .env(LIVE_KEY_MESSAGES_BUDGET_ENV, "0");
    command
}

fn clean(exe: &Path, folders: &Folders) -> Command {
    let mut command = Command::new(exe);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("KUMOCHI_") {
            command.env_remove(name);
        }
    }
    folders.apply(&mut command);
    command
}

/// Whether `running` holds exactly this launch's brain and shell besides `me`.
fn only_ours(running: &[platform::Entry], me: u32, ours: [u32; 2]) -> bool {
    let mut others: Vec<u32> = running
        .iter()
        .map(|entry| entry.pid)
        .filter(|&pid| pid != me)
        .collect();
    others.sort_unstable();
    let mut ours = ours;
    ours.sort_unstable();
    others == ours
}

/// Where the ledger lives: beside the gate's run lock, so cleaning or moving build output can't
/// lose it, and taken only under that lock, so a second run refuses while one is on.
fn ledger_path() -> Result<std::path::PathBuf> {
    hermetic::gate_file(LEDGER)
}

/// Grants each launch its slice, in order, before it starts; never reopened or rewritten.
#[derive(Debug)]
struct Ledger(File);

impl Ledger {
    fn create(path: &Path) -> Result<Self> {
        match File::create_new(path) {
            Ok(file) => Ok(Self(file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => bail!(
                "{} exists: an earlier run spent its approval, and slices are never refunded; a \
                 new run needs a new approval, after which the owner deletes that file",
                path.display()
            ),
            Err(error) => Err(anyhow!(error).context(format!("creating {}", path.display()))),
        }
    }

    fn grant(&mut self, launch: usize, slice: Slice) -> Result<()> {
        writeln!(
            self.0,
            "launch {launch} catalog {} messages {}",
            slice.catalog, slice.messages
        )?;
        self.0.sync_all()?;
        Ok(())
    }
}

/// Starts the launch's brain, then its shell once the brain listens, so the shell connects to that
/// brain instead of starting one; runs the steps, then closes the shell and waits for every product
/// process to exit.
fn supervise(mut brain: Command, mut shell: Command, tap: &Path, steps: &[Step]) -> Result<()> {
    let mut brain = brain.spawn().context("starting the gate brain")?;
    let pipe = Pipe::Control.name(platform::gate_pipe::session_id()?);
    let listen = Instant::now() + LISTEN;
    while !platform::gate_pipe::listening(&pipe) {
        ensure!(
            brain.try_wait()?.is_none(),
            "the gate brain exited before it listened"
        );
        ensure!(
            Instant::now() < listen,
            "the gate brain didn't listen in {LISTEN:?}"
        );
        thread::sleep(POLL);
    }
    let mut child = shell.spawn().context("starting the gate shell")?;
    let ours = [brain.id(), child.id()];
    let mut cursor = 0;
    let mut state = State::default();
    let stepped = steps
        .iter()
        .try_for_each(|step| watch(ours, tap, step, &mut state, &mut cursor));
    if let Err(error) = stepped {
        return crate::session::combine([
            Err(error),
            crate::live::end(&mut child),
            crate::live::end(&mut brain),
        ]);
    }
    platform::close_shell_window(&mut child).context("closing the gate shell")?;
    let deadline = Instant::now() + QUIT;
    while !platform::processes()?
        .iter()
        .all(|entry| entry.pid == std::process::id())
    {
        if Instant::now() >= deadline {
            return crate::session::combine([
                Err(anyhow!(
                    "the gate shell didn't exit in {QUIT:?} after its window closed; it was ended"
                )),
                crate::live::end(&mut child),
                crate::live::end(&mut brain),
            ]);
        }
        thread::sleep(POLL);
    }
    let after = &read(tap)?[cursor..];
    ensure!(
        after
            .iter()
            .all(|seen| matches!(seen, Seen::Key(KeyView::Ready | KeyView::Missing))),
        "the brain reported more while quitting: {after:?}"
    );
    Ok(())
}

/// Waits for `step` against the tap from `cursor` on, leaving `cursor` after the frame that met it.
fn watch(
    ours: [u32; 2],
    tap: &Path,
    step: &Step,
    state: &mut State,
    cursor: &mut usize,
) -> Result<()> {
    println!("{}", step.say);
    if let Some(ask) = step.ask {
        println!("    {ask}");
    }
    *state = State {
        thread: state.thread,
        ..State::default()
    };
    let started = Instant::now();
    loop {
        ensure!(
            only_ours(&platform::processes()?, std::process::id(), ours),
            "the gate brain or shell exited, or another product process started; nothing it starts \
             can send on the key route, and the check stops"
        );
        let seen = read(tap)?;
        while let Some(event) = seen.get(*cursor) {
            *cursor += 1;
            match check(step.expect, state, event) {
                Progress::Waiting => {}
                Progress::Met => {
                    println!("  ok");
                    return Ok(());
                }
                Progress::Stop(why) => bail!("{:?}: {why}", step.expect),
            }
        }
        if step.expect == Expect::Quiet && state.missing && started.elapsed() >= QUIET {
            println!("  ok");
            return Ok(());
        }
        ensure!(
            started.elapsed() < STEP,
            "{:?} didn't happen in {STEP:?}",
            step.expect
        );
        thread::sleep(POLL);
    }
}

/// What the tap shows that the plan cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    /// The shell staged a pasted key.
    Staged,
    Key(KeyView),
    Run {
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        error: bool,
    },
    /// A submit was refused.
    Refused,
    /// A frame the plan never expects, by its op name.
    Unexpected(String),
}

/// Ops that end the check wherever they appear: an error, a screen capture, another build.
const UNEXPECTED: [&str; 5] = [
    "Error",
    "CaptureDone",
    "HoldScreen",
    "ScreenHoldRecognized",
    "BuildMismatch",
];

/// Every complete record in the recording at `path`; a record still being written is left for
/// the next read, and a missing file is an empty recording.
fn read(path: &Path) -> Result<Vec<Seen>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    if tap::read_header(&mut reader).is_err() {
        return Ok(Vec::new());
    }
    let mut seen = Vec::new();
    while let Ok(Some(record)) = tap::read(&mut reader) {
        seen.extend(of(&record));
    }
    Ok(seen)
}

fn of(record: &Record) -> Option<Seen> {
    let op = record.op.as_str();
    if UNEXPECTED.contains(&op) {
        return Some(Seen::Unexpected(op.to_owned()));
    }
    match (record.pipe, record.direction, op) {
        (Pipe::Control, Direction::ToBrain, "AnthropicKeyStaged") => Some(Seen::Staged),
        (Pipe::Control, Direction::ToShell, "Connections" | "RunState" | "TurnRefused") => {
            let Some(Ok(message)) = record.payload.as_deref().map(decode::<ControlToShell>) else {
                return Some(Seen::Unexpected(format!("undecodable {op}")));
            };
            match message {
                ControlToShell::Connections(ConnectionsView { anthropic, .. }) => {
                    Some(Seen::Key(anthropic))
                }
                ControlToShell::RunState {
                    run,
                    thread,
                    phase,
                    error,
                    ..
                } => Some(Seen::Run {
                    run,
                    thread,
                    phase,
                    error: error.is_some(),
                }),
                ControlToShell::TurnRefused { .. } => Some(Seen::Refused),
                _ => Some(Seen::Unexpected(format!("mislabelled {op}"))),
            }
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct State {
    staged: bool,
    missing: bool,
    run: Option<RunId>,
    thread: Option<ThreadId>,
}

#[derive(Debug, PartialEq, Eq)]
enum Progress {
    Waiting,
    Met,
    Stop(String),
}

/// What `seen` means for a step waiting for `expect`. Anything the step doesn't name stops the
/// check, as do an unexpected op and a refused or unconfirmed key. An answer is one run to `Done`;
/// `thread` carries the first answered run's thread to the second.
fn check(expect: Expect, state: &mut State, seen: &Seen) -> Progress {
    use Progress::{Met, Waiting};
    let progress = match (expect, seen) {
        (Expect::Pasted, Seen::Staged) => {
            state.staged = true;
            Some(Waiting)
        }
        (Expect::Pasted, Seen::Key(KeyView::Ready)) => {
            Some(if state.staged { Met } else { Waiting })
        }
        (Expect::Pasted | Expect::Started, Seen::Key(KeyView::Missing | KeyView::Checking))
        | (Expect::Answered { .. }, Seen::Key(KeyView::Ready))
        | (Expect::Removed, Seen::Key(KeyView::Ready | KeyView::Checking))
        | (
            Expect::Refused,
            Seen::Key(KeyView::Missing)
            | Seen::Run {
                phase: Phase::Thinking,
                ..
            },
        ) => Some(Waiting),
        (Expect::Started, Seen::Key(KeyView::Ready))
        | (Expect::Removed, Seen::Key(KeyView::Missing))
        | (
            Expect::Refused,
            Seen::Refused
            | Seen::Run {
                phase: Phase::Failed,
                ..
            },
        ) => Some(Met),
        (
            Expect::Answered { same },
            &Seen::Run {
                run,
                thread,
                phase,
                error,
            },
        ) => Some(if state.run.is_some_and(|held| held != run) {
            Progress::Stop("a second run".to_owned())
        } else if same && state.thread != Some(thread) {
            Progress::Stop("a run in another thread".to_owned())
        } else {
            state.run = Some(run);
            match phase {
                Phase::Thinking | Phase::Answering => Waiting,
                Phase::Done if !error => {
                    state.thread = Some(thread);
                    Met
                }
                _ => Progress::Stop(format!("the run ended {phase:?}")),
            }
        }),
        (Expect::Quiet, Seen::Key(KeyView::Missing)) => {
            state.missing = true;
            Some(Waiting)
        }
        _ => None,
    };
    progress.unwrap_or_else(|| Progress::Stop(format!("unexpected {seen:?}")))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;

    use kumochi_protocol::message::{ClaudeCodeView, KeyRejection};

    use super::*;
    use crate::scratch;

    const RUN: RunId = RunId(7);
    const THREAD: ThreadId = ThreadId(3);

    fn run(phase: Phase) -> Seen {
        Seen::Run {
            run: RUN,
            thread: THREAD,
            phase,
            error: false,
        }
    }

    /// Plays `seen` against one step and returns each frame's progress.
    fn play(expect: Expect, state: &mut State, seen: &[Seen]) -> Vec<Progress> {
        seen.iter().map(|seen| check(expect, state, seen)).collect()
    }

    fn stop(what: &str) -> Progress {
        Progress::Stop(what.to_owned())
    }

    #[test]
    fn each_launch_gets_exactly_its_slice_a_new_tap_and_its_own_folders() {
        let dir = scratch::dir("anthropic-key-command");
        let folders = Folders::create(&dir).expect("folders");
        for launch in &PLAN {
            let tap = dir.join("tap.bin");
            let command = brain_command(Path::new("kumochid.exe"), &folders, &tap, launch.slice);
            let set = |name: &str| {
                command
                    .get_envs()
                    .find(|(key, _)| *key == OsStr::new(name))
                    .and_then(|(_, value)| value)
                    .map(|value| value.to_string_lossy().into_owned())
            };
            assert_eq!(
                set(LIVE_KEY_CATALOG_BUDGET_ENV),
                Some(launch.slice.catalog.to_string())
            );
            assert_eq!(
                set(LIVE_KEY_MESSAGES_BUDGET_ENV),
                Some(launch.slice.messages.to_string())
            );
            assert_eq!(set(PIPE_TAP_ENV), Some(tap.to_string_lossy().into_owned()));
            assert_eq!(
                set("LOCALAPPDATA"),
                Some(folders.local.to_string_lossy().into_owned())
            );
        }
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn the_shell_and_any_brain_it_starts_get_no_key_budget_or_tap() {
        let dir = scratch::dir("anthropic-key-shell");
        let folders = Folders::create(&dir).expect("folders");
        let command = shell_command(Path::new("kumochi.exe"), &folders);
        let set = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
        };
        assert_eq!(set(LIVE_KEY_CATALOG_BUDGET_ENV), Some(Some("0".to_owned())));
        assert_eq!(
            set(LIVE_KEY_MESSAGES_BUDGET_ENV),
            Some(Some("0".to_owned()))
        );
        assert!(matches!(set(PIPE_TAP_ENV), None | Some(None)));
        assert_eq!(
            set("LOCALAPPDATA"),
            Some(Some(folders.local.to_string_lossy().into_owned()))
        );
        fs::remove_dir_all(dir).expect("cleanup");
    }

    /// Product processes as (pid, parent, shell).
    fn entries(list: &[(u32, u32, bool)]) -> Vec<platform::Entry> {
        list.iter()
            .map(|&(pid, parent, shell)| platform::Entry { pid, parent, shell })
            .collect()
    }

    /// The check is xtask 1; its brain is 10 and its shell 20.
    #[test]
    fn a_launch_is_its_brain_its_shell_and_the_brains_workers_only() {
        let (me, ours) = (1, [10, 20]);
        let with = |extra: &[(u32, u32, bool)]| {
            let mut list = vec![(1, 0, false), (20, 1, true), (10, 1, false)];
            list.extend_from_slice(extra);
            only_ours(&entries(&list), me, ours)
        };
        assert!(with(&[]));
        assert!(with(&[(30, 10, false), (40, 10, false)]), "both workers");
        assert!(
            with(&[(30, 10, false)]),
            "one worker, the other not started or replaced"
        );
        assert!(
            !with(&[(30, 10, false), (40, 10, false), (50, 10, false)]),
            "more workers than the brain has roles"
        );
        assert!(!with(&[(30, 20, false)]), "a brain the shell started");
        assert!(!with(&[(30, 10, true)]), "a shell the brain relaunched");
        assert!(!with(&[(30, 99, false)]), "another brain's worker");
        assert!(!with(&[(30, 1, false)]), "a second brain beside ours");
        assert!(!only_ours(
            &entries(&[(1, 0, false), (10, 1, false), (30, 10, false)]),
            me,
            ours
        ));
        assert!(!only_ours(
            &entries(&[(1, 0, false), (20, 1, true), (30, 10, false)]),
            me,
            ours
        ));
    }

    #[test]
    fn the_ledger_lives_beside_the_gate_run_lock() {
        let ledger = ledger_path().expect("ledger path");
        assert_eq!(
            ledger.parent(),
            Some(platform::local_app_data().expect("LocalAppData").as_path())
        );
        assert!(
            ledger
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-anthropic-key.ledger"))
        );
    }

    #[test]
    fn the_ledger_records_each_grant_and_a_second_run_is_refused_without_touching_it() {
        let dir = scratch::dir("anthropic-key-ledger");
        fs::create_dir_all(&dir).expect("folder");
        let path = dir.join(LEDGER);
        let mut ledger = Ledger::create(&path).expect("first run");
        for (index, launch) in PLAN.iter().enumerate() {
            ledger.grant(index + 1, launch.slice).expect("granted");
        }
        drop(ledger);
        let written = "launch 1 catalog 1 messages 0\nlaunch 2 catalog 2 messages 3\n\
                       launch 3 catalog 0 messages 0\n";
        assert_eq!(fs::read_to_string(&path).expect("ledger"), written);
        let error = Ledger::create(&path).expect_err("a second run");
        assert_eq!(
            format!("{error:#}"),
            format!(
                "{} exists: an earlier run spent its approval, and slices are never refunded; a \
                 new run needs a new approval, after which the owner deletes that file",
                path.display()
            )
        );
        assert_eq!(fs::read_to_string(&path).expect("untouched"), written);
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn a_paste_is_met_only_by_acceptance_after_staging() {
        let mut state = State::default();
        let seen = [
            Seen::Key(KeyView::Ready),
            Seen::Staged,
            Seen::Key(KeyView::Checking),
            Seen::Key(KeyView::Ready),
        ];
        assert_eq!(
            play(Expect::Pasted, &mut state, &seen),
            [
                Progress::Waiting,
                Progress::Waiting,
                Progress::Waiting,
                Progress::Met
            ]
        );
        for wrong in [
            Seen::Key(KeyView::Rejected(KeyRejection::Refused)),
            Seen::Key(KeyView::Unconfirmed),
            run(Phase::Thinking),
            Seen::Refused,
            Seen::Unexpected("Error".to_owned()),
        ] {
            assert!(
                matches!(
                    check(Expect::Pasted, &mut State::default(), &wrong),
                    Progress::Stop(_)
                ),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn an_answer_is_one_run_to_done_and_the_second_stays_in_its_thread() {
        let mut state = State::default();
        let first = [
            run(Phase::Thinking),
            run(Phase::Answering),
            run(Phase::Done),
        ];
        assert_eq!(
            play(Expect::Answered { same: false }, &mut state, &first),
            [Progress::Waiting, Progress::Waiting, Progress::Met]
        );
        assert_eq!(state.thread, Some(THREAD));

        let second = Expect::Answered { same: true };
        let elsewhere = Seen::Run {
            run: RunId(8),
            thread: ThreadId(4),
            phase: Phase::Thinking,
            error: false,
        };
        state.run = None;
        assert_eq!(
            check(second, &mut state, &elsewhere),
            stop("a run in another thread")
        );
        let mut state = State {
            run: Some(RunId(6)),
            thread: Some(THREAD),
            ..State::default()
        };
        assert_eq!(
            check(second, &mut state, &run(Phase::Thinking)),
            stop("a second run")
        );
        for phase in [Phase::Failed, Phase::Interrupted] {
            assert_eq!(
                check(
                    Expect::Answered { same: false },
                    &mut State::default(),
                    &run(phase)
                ),
                Progress::Stop(format!("the run ended {phase:?}"))
            );
        }
        let failed_done = Seen::Run {
            run: RUN,
            thread: THREAD,
            phase: Phase::Done,
            error: true,
        };
        assert_eq!(
            check(
                Expect::Answered { same: false },
                &mut State::default(),
                &failed_done
            ),
            stop("the run ended Done")
        );
        assert!(matches!(
            check(
                Expect::Answered { same: false },
                &mut State::default(),
                &Seen::Staged
            ),
            Progress::Stop(_)
        ));
    }

    #[test]
    fn the_retired_route_must_refuse_and_the_last_launch_must_stay_quiet() {
        for refused in [Seen::Refused, run(Phase::Failed)] {
            assert_eq!(
                check(Expect::Refused, &mut State::default(), &refused),
                Progress::Met
            );
        }
        assert_eq!(
            check(
                Expect::Refused,
                &mut State::default(),
                &run(Phase::Thinking)
            ),
            Progress::Waiting
        );
        for sent in [
            run(Phase::Answering),
            run(Phase::Done),
            Seen::Key(KeyView::Ready),
        ] {
            assert!(matches!(
                check(Expect::Refused, &mut State::default(), &sent),
                Progress::Stop(_)
            ));
        }
        let mut state = State::default();
        assert_eq!(
            check(Expect::Quiet, &mut state, &Seen::Key(KeyView::Missing)),
            Progress::Waiting
        );
        assert!(state.missing);
        for woke in [
            Seen::Key(KeyView::Checking),
            Seen::Key(KeyView::Ready),
            run(Phase::Thinking),
            Seen::Staged,
        ] {
            assert!(matches!(
                check(Expect::Quiet, &mut State::default(), &woke),
                Progress::Stop(_)
            ));
        }
    }

    #[test]
    fn the_tap_yields_only_what_the_plan_watches_and_waits_for_a_cut_record() {
        let dir = scratch::dir("anthropic-key-tap");
        fs::create_dir_all(&dir).expect("folder");
        let path = dir.join("tap.bin");
        assert_eq!(read(&path).expect("missing is empty"), []);
        let shell = |message: &ControlToShell| {
            let mut record =
                Record::of(Pipe::Control, Direction::ToShell, message, 0).expect("record");
            let payload = record.payload.as_ref().map_or(0, Vec::len);
            record.len = u32::try_from(payload).expect("small");
            record
        };
        let bare = |direction, op: &str| Record {
            pipe: Pipe::Control,
            direction,
            op: op.to_owned(),
            len: 0,
            payload: None,
        };
        let records = [
            shell(&ControlToShell::Connections(ConnectionsView {
                anthropic: KeyView::Checking,
                anthropic_revision: None,
                claude_code: ClaudeCodeView::TurnedOff,
            })),
            bare(Direction::ToBrain, "AnthropicKeyStaged"),
            shell(&ControlToShell::Ping),
            shell(&ControlToShell::RunState {
                run: RUN,
                thread: THREAD,
                phase: Phase::Done,
                error: None,
                origin: None,
            }),
            shell(&ControlToShell::TurnRefused {
                turn: 1,
                thread: None,
            }),
            bare(Direction::ToShell, "CaptureDone"),
        ];
        let mut bytes = Vec::new();
        tap::write_header(&mut bytes).expect("header");
        for record in &records {
            tap::write(&mut bytes, record).expect("written");
        }
        let whole = bytes.len();
        tap::write(&mut bytes, &bare(Direction::ToShell, "Error")).expect("written");
        bytes.truncate(whole + 3);
        fs::write(&path, &bytes).expect("tap");
        assert_eq!(
            read(&path).expect("read"),
            [
                Seen::Key(KeyView::Checking),
                Seen::Staged,
                run(Phase::Done),
                Seen::Refused,
                Seen::Unexpected("CaptureDone".to_owned()),
            ]
        );
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
