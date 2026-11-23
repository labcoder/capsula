//! `cargo xtask live claude-question`: three Claude Code questions on the dev brain's own Claude
//! Code sign-in, with no `ChatGPT` or Anthropic key model request possible. In order: discovery
//! alone; a one-line question on a new thread; the title job that question's `Done` starts; and a
//! long question stopped after its first text. Only counts, lengths, durations, yes or no and the
//! brain's state words are printed: never a prompt, an answer, a title, a version or a frame.
//!
//! The run's share of the owner's cap is fixed here and enforced twice: [`budget`] refuses a plan
//! that would ask more than [`CAP`] before anything launches, and the brain gets exactly what the
//! plan asks as `KUMOCHI_CLAUDE_QUESTIONS`, with `KUMOCHI_LIVE_BUDGET` at 0. The first step that
//! fails stops the rest. The exit code names it, and the outer xtask turns it into the most
//! questions the run can have asked.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{CLAUDE_CODE_DIR, DATA_DIR, ENGINE_DIR};
use kumochi_protocol::message::{
    ClaudeCodeView, ClaudeVersion, ConnectionKind, ControlToBrain, ControlToShell, DataToBrain,
    DataToShell, Deleted, Effort, ErrorKey, MAX_TITLE_CHARS, ModelChoice, ModelSlug, PageOf, Phase,
    RunId, RunOrigin, ScreenChoice, SettingsOp, SettingsView, ThreadId, UserError,
};

use super::claude_signin::{FIND, state};
use super::signin::VIEW;
use super::{Brain, joined, refuse_running, with_cleanup};
use crate::platform;
use crate::slice2::client::{Client, Frame, Seen};

/// The most Claude Code questions one run may ask. The owner's cap counts across runs and the
/// Captain keeps that count; this is one run's share.
const CAP: u32 = 3;
/// How long the copied harness may run: discovery, three questions at their bounds below, and
/// the cleanup [guess].
pub(super) const RUN_LIMIT: Duration = Duration::from_mins(12);
/// How long a question may take from its turn to its end: the brain's 120 s from the bind, plus
/// the launch and its audit [guess].
const QUESTION: Duration = Duration::from_secs(150);
/// How long the title may take from the question's end: the job's read, a launch and a question
/// [guess].
const TITLE: Duration = Duration::from_secs(150);
/// How long a stopped run may take to end: the lane's teardown waits a few one-second graces
/// [guess].
const STOPPING: Duration = Duration::from_secs(30);
/// How long the stopped launch may take, after its run ended, to leave no process and no folder
/// [guess].
const SETTLE: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(250);
/// The one Claude Code version the reviewed row admits.
const REVIEWED: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 306,
};
/// Claude Code's default model in the reviewed row; the run asks at `Low`.
const HAIKU: &str = "haiku";
/// The executable a Claude Code launch runs.
const CLAUDE_EXE: &str = "claude.exe";
/// The first question: one line, no user data.
const SHORT: &str = "Reply with one short sentence: what colour is a clear daytime sky?";
/// The question that's stopped: long enough that text arrives well before its end.
const LONG: &str =
    "Count from one to three hundred in words, one number per line, and write nothing else.";
/// The file that keeps the default the run replaced until it's put back, beside the run folders.
const RESTORE: &str = "claude-question.restore";
/// The most entries the Claude Code folder's listing takes [guess].
const MAX_ENTRIES: usize = 10_000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const KINDS: [ConnectionKind; 3] = [
    ConnectionKind::ChatGpt,
    ConnectionKind::AnthropicKey,
    ConnectionKind::ClaudeCode,
];

/// One step of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The budget, the folders, the brain and the pipe.
    Start,
    /// Request 0: Claude Code found ready at the reviewed version.
    Discover,
    /// Claude Code's haiku at low made the default, after a stopped run's is put back.
    Default,
    /// Request 1: a one-line question on a new thread.
    Question,
    /// The title job request 1's `Done` starts.
    Title,
    /// Request 3: a long question, stopped after its first text.
    Stop,
    /// The thread deleted, the default put back, and the folder listed again.
    Cleanup,
}

/// The run, in order.
const PLAN: [Step; 7] = [
    Step::Start,
    Step::Discover,
    Step::Default,
    Step::Question,
    Step::Title,
    Step::Stop,
    Step::Cleanup,
];

impl Step {
    /// The Claude Code questions the step asks.
    const fn questions(self) -> u32 {
        match self {
            Self::Question | Self::Title | Self::Stop => 1,
            Self::Start | Self::Discover | Self::Default | Self::Cleanup => 0,
        }
    }

    /// The copied harness's exit code when the step fails.
    const fn code(self) -> u8 {
        match self {
            Self::Start => 10,
            Self::Discover => 11,
            Self::Default => 12,
            Self::Question => 13,
            Self::Title => 14,
            Self::Stop => 15,
            Self::Cleanup => 16,
        }
    }

    const fn words(self) -> &'static str {
        match self {
            Self::Start => "starting",
            Self::Discover => "finding claude code",
            Self::Default => "setting the default",
            Self::Question => "the question",
            Self::Title => "the title",
            Self::Stop => "the stop",
            Self::Cleanup => "cleaning up",
        }
    }
}

/// The questions `plan` asks; more than [`CAP`] is refused.
fn budget(plan: &[Step]) -> Result<u32> {
    let asked = plan.iter().map(|step| step.questions()).sum::<u32>();
    ensure!(
        asked <= CAP,
        "the run would ask Claude Code {asked} questions, more than its {CAP}; nothing was started"
    );
    Ok(asked)
}

/// What the outer xtask checks before it builds anything: the plan's budget, then that no
/// product process runs.
pub(super) fn preflight(running: &[platform::Entry], me: u32) -> Result<u32> {
    let asked = budget(&PLAN)?;
    refuse_running(running, me)?;
    Ok(asked)
}

/// The line the outer xtask prints for the copied harness's exit code, and whether the run
/// passed. A failed step counts every question up to and including its own, since its question
/// may have gone out before it failed. A failed question also counts the title: the brain starts
/// the title job in the step that reports `Done`, and the question can fail after that.
fn verdict(code: Option<i32>) -> (String, bool) {
    let all = PLAN.iter().map(|step| step.questions()).sum::<u32>();
    let Some(code) = code else {
        return (
            format!("claude question: no exit code (questions: at most {all})"),
            false,
        );
    };
    if code == 0 {
        return (format!("claude question: done (questions: {all})"), true);
    }
    let line = match PLAN.iter().position(|step| i32::from(step.code()) == code) {
        Some(at) => {
            let last = if PLAN[at] == Step::Question {
                at + 1
            } else {
                at
            };
            format!(
                "claude question: stopped at {}, exit {code} (questions: at most {})",
                PLAN[at].words(),
                PLAN[..=last]
                    .iter()
                    .map(|step| step.questions())
                    .sum::<u32>()
            )
        }
        None => format!("claude question: exit {code} (questions: at most {all})"),
    };
    (line, false)
}

/// What the outer xtask prints once the copied harness ended, however it ended: the verdict, with
/// no exit code when it timed out or couldn't be waited for, then a line when the restore file is
/// still there. Fails unless the run passed.
pub(super) fn report(ran: Result<Option<i32>>, live: &Path, out: &mut impl Write) -> Result<()> {
    let (code, failure) = match ran {
        Ok(code) => (code, None),
        Err(error) => (None, Some(error)),
    };
    let (verdict, done) = verdict(code);
    writeln!(out, "{verdict}")?;
    let restore = live.join(RESTORE);
    if restore.exists() {
        match fs::read_to_string(&restore).map(|text| parse_choice(&text)) {
            Ok(Ok(old)) => writeln!(
                out,
                "default: may still be claude code's haiku at low; the next run from this \
                 checkout puts back {} first",
                format_choice(&old).trim_end()
            )?,
            _ => writeln!(
                out,
                "default: may still be claude code's haiku at low, and {} can't be read; set \
                 the default in Settings and delete it",
                restore.display()
            )?,
        }
    }
    if let Some(failure) = failure {
        return Err(failure);
    }
    ensure!(done, "live claude-question failed");
    Ok(())
}

/// The step that failed, and why.
#[derive(Debug)]
struct Stopped {
    step: Step,
    error: anyhow::Error,
}

trait At<T> {
    fn at(self, step: Step) -> Result<T, Stopped>;
}

impl<T> At<T> for Result<T> {
    fn at(self, step: Step) -> Result<T, Stopped> {
        self.map_err(|error| Stopped { step, error })
    }
}

/// The copied harness: runs the plan and returns its exit code.
pub(super) fn inner() -> i32 {
    let mut out = io::stdout().lock();
    let code = match run(&mut out) {
        Ok(()) => 0,
        Err(Stopped { step, error }) => {
            let _best_effort = writeln!(out, "stopped at {}: {error:#}", step.words());
            step.code()
        }
    };
    let _best_effort = out.flush();
    i32::from(code)
}

fn run(out: &mut impl Write) -> Result<(), Stopped> {
    let asked = budget(&PLAN).at(Step::Start)?;
    let places = Places::find().at(Step::Start)?;
    let brain = Brain::start(0, asked).at(Step::Start)?;
    let pid = brain.child.id();
    let Ok(mut client) = Client::connect() else {
        let error = joined(anyhow!("the brain didn't accept the client"), brain.stop());
        return Err(Stopped {
            step: Step::Start,
            error,
        });
    };
    let driven = drive(
        &mut Session {
            client: &mut client,
            claude: None,
            titled: false,
            turns: 0,
            pages: 0,
        },
        out,
        pid,
        &places,
    );
    let cleanup = if client.control(&ControlToBrain::Quitting).is_ok() {
        brain.quit()
    } else {
        Err(joined(
            anyhow!("couldn't send Quitting; the brain was ended"),
            brain.stop(),
        ))
    };
    match driven {
        Ok(()) => cleanup.at(Step::Cleanup),
        Err(Stopped { step, error }) => Err(Stopped {
            step,
            error: joined(error, cleanup),
        }),
    }
}

/// The pipe, and what the brain said on it that later steps need.
struct Session<'a> {
    client: &'a mut Client,
    /// Claude Code's latest view, printed when a step fails.
    claude: Option<ClaudeCodeView>,
    /// A `ThreadsChanged` arrived: the brain committed a title.
    titled: bool,
    /// `TextSubmit`s sent on this connection.
    turns: u32,
    /// Page requests sent on this connection.
    pages: u32,
}

impl Session<'_> {
    /// Takes frames until `pick` chooses one, within `limit`; `None` past it. Every frame updates
    /// what the session tracks before `pick` sees it.
    fn next<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&Frame) -> Option<T>,
    ) -> Result<Option<T>> {
        let (claude, titled) = (&mut self.claude, &mut self.titled);
        let picked = self.client.until(limit, |seen| {
            match &seen.frame {
                Frame::Control(ControlToShell::Connections(view)) => {
                    *claude = Some(view.claude_code);
                }
                Frame::Control(ControlToShell::ThreadsChanged) => *titled = true,
                Frame::Closed => return Some(None),
                _ => {}
            }
            pick(&seen.frame).map(Some)
        });
        // As in `signin`: Closed is picked first, so an error means the deadline, and its text,
        // which holds a Debug tail of frames, is dropped.
        match picked {
            Ok(Some(picked)) => Ok(Some(picked)),
            Ok(None) => bail!("the brain closed the pipe"),
            Err(_deadline) => Ok(None),
        }
    }

    /// The frame `pick` chooses within `limit`, or a failure naming `what`.
    fn expect<T>(
        &mut self,
        limit: Duration,
        what: &str,
        pick: impl FnMut(&Frame) -> Option<T>,
    ) -> Result<T> {
        self.next(limit, pick)?
            .ok_or_else(|| anyhow!("no {what} in {limit:?}"))
    }

    fn control(&self, message: &ControlToBrain, what: &str) -> Result<()> {
        self.client
            .control(message)
            .map(drop)
            .map_err(|_pipe| anyhow!("couldn't send {what} to the brain"))
    }

    fn data(&self, message: &DataToBrain, what: &str) -> Result<()> {
        self.client
            .data(message)
            .map(drop)
            .map_err(|_pipe| anyhow!("couldn't send {what} to the brain"))
    }
}

fn drive(
    session: &mut Session<'_>,
    out: &mut impl Write,
    brain: u32,
    places: &Places,
) -> Result<(), Stopped> {
    let settings = session
        .expect(VIEW, "settings", |frame| match frame {
            Frame::Control(ControlToShell::Settings { view, .. }) => Some(view.clone()),
            _ => None,
        })
        .at(Step::Start)?;
    discover(session, out).at(Step::Discover)?;
    let before = places.listing().at(Step::Discover)?;
    let replaced = default(session, out, &settings, places).at(Step::Default)?;
    let mut thread = None;
    let proved = proofs(session, out, brain, places, &mut thread).map_err(|stopped| Stopped {
        step: stopped.step,
        error: anyhow!(
            "{:#}; claude code is {}",
            stopped.error,
            session.claude.map_or_else(|| "unknown".to_owned(), state)
        ),
    });
    let deleted = thread.map_or(Ok(()), |thread| delete(session, out, thread));
    let restored = replaced.map_or(Ok(()), |old| put_back(session, out, &old, places));
    let listed = places.listing().and_then(|after| {
        changes(&before, &after)
            .iter()
            .try_for_each(|line| writeln!(out, "{line}"))
            .map_err(Into::into)
    });
    let cleaned = with_cleanup(with_cleanup(deleted, restored), listed);
    match proved {
        Ok(()) => cleaned.at(Step::Cleanup),
        Err(Stopped { step, error }) => Err(Stopped {
            step,
            error: joined(error, cleaned),
        }),
    }
}

/// Requests 1 to 3, in order; the first that fails stops the rest. `thread` is the run's thread
/// once the brain names it, so the cleanup deletes it however the run ends.
fn proofs(
    session: &mut Session<'_>,
    out: &mut impl Write,
    brain: u32,
    places: &Places,
    thread: &mut Option<ThreadId>,
) -> Result<(), Stopped> {
    let made = question(session, out, thread).at(Step::Question)?;
    title(session, out, made).at(Step::Title)?;
    stop(session, out, made, brain, places).at(Step::Stop)
}

/// Request 0: waits through `NotFound` and `Finding` for discovery to settle, and passes only on
/// `Ready` at the reviewed version.
fn discover(session: &mut Session<'_>, out: &mut impl Write) -> Result<()> {
    let start = Instant::now();
    let deadline = start + FIND;
    let view = loop {
        match session.claude {
            Some(view) if !matches!(view, ClaudeCodeView::NotFound | ClaudeCodeView::Finding) => {
                break view;
            }
            _ => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let seen = session.next(left, |frame| {
            matches!(frame, Frame::Control(ControlToShell::Connections(_))).then_some(())
        })?;
        if seen.is_none() {
            break session.claude.unwrap_or(ClaudeCodeView::NotFound);
        }
    };
    writeln!(out, "claude code: {}", state(view))?;
    writeln!(out, "discovery: {} ms", start.elapsed().as_millis())?;
    let ClaudeCodeView::Ready { version, .. } = view else {
        bail!("claude code isn't ready; nothing was asked");
    };
    let reviewed = version == REVIEWED;
    writeln!(
        out,
        "at the reviewed version 2.1.306: {}",
        if reviewed { "yes" } else { "no" }
    )?;
    ensure!(
        reviewed,
        "claude code is ready at another version; nothing was asked"
    );
    Ok(())
}

/// Makes Claude Code's haiku at low the default for the run, after putting back the one an
/// earlier run left. Returns the default to put back when it changed; the restore file holds it
/// until then.
fn default(
    session: &mut Session<'_>,
    out: &mut impl Write,
    settings: &SettingsView,
    places: &Places,
) -> Result<Option<ModelChoice>> {
    ensure!(
        !settings.talk_back,
        "turn talk-back off first: the check plays no speech, so a spoken reply would never end"
    );
    let mut current = settings.default_model.clone();
    if let Some(left) = places.left_default()? {
        set_default(session, &left)?;
        places.forget_default()?;
        writeln!(out, "default: put back the one an earlier run left")?;
        current = Some(left);
    }
    let haiku = ModelChoice {
        model: ModelSlug::new(HAIKU)?,
        effort: Effort::Low,
        connection: ConnectionKind::ClaudeCode,
    };
    let old = current.context("the profile has no default model to put back afterwards")?;
    if old == haiku {
        writeln!(out, "default: already claude code's haiku at low")?;
        return Ok(None);
    }
    places.keep_default(&old)?;
    let set = set_default(session, &haiku).and_then(|()| {
        writeln!(
            out,
            "default: claude code's haiku at low for this run, replacing {}",
            format_choice(&old).trim_end()
        )
        .map_err(Into::into)
    });
    if let Err(error) = set {
        return Err(joined(error, put_back(session, out, &old, places)));
    }
    Ok(Some(old))
}

fn set_default(session: &mut Session<'_>, choice: &ModelChoice) -> Result<()> {
    session.control(
        &ControlToBrain::Settings(SettingsOp::SetDefaultModel(choice.clone())),
        "SetDefaultModel",
    )?;
    let view = session.expect(VIEW, "answer to SetDefaultModel", |frame| match frame {
        Frame::Control(ControlToShell::Settings { view, answer: true }) => Some(view.clone()),
        _ => None,
    })?;
    ensure!(
        view.default_model.as_ref() == Some(choice),
        "the brain didn't take the default"
    );
    Ok(())
}

fn put_back(
    session: &mut Session<'_>,
    out: &mut impl Write,
    old: &ModelChoice,
    places: &Places,
) -> Result<()> {
    set_default(session, old)?;
    places.forget_default()?;
    writeln!(out, "default: put back")?;
    Ok(())
}

/// Request 1: a one-line question on a new thread. It passes when text arrives, no tool frame
/// does, and the run ends `Done`.
fn question(
    session: &mut Session<'_>,
    out: &mut impl Write,
    thread: &mut Option<ThreadId>,
) -> Result<ThreadId> {
    let start = Instant::now();
    let (run, made, mut answer) = submit(session, None, SHORT)?;
    *thread = Some(made);
    session.titled = false;
    collect(session, run, &mut answer, QUESTION, Until::End)?;
    writeln!(
        out,
        "question: {}, text {} characters in {} chunks, tool frames {}, {} ms",
        ended(&answer),
        answer.chars,
        answer.chunks,
        answer.tools,
        start.elapsed().as_millis()
    )?;
    let audit = answer.error == Some(ErrorKey::ClaudeCodeAuditFailed);
    if audit_view_due(&answer, session.claude) {
        session.next(VIEW, |frame| {
            matches!(frame, Frame::Control(ControlToShell::Connections(view))
                if matches!(view.claude_code, ClaudeCodeView::AuditFailed(_)))
            .then_some(())
        })?;
    }
    ensure!(
        answer.phase == Some(Phase::Done),
        "the question ended {}{}",
        ended(&answer),
        if audit {
            ", its phase in the brain's log"
        } else {
            ""
        }
    );
    ensure!(answer.chars > 0, "the question ended done with no text");
    ensure!(answer.tools == 0, "the question sent tool frames");
    writeln!(
        out,
        "claude code took --effort, and a launch with no tools passed its audit: yes"
    )?;
    Ok(made)
}

/// Whether the `AuditFailed` view that names the failed check is still due: the brain sends it
/// after the run's failure, so a failure line built at once would show the view before it.
fn audit_view_due(answer: &Answer, claude: Option<ClaudeCodeView>) -> bool {
    answer.error == Some(ErrorKey::ClaudeCodeAuditFailed)
        && !matches!(claude, Some(ClaudeCodeView::AuditFailed(_)))
}

/// The title job request 1's `Done` started. It passes when the brain commits a title, which it
/// does only for an answer within the job's output cap that `clean_title` turned into a line.
fn title(session: &mut Session<'_>, out: &mut impl Write, thread: ThreadId) -> Result<()> {
    let start = Instant::now();
    if !session.titled {
        session.next(TITLE, |frame| {
            matches!(frame, Frame::Control(ControlToShell::ThreadsChanged)).then_some(())
        })?;
    }
    ensure!(session.titled, "no title in {TITLE:?}");
    session.pages += 1;
    let request = session.pages;
    session.data(
        &DataToBrain::PageRequest {
            request,
            page: PageOf::Threads {
                search: None,
                after: None,
            },
        },
        "the thread list request",
    )?;
    let chars = session.expect(VIEW, "thread list", |frame| match frame {
        Frame::Data(
            DataToShell::ThreadList {
                request: of,
                threads,
                ..
            },
            _,
        ) if *of == request => Some(
            threads
                .iter()
                .find(|row| row.id == thread)
                .map(|row| row.title.chars().count()),
        ),
        Frame::Data(DataToShell::PageFailed { request: of }, _) if *of == request => Some(None),
        _ => None,
    })?;
    let chars = chars.context("the thread list's first page doesn't hold the run's thread")?;
    writeln!(
        out,
        "title: set, {chars} characters, {} ms",
        start.elapsed().as_millis()
    )?;
    ensure!(
        (1..=MAX_TITLE_CHARS).contains(&chars),
        "the title has {chars} characters"
    );
    writeln!(
        out,
        "title within the job's output cap and through clean_title: yes"
    )?;
    Ok(())
}

/// Request 3: a long question on the same thread, stopped after its first text. It passes when
/// the run ends `Interrupted` and, within [`SETTLE`], no `claude.exe` runs under the brain and
/// no launch folder is left under `engine\`.
fn stop(
    session: &mut Session<'_>,
    out: &mut impl Write,
    thread: ThreadId,
    brain: u32,
    places: &Places,
) -> Result<()> {
    let (run, _, mut answer) = submit(session, Some(thread), LONG)?;
    collect(session, run, &mut answer, QUESTION, Until::Text)?;
    ensure!(
        answer.phase.is_none(),
        "the long question ended {} before any text",
        ended(&answer)
    );
    let stopped = Instant::now();
    session.control(
        &ControlToBrain::Stop {
            run,
            revealed: Vec::new(),
        },
        "Stop",
    )?;
    session.control(
        &ControlToBrain::SpeechStopped { run, cursor: None },
        "SpeechStopped",
    )?;
    collect(session, run, &mut answer, STOPPING, Until::End)?;
    let ms = stopped.elapsed().as_millis();
    let (processes, folders) = settle(brain, places)?;
    writeln!(
        out,
        "stop: {} after {} characters, {ms} ms; claude processes left {processes}, launch \
         folders left {folders}",
        ended(&answer),
        answer.chars
    )?;
    ensure!(
        answer.phase == Some(Phase::Interrupted),
        "the stopped question ended {}",
        ended(&answer)
    );
    ensure!(
        processes == 0 && folders == 0,
        "the stopped launch left {processes} processes and {folders} folders after {SETTLE:?}"
    );
    Ok(())
}

/// Sends a turn and returns its run, its thread and what came with the run's first state.
fn submit(
    session: &mut Session<'_>,
    thread: Option<ThreadId>,
    text: &str,
) -> Result<(RunId, ThreadId, Answer)> {
    session.turns += 1;
    let turn = session.turns;
    session.data(
        &DataToBrain::TextSubmit {
            turn,
            screen: ScreenChoice::None,
            thread,
            text: text.into(),
        },
        "the turn",
    )?;
    let start = session.client.log.len();
    let named = session.expect(VIEW, "run for the turn", |frame| match frame {
        Frame::Control(ControlToShell::RunState {
            run,
            thread,
            origin: Some(RunOrigin::Typed { turn: named }),
            ..
        }) if *named == turn => Some(Some((*run, *thread))),
        Frame::Control(ControlToShell::TurnRefused { turn: refused, .. }) if *refused == turn => {
            Some(None)
        }
        _ => None,
    })?;
    let (run, thread) = named.context("the brain refused the turn")?;
    Ok((run, thread, taken(run, &session.client.log, start)))
}

/// What the wait for the run's name took of the run, from `start` in the log: the pipes aren't
/// ordered with each other, so its text and `RunEnd` can come before the `RunState` naming it.
fn taken(run: RunId, log: &[Seen], start: usize) -> Answer {
    let mut answer = Answer::default();
    for seen in &log[start..] {
        count(&mut answer, run, &seen.frame);
    }
    answer
}

/// What came back for one run; the text is counted, never kept.
#[derive(Debug, Default)]
struct Answer {
    /// Its terminal phase, once it arrived.
    phase: Option<Phase>,
    /// Why it failed, when the brain said.
    error: Option<ErrorKey>,
    chunks: u32,
    chars: usize,
    /// Frames only a tool call makes: steps and focus moves.
    tools: u32,
    /// Its `RunEnd` arrived.
    data_ended: bool,
}

fn terminal(phase: Phase) -> bool {
    matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed)
}

/// How far [`collect`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Until {
    /// The run's first text, or its terminal phase.
    Text,
    /// The run's terminal phase and its `RunEnd`.
    End,
}

/// Reads `run`'s frames into `answer` until `until`, within `limit`.
fn collect(
    session: &mut Session<'_>,
    run: RunId,
    answer: &mut Answer,
    limit: Duration,
    until: Until,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        let enough = match until {
            Until::Text => answer.chunks > 0 || answer.phase.is_some(),
            Until::End => answer.phase.is_some() && answer.data_ended,
        };
        if enough {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let seen = session.next(left, |frame| count(answer, run, frame).then_some(()))?;
        if seen.is_none() {
            bail!(
                "the run didn't {} in {limit:?}",
                match until {
                    Until::Text => "send text",
                    Until::End => "end",
                }
            );
        }
    }
}

/// Adds `frame` to `answer` when it's one of `run`'s; says whether it was.
fn count(answer: &mut Answer, run: RunId, frame: &Frame) -> bool {
    match frame {
        Frame::Data(DataToShell::AssistantText { run: of, delta }, _) if *of == run => {
            answer.chunks += 1;
            answer.chars += delta.as_str().chars().count();
        }
        Frame::Data(DataToShell::RunEnd { run: of }, _) if *of == run => {
            answer.data_ended = true;
        }
        Frame::Data(
            DataToShell::StepCommit { run: of, .. } | DataToShell::SceneStepCommit { run: of, .. },
            _,
        )
        | Frame::Control(ControlToShell::Focus { run: of, .. })
            if *of == run =>
        {
            answer.tools += 1;
        }
        Frame::Control(ControlToShell::RunState {
            run: of,
            phase,
            error,
            ..
        }) if *of == run && terminal(*phase) => {
            answer.phase = Some(*phase);
            answer.error = error.as_ref().map(UserError::key);
        }
        _ => return false,
    }
    true
}

/// The run's end in words: its phase, and the brain's reason when it failed with one.
fn ended(answer: &Answer) -> String {
    match (answer.phase, answer.error) {
        (None, _) => "unended".to_owned(),
        (Some(Phase::Failed), Some(key)) => format!("failed ({key:?})"),
        (Some(phase), _) => format!("{phase:?}").to_lowercase(),
    }
}

/// Waits up to [`SETTLE`] for the brain's tree to hold no `claude.exe` and `engine\` to hold no
/// launch folder; returns how many of each are left.
fn settle(brain: u32, places: &Places) -> Result<(usize, usize)> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let left = (
            claude_left(&platform::running()?, brain),
            places.launches()?,
        );
        if left == (0, 0) || Instant::now() >= deadline {
            return Ok(left);
        }
        thread::sleep(POLL);
    }
}

/// How many `claude.exe` run under `brain`, at any depth.
fn claude_left(running: &[platform::Running], brain: u32) -> usize {
    let mut tree = vec![brain];
    let mut grew = true;
    while grew {
        grew = false;
        for process in running {
            if !tree.contains(&process.pid) && tree.contains(&process.parent) {
                tree.push(process.pid);
                grew = true;
            }
        }
    }
    running
        .iter()
        .filter(|process| {
            process.pid != brain
                && tree.contains(&process.pid)
                && process.name.eq_ignore_ascii_case(CLAUDE_EXE)
        })
        .count()
}

fn delete(session: &mut Session<'_>, out: &mut impl Write, thread: ThreadId) -> Result<()> {
    session.control(
        &ControlToBrain::DeleteThread {
            thread,
            after_turns: session.turns,
            resend: false,
        },
        "DeleteThread",
    )?;
    let outcome = session.expect(VIEW, "answer to DeleteThread", |frame| match frame {
        Frame::Control(ControlToShell::ThreadDeleted {
            thread: of,
            outcome,
            ..
        }) if *of == thread => Some(*outcome),
        _ => None,
    })?;
    let words = match outcome {
        Deleted::NotHidden => "not deleted",
        Deleted::Hidden => "hidden, removed at the next start",
        Deleted::Removed => "deleted",
    };
    writeln!(out, "thread: {words}")?;
    ensure!(
        outcome != Deleted::NotHidden,
        "the brain didn't delete the run's thread"
    );
    Ok(())
}

/// The folders and the file the check looks at.
#[derive(Debug)]
struct Places {
    /// Kumochi's Claude Code folder, listed by name and size.
    claude_code: PathBuf,
    /// The launch folders' parent.
    engine: PathBuf,
    /// Where the default the run replaced is kept until it's put back.
    restore: PathBuf,
}

impl Places {
    fn find() -> Result<Self> {
        let data = platform::local_app_data()?.join(DATA_DIR);
        let me = std::env::current_exe()?;
        let live = me
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("the harness runs in target\\live\\<time>\\bin")?;
        Ok(Self {
            claude_code: data.join(CLAUDE_CODE_DIR),
            engine: data.join(ENGINE_DIR),
            restore: live.join(RESTORE),
        })
    }

    /// How many entries `engine\` holds; none when it's missing.
    fn launches(&self) -> Result<usize> {
        match fs::read_dir(&self.engine) {
            Ok(entries) => Ok(entries.count()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error).context("listing the launch folders"),
        }
    }

    fn listing(&self) -> Result<BTreeMap<String, u64>> {
        listing(&self.claude_code)
    }

    /// The default an earlier run replaced and didn't put back.
    fn left_default(&self) -> Result<Option<ModelChoice>> {
        match fs::read_to_string(&self.restore) {
            Ok(text) => parse_choice(&text).map(Some).with_context(|| {
                format!(
                    "set the default in Settings and delete {}",
                    self.restore.display()
                )
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("reading the restore file"),
        }
    }

    fn keep_default(&self, choice: &ModelChoice) -> Result<()> {
        fs::write(&self.restore, format_choice(choice)).context("writing the restore file")
    }

    fn forget_default(&self) -> Result<()> {
        match fs::remove_file(&self.restore) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("deleting the restore file"),
        }
    }
}

/// Every entry under `root` by its path relative to it, folders with a trailing `\` and files
/// with their size. Only names and metadata are read, never a file's contents; a reparse point
/// is listed but not followed.
fn listing(root: &Path) -> Result<BTreeMap<String, u64>> {
    let mut found = BTreeMap::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound && folder == root => {
                return Ok(found);
            }
            Err(error) => return Err(error).context("listing claude code's folder"),
        };
        for entry in entries {
            let path = entry.context("listing claude code's folder")?.path();
            let meta = fs::symlink_metadata(&path).context("reading an entry's metadata")?;
            let name = path.strip_prefix(root)?.display().to_string();
            if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                found.insert(format!("{name}\\"), 0);
                folders.push(path);
            } else {
                found.insert(name, meta.len());
            }
            ensure!(
                found.len() <= MAX_ENTRIES,
                "claude code's folder holds more than {MAX_ENTRIES} entries"
            );
        }
    }
    Ok(found)
}

/// What changed between two listings, one line each, then the counts.
fn changes(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, size) in after {
        match before.get(name) {
            None => lines.push(format!("claude code folder: added {name} ({size} bytes)")),
            Some(old) if old != size => lines.push(format!(
                "claude code folder: changed {name} ({old} to {size} bytes)"
            )),
            Some(_) => {}
        }
    }
    for name in before.keys().filter(|name| !after.contains_key(*name)) {
        lines.push(format!("claude code folder: removed {name}"));
    }
    lines.push(format!(
        "claude code folder: {} entries before, {} after",
        before.len(),
        after.len()
    ));
    lines
}

fn effort_word(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

fn kind_word(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::ChatGpt => "chatgpt",
        ConnectionKind::AnthropicKey => "anthropic-key",
        ConnectionKind::ClaudeCode => "claude-code",
    }
}

/// The restore file's one line: the model, its effort and its connection.
fn format_choice(choice: &ModelChoice) -> String {
    format!(
        "{} {} {}\n",
        choice.model.as_str(),
        effort_word(choice.effort),
        kind_word(choice.connection)
    )
}

fn parse_choice(text: &str) -> Result<ModelChoice> {
    let mut words = text.split_whitespace();
    let (Some(model), Some(effort), Some(kind), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        bail!("the restore file isn't a model, an effort and a connection");
    };
    Ok(ModelChoice {
        model: ModelSlug::new(model)
            .map_err(|_slug| anyhow!("the restore file's model isn't a slug"))?,
        effort: Effort::ALL
            .into_iter()
            .find(|known| effort_word(*known) == effort)
            .context("the restore file's effort isn't one")?,
        connection: KINDS
            .into_iter()
            .find(|known| kind_word(*known) == kind)
            .context("the restore file's connection isn't one")?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use kumochi_protocol::message::{
        AuditFailed, ClaudeCodeView, ConnectionKind, ControlToShell, DataToShell, Effort, ErrorKey,
        ModelChoice, ModelSlug, Phase, RunId, RunOrigin, ThreadId,
    };

    use super::{
        Answer, CAP, KINDS, PLAN, RESTORE, REVIEWED, Step, audit_view_due, budget, changes,
        claude_left, count, format_choice, parse_choice, report, taken, verdict,
    };
    use crate::slice2::client::{Frame, Seen};
    use crate::{platform, scratch};

    #[test]
    fn a_question_the_audit_failed_waits_for_the_view_that_names_the_check() {
        let failed = |error| Answer {
            phase: Some(Phase::Failed),
            error: Some(error),
            ..Answer::default()
        };
        let ready = ClaudeCodeView::Ready {
            version: REVIEWED,
            analytics_on: false,
        };
        let audit = failed(ErrorKey::ClaudeCodeAuditFailed);
        assert!(audit_view_due(&audit, Some(ready)));
        assert!(audit_view_due(&audit, None));
        assert!(!audit_view_due(
            &audit,
            Some(ClaudeCodeView::AuditFailed(AuditFailed::McpServer))
        ));
        assert!(!audit_view_due(
            &failed(ErrorKey::ClaudeCodeSignedOut),
            Some(ready)
        ));
        assert!(!audit_view_due(&Answer::default(), Some(ready)));
    }

    #[test]
    fn text_that_comes_before_the_runs_name_counts_toward_its_answer() {
        let run = RunId(7);
        let state = |phase| {
            Frame::Control(ControlToShell::RunState {
                run,
                thread: ThreadId(3),
                phase,
                error: None,
                origin: Some(RunOrigin::Typed { turn: 2 }),
            })
        };
        let text = |run, delta: &str| {
            Frame::Data(
                DataToShell::AssistantText {
                    run,
                    delta: delta.to_owned().into(),
                },
                40,
            )
        };
        let log: Vec<Seen> = [
            text(RunId(6), "older"),
            text(run, "hello"),
            state(Phase::Thinking),
        ]
        .into_iter()
        .map(|frame| Seen {
            at: Instant::now(),
            frame,
        })
        .collect();
        let mut answer = taken(run, &log, 1);
        assert!(count(&mut answer, run, &state(Phase::Done)));
        assert!(count(
            &mut answer,
            run,
            &Frame::Data(DataToShell::RunEnd { run }, 20)
        ));
        assert_eq!(
            (answer.chunks, answer.chars, answer.tools),
            (1, 5, 0),
            "the text before the name is counted"
        );
        assert_eq!(answer.phase, Some(Phase::Done));
        assert!(answer.data_ended);
    }

    #[test]
    fn the_plan_asks_its_cap_and_a_plan_over_it_is_refused_before_anything_starts() {
        assert_eq!(budget(&PLAN).expect("within the cap"), 3);
        assert_eq!(CAP, 3);
        let asking: Vec<Step> = PLAN
            .into_iter()
            .filter(|step| step.questions() > 0)
            .collect();
        assert_eq!(asking, [Step::Question, Step::Title, Step::Stop]);
        let mut over = PLAN.to_vec();
        over.insert(4, Step::Question);
        let error = budget(&over).expect_err("four questions");
        assert_eq!(
            format!("{error:#}"),
            "the run would ask Claude Code 4 questions, more than its 3; nothing was started"
        );
        assert_eq!(budget(&[]).expect("nothing asked"), 0);
    }

    #[test]
    fn each_exit_code_names_its_step_and_the_most_questions_it_can_have_asked() {
        let line = |code| verdict(code).0;
        assert_eq!(
            verdict(Some(0)),
            ("claude question: done (questions: 3)".to_owned(), true)
        );
        for (code, step, most) in [
            (10, "starting", 0),
            (11, "finding claude code", 0),
            (12, "setting the default", 0),
            (13, "the question", 2),
            (14, "the title", 2),
            (15, "the stop", 3),
            (16, "cleaning up", 3),
        ] {
            assert_eq!(
                verdict(Some(code)),
                (
                    format!(
                        "claude question: stopped at {step}, exit {code} (questions: at most \
                         {most})"
                    ),
                    false
                )
            );
        }
        assert_eq!(
            line(Some(1)),
            "claude question: exit 1 (questions: at most 3)"
        );
        assert_eq!(
            line(Some(101)),
            "claude question: exit 101 (questions: at most 3)"
        );
        assert_eq!(
            line(None),
            "claude question: no exit code (questions: at most 3)"
        );
        let codes: Vec<u8> = PLAN.into_iter().map(Step::code).collect();
        assert!(
            codes.windows(2).all(|pair| pair[0] < pair[1]),
            "distinct and in plan order: {codes:?}"
        );
        assert!(
            codes.iter().all(|code| *code > 1),
            "never 0 or a plain failure: {codes:?}"
        );
    }

    #[test]
    fn a_harness_that_timed_out_still_prints_its_verdict() {
        let live = scratch::dir("question-timeout");
        let mut out = Vec::new();
        let error = report(
            Err(anyhow::anyhow!("the live run didn't finish in 720s")),
            &live,
            &mut out,
        )
        .expect_err("timed out");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "claude question: no exit code (questions: at most 3)\n"
        );
        assert_eq!(format!("{error:#}"), "the live run didn't finish in 720s");
        let mut out = Vec::new();
        report(Ok(Some(0)), &live, &mut out).expect("passed");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "claude question: done (questions: 3)\n"
        );
    }

    #[test]
    fn a_restore_file_left_behind_is_named_after_the_verdict() {
        let live = scratch::dir("question-restore");
        let restore = live.join(RESTORE);
        std::fs::write(&restore, "gpt-6.1-sol low chatgpt\n").expect("planted");
        let has_code = |line: &str, code: &str| {
            line.split(|ch: char| !ch.is_ascii_digit())
                .any(|field| field == code)
        };
        for (ran, code) in [
            (Ok(Some(15)), Some("15")),
            (Ok(None), None),
            (Err(anyhow::anyhow!("killed")), None),
        ] {
            let mut out = Vec::new();
            report(ran, &live, &mut out).expect_err("failed");
            let text = String::from_utf8(out).expect("utf-8");
            let (received, warning) = text.split_once('\n').expect("verdict before recovery");
            let names_exit = code.map_or_else(
                || received.contains("no exit code"),
                |code| has_code(received, code),
            );
            assert!(names_exit && received.contains("at most 3"), "{received}");
            assert_eq!(
                warning,
                "default: may still be claude code's haiku at low; the next run from this \
                 checkout puts back gpt-6.1-sol low chatgpt first\n"
            );
        }
        std::fs::write(&restore, "garbage").expect("planted");
        let mut out = Vec::new();
        report(Ok(Some(12)), &live, &mut out).expect_err("failed");
        let text = String::from_utf8(out).expect("utf-8");
        let (received, warning) = text.split_once('\n').expect("verdict before recovery");
        assert!(
            has_code(received, "12") && received.contains("at most 0"),
            "{received}"
        );
        assert!(
            warning.contains(&restore.display().to_string())
                && warning.contains("Settings")
                && (warning.contains("delete") || warning.contains("remove")),
            "{warning}"
        );
    }

    #[test]
    fn the_restore_file_round_trips_every_choice_and_refuses_anything_else() {
        for connection in KINDS {
            for effort in Effort::ALL {
                let choice = ModelChoice {
                    model: ModelSlug::new("gpt-6.1-sol").expect("a slug"),
                    effort,
                    connection,
                };
                assert_eq!(
                    parse_choice(&format_choice(&choice)).expect("round trips"),
                    choice
                );
            }
        }
        assert_eq!(
            format_choice(&ModelChoice {
                model: ModelSlug::new("haiku").expect("a slug"),
                effort: Effort::Low,
                connection: ConnectionKind::ClaudeCode,
            }),
            "haiku low claude-code\n"
        );
        let shape = "the restore file isn't a model, an effort and a connection";
        for (text, error) in [
            ("", shape),
            ("haiku low", shape),
            ("haiku low claude-code extra", shape),
            (
                "Haiku low claude-code",
                "the restore file's model isn't a slug",
            ),
            (
                "haiku Low claude-code",
                "the restore file's effort isn't one",
            ),
            (
                "haiku low claude",
                "the restore file's connection isn't one",
            ),
        ] {
            assert_eq!(
                format!("{:#}", parse_choice(text).expect_err("refused")),
                error,
                "{text:?}"
            );
        }
    }

    #[test]
    fn only_claude_under_the_brain_counts_at_any_depth() {
        let process = |pid, parent, name: &str| platform::Running {
            pid,
            parent,
            name: name.to_owned(),
        };
        let running = [
            process(10, 4, "kumochid.exe"),
            process(11, 10, "claude.exe"),
            process(12, 11, "CLAUDE.EXE"),
            process(13, 10, "conhost.exe"),
            process(14, 13, "claude.exe"),
            process(20, 1, "claude.exe"),
            process(21, 20, "claude.exe"),
            process(30, 31, "claude.exe"),
            process(31, 30, "claude.exe"),
        ];
        assert_eq!(claude_left(&running, 10), 3);
        assert_eq!(claude_left(&running, 99), 0);
        assert_eq!(claude_left(&running[..1], 10), 0);
        assert_eq!(claude_left(&running, 20), 1);
    }

    #[test]
    fn the_listing_reports_added_changed_and_removed_entries_then_counts() {
        let map = |entries: &[(&str, u64)]| {
            entries
                .iter()
                .map(|(name, size)| ((*name).to_owned(), *size))
                .collect::<BTreeMap<_, _>>()
        };
        let before = map(&[("config\\", 0), ("config\\a.json", 5), ("gone", 1)]);
        let after = map(&[
            ("config\\", 0),
            ("config\\a.json", 9),
            ("config\\sessions\\", 0),
            ("config\\sessions\\1.json", 2),
        ]);
        assert_eq!(
            changes(&before, &after),
            [
                "claude code folder: changed config\\a.json (5 to 9 bytes)",
                "claude code folder: added config\\sessions\\ (0 bytes)",
                "claude code folder: added config\\sessions\\1.json (2 bytes)",
                "claude code folder: removed gone",
                "claude code folder: 3 entries before, 4 after",
            ]
        );
        assert_eq!(
            changes(&after, &after),
            ["claude code folder: 4 entries before, 4 after"]
        );
    }
}
