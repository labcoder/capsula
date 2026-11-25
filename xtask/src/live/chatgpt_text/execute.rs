//! First text on the ordinary Dev brain, with an owned child and bounded evidence.

mod ledger;
#[cfg(test)]
mod tests;

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{PAIR, PLAN, Startup, plan};
use crate::slice2::client::{Client, Frame, Seen};
use anyhow::{Result, anyhow, bail, ensure};
use kumochi_protocol::constants::{
    COMPACT_AT_ENV, LIVE_CHATGPT_TEXT_ENV, LIVE_CHATGPT_TEXT_STAMPS_FILE, LIVE_RUN_BIN_DIR,
    LIVE_RUNS_DIR, PIPE_TAP_ENV, SHELL_EXE,
};
use kumochi_protocol::message::{
    AccountView, ClaudeCodeView, ConnectionKind, ControlToBrain, ControlToShell, DataToBrain,
    DataToShell, Effort, GitHubView, KeyView, ModelChoice, ModelSlug, Phase, PlanState, RunId,
    RunOrigin, ScreenChoice, SettingsOp, ThreadId,
};

const TARGETS: [&str; 2] = ["gpt-6-luna", "gpt-6.1-sol"];
const WAIT: Duration = Duration::from_secs(30);
const CLAIM: &str = "chatgpt-text.started";

pub(in crate::live) fn run(budget: u32) -> Result<()> {
    let exe = std::env::current_exe().map_err(|_| anyhow!("no copied executable"))?;
    let folder = run_folder(&exe).ok_or_else(|| anyhow!("run the prepared Dev copy"))?;
    claim_checked(
        folder,
        || plan(budget),
        || {
            let mut outcomes = Outcomes::default();
            let result = observe(folder, budget, &mut outcomes);
            match &result {
                Ok(receipt) => println!("{receipt}"),
                Err(_) => println!("{}", outcomes.invalid_receipt(budget)),
            }
            result.map(drop)
        },
    )
}

fn claim_checked<T>(
    folder: &Path,
    inspect: impl FnOnce() -> Result<Startup>,
    claimed: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let claim = folder.join(CLAIM);
    ensure!(!claim.exists(), "run folder was already claimed");
    let plan = std::fs::File::open(folder.join(PLAN))
        .and_then(|file| std::io::read_to_string(file.take(256)))
        .map_err(|_| anyhow!("missing prepared plan"))?;
    let startup = inspect()?;
    ensure!(
        plan == startup.plan,
        "budget or startup work differs from the prepared plan"
    );
    let pair = std::fs::read(folder.join(PAIR)).map_err(|_| anyhow!("missing prepared pair"))?;
    ensure!(
        pair == startup.pair,
        "the profile folders differ from the prepared ones"
    );
    ensure!(
        !folder.join(LIVE_CHATGPT_TEXT_STAMPS_FILE).exists(),
        "run folder was already used"
    );
    std::fs::File::create_new(claim).map_err(|_| anyhow!("cannot take a new run claim"))?;
    claimed()
}

fn run_folder(exe: &Path) -> Option<&Path> {
    let bin = exe.parent()?;
    let folder = bin.parent()?;
    let number = folder.file_name()?.to_str()?;
    (exe.file_name()? == SHELL_EXE
        && bin.file_name()? == LIVE_RUN_BIN_DIR
        && folder.parent()?.file_name()? == LIVE_RUNS_DIR
        && number.bytes().all(|b| b.is_ascii_digit())
        && number.parse::<u64>().is_ok())
    .then_some(folder)
}

struct Outcomes {
    work: &'static str,
    restore: &'static str,
    exit: &'static str,
}

impl Default for Outcomes {
    fn default() -> Self {
        Self {
            work: "not-started",
            restore: "not-needed",
            exit: "not-started",
        }
    }
}

impl Outcomes {
    fn invalid_receipt(&self, budget: u32) -> String {
        format!(
            r#"{{"status":"invalid","model_charge":{budget},"auxiliary":null,"work":"{}","restore":"{}","exit":"{}"}}"#,
            self.work, self.restore, self.exit
        )
    }
}

fn observe(folder: &Path, budget: u32, outcomes: &mut Outcomes) -> Result<String> {
    let brain = super::super::Brain::start_with(budget, 0, |cmd| {
        cmd.env(LIVE_CHATGPT_TEXT_ENV, "1")
            .env_remove(PIPE_TAP_ENV)
            .env_remove(COMPACT_AT_ENV);
    })
    .map_err(|_| anyhow!("could not start the owned Dev brain"))?;
    outcomes.exit = "unconfirmed";
    let pid = brain.child.id();
    let end = Instant::now() + super::super::RUN_LIMIT;
    let work_by = end
        .checked_sub(super::super::EXIT + WAIT)
        .ok_or_else(|| anyhow!("invalid run deadline"))?;
    let file = folder.join(LIVE_CHATGPT_TEXT_STAMPS_FILE);
    let client = Client::project(pid, end, projector())
        .map_err(|_| anyhow!("owned pipes did not authenticate"))?;
    let ready = ledger::read(&file, pid, budget).and_then(|receipt| {
        ensure!(
            !receipt.finished && receipt.attempts.is_empty() && receipt.auxiliary == [0; 5],
            "unexpected ready state"
        );
        let until = (Instant::now() + WAIT).min(work_by);
        readiness(|| Ok(client.next_fact(until)?.frame))
    });
    let samples = drive(
        ready,
        budget,
        work_by,
        outcomes,
        |main, current, choice, until, eligible| {
            set(&client, until, main, current, choice, eligible)
        },
        |main, turn, target, eligible| turn_sample(&client, work_by, main, turn, target, eligible),
        || quit(brain, &client, end),
    )?;
    report(&samples, &ledger::read(&file, pid, budget)?, outcomes)
}

fn quit(brain: super::super::Brain, client: &Pipe, end: Instant) -> Result<()> {
    let quitting = client.control(&ControlToBrain::Quitting);
    let cleanup = if quitting.is_ok() {
        brain.quit()
    } else {
        brain.stop()
    };
    let sent = quitting.map_err(|_| anyhow!("quit was not sent"))?;
    cleanup.map_err(|_| anyhow!("an orderly owned exit was not confirmed"))?;
    client.finish_projected(sent, end)
}

type Choice = (usize, Effort);
type Pipe = Client<Option<Fact>>;
type Ready = (ThreadId, Option<Choice>, [bool; 2]);

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
enum Fact {
    Ready(u8),
    Catalog([bool; 2]),
    Setting(ThreadId, Option<Choice>, bool),
    Refused,
    Unavailable,
    TurnRefused(u32, Option<ThreadId>),
    Named(u32, RunId, ThreadId),
    Terminal(RunId, ThreadId, bool),
    Text(RunId),
    End(RunId),
}

fn projector() -> impl FnMut(Frame) -> std::result::Result<Option<Fact>, ()> {
    let mut ended = RunId(0);
    move |frame| {
        Ok(Some(match frame {
            Frame::Control(frame) => match frame {
                ControlToShell::Account(AccountView::SignedIn {
                    plan: PlanState::Enabled,
                    recovery: None,
                    ..
                }) => Fact::Ready(1),
                ControlToShell::Connections(view)
                    if view.anthropic == KeyView::Missing
                        && view.claude_code == ClaudeCodeView::TurnedOff =>
                {
                    Fact::Ready(2)
                }
                ControlToShell::GitHub(GitHubView::SignedOut { cleanup: None }) => Fact::Ready(4),
                ControlToShell::Settings { view, answer }
                    if !view.screen_holds
                        && !view.show_screen
                        && !view.hold_to_talk
                        && !view.talk_back =>
                {
                    let selected = view
                        .default_model
                        .filter(|choice| choice.connection == ConnectionKind::ChatGpt)
                        .and_then(|choice| {
                            TARGETS
                                .iter()
                                .position(|slug| *slug == choice.model.as_str())
                                .map(|target| (target, choice.effort))
                        });
                    Fact::Setting(view.main_thread, selected, answer)
                }
                ControlToShell::RunState {
                    run,
                    thread,
                    origin,
                    phase,
                    ..
                } => match (phase, origin) {
                    (Phase::Thinking, Some(RunOrigin::Typed { turn })) => {
                        Fact::Named(turn, run, thread)
                    }
                    (Phase::Answering, None) => return Ok(None),
                    (Phase::Done, None) => Fact::Terminal(run, thread, true),
                    (Phase::Failed, None) => Fact::Terminal(run, thread, false),
                    _ => return Err(()),
                },
                ControlToShell::TurnRefused { turn, thread } => Fact::TurnRefused(turn, thread),
                ControlToShell::Error(_) => Fact::Refused,
                ControlToShell::Account(_)
                | ControlToShell::Connections(_)
                | ControlToShell::GitHub(_) => Fact::Unavailable,
                ControlToShell::Settings { .. } => return Err(()),
                _ => return Ok(None),
            },
            Frame::Data(frame, _) => match frame {
                DataToShell::Models {
                    connection: ConnectionKind::ChatGpt,
                    models,
                } => Fact::Catalog(TARGETS.map(|slug| {
                    models
                        .iter()
                        .find(|row| row.slug.as_str() == slug)
                        .and_then(|row| row.supported_efforts)
                        .is_some_and(|set| set.contains(Effort::Low))
                })),
                DataToShell::AssistantText { run, delta } if run > ended => {
                    if delta.as_str().is_empty() {
                        return Ok(None);
                    }
                    Fact::Text(run)
                }
                DataToShell::RunEnd { run } if run > ended => {
                    ended = run;
                    Fact::End(run)
                }
                DataToShell::AssistantText { .. }
                | DataToShell::RunEnd { .. }
                | DataToShell::StepCommit { .. } => return Err(()),
                _ => return Ok(None),
            },
            Frame::Closed | Frame::BadRead => return Err(()),
        }))
    }
}

fn drive(
    ready: Result<Ready>,
    budget: u32,
    deadline: Instant,
    outcomes: &mut Outcomes,
    mut select: impl FnMut(ThreadId, Choice, Choice, Instant, &mut [bool; 2]) -> Result<Option<Choice>>,
    mut submit: impl FnMut(ThreadId, u32, usize, &mut [bool; 2]) -> Result<Option<Sample>>,
    quit: impl FnOnce() -> Result<()>,
) -> Result<Vec<Sample>> {
    let mut out = Vec::new();
    let mut restore = None;
    let mut confirmed = None;
    let work = (|| {
        let (main, saved, known) = ready?;
        let count = 10 * known.into_iter().map(u32::from).sum::<u32>();
        ensure!(
            count <= budget,
            "budget cannot fit the advertised Low targets"
        );
        let Some(saved) = saved else {
            return Ok(());
        };
        let mut current = saved;
        let mut eligible = known;
        for target in (0..TARGETS.len()).filter(|&target| known[target]) {
            ensure!(eligible[target], "target is no longer eligible");
            let wanted = (target, Effort::Low);
            if wanted != current {
                restore = Some((main, saved));
                confirmed = None;
                confirmed = select(main, current, wanted, deadline, &mut eligible)?;
                if confirmed == Some(saved) {
                    restore = None;
                }
                ensure!(confirmed == Some(wanted), "setting was not accepted");
                current = wanted;
            }
            for _ in 0..10 {
                ensure!(eligible[target], "target is no longer eligible");
                let turn = u32::try_from(out.len())? + 1;
                let sample =
                    submit(main, turn, target, &mut eligible).inspect_err(|_| confirmed = None)?;
                out.push(sample.ok_or_else(|| anyhow!("typed turn failed or was refused"))?);
            }
        }
        Ok(())
    })();
    outcomes.work = if work.is_ok() { "completed" } else { "failed" };
    if let Some((main, saved)) = restore {
        if let Some(current) = confirmed {
            outcomes.restore = match select(main, current, saved, deadline + WAIT, &mut [false; 2])
            {
                Ok(active) if active == Some(saved) => "confirmed",
                Ok(_) => "unconfirmed",
                Err(_) => "unknown",
            };
        } else {
            outcomes.restore = "unknown";
        }
    }
    let exited = quit();
    if exited.is_ok() {
        outcomes.exit = "confirmed";
    } else if outcomes.restore == "confirmed" {
        outcomes.restore = "unknown";
    }
    work?;
    ensure!(
        matches!(outcomes.restore, "not-needed" | "confirmed"),
        "settings restoration not confirmed"
    );
    exited?;
    Ok(out)
}

fn readiness(mut read: impl FnMut() -> Result<Fact>) -> Result<Ready> {
    let (mut ready, mut settings, mut catalog) = (0, None, None);
    loop {
        match read()? {
            Fact::Ready(n) => ready |= n,
            Fact::Setting(main, saved, false) => settings = Some((main, saved)),
            Fact::Catalog(known) => catalog = Some(known),
            _ => bail!("disposable profile not ready"),
        }
        if let (7, Some((main, saved)), Some(known)) = (ready, settings, catalog)
            && (saved.is_some() || known == [false; 2])
        {
            return Ok((main, saved, known));
        }
    }
}

fn set(
    client: &Pipe,
    deadline: Instant,
    main: ThreadId,
    current: Choice,
    selected: Choice,
    eligible: &mut [bool; 2],
) -> Result<Option<Choice>> {
    ensure!(Instant::now() < deadline, "setting deadline expired");
    let (target, effort) = selected;
    let model = ModelChoice {
        connection: ConnectionKind::ChatGpt,
        model: ModelSlug::new(TARGETS[target])?,
        effort,
    };
    let sent = client
        .control(&ControlToBrain::Settings(SettingsOp::SetDefaultModel(
            model,
        )))
        .map_err(|_| anyhow!("setting write failed"))?;
    let until = (sent + WAIT).min(deadline);
    confirm_setting(main, current, selected, sent, eligible, || {
        client.next_fact(until)
    })
}

fn confirm_setting(
    main: ThreadId,
    current: Choice,
    wanted: Choice,
    sent: Instant,
    eligible: &mut [bool; 2],
    mut read: impl FnMut() -> Result<Seen<Fact>>,
) -> Result<Option<Choice>> {
    let mut refused = false;
    loop {
        let Seen { at, frame } = read()?;
        if frame.update_eligibility(eligible) {
            continue;
        }
        if at < sent {
            return Ok(None);
        }
        match frame {
            Fact::Refused if !refused => refused = true,
            Fact::Setting(thread, choice, true) if thread == main => {
                let expected = if refused { current } else { wanted };
                return Ok(choice.filter(|&choice| choice == expected));
            }
            _ => return Ok(None),
        }
    }
}

impl Fact {
    fn update_eligibility(self, eligible: &mut [bool; 2]) -> bool {
        match self {
            Self::Ready(_) => {}
            Self::Unavailable => *eligible = [false; 2],
            Self::Catalog(offered) => {
                for (allowed, low) in eligible.iter_mut().zip(offered) {
                    *allowed &= low;
                }
            }
            _ => return false,
        }
        true
    }
}

struct Sample {
    run: RunId,
    target: usize,
    pipe_us: u128,
}

fn turn_sample(
    client: &Pipe,
    end: Instant,
    main: ThreadId,
    turn: u32,
    target: usize,
    eligible: &mut [bool; 2],
) -> Result<Option<Sample>> {
    ensure!(Instant::now() < end, "turn deadline expired");
    let sent = client
        .data(&DataToBrain::TextSubmit {
            turn,
            thread: Some(main),
            screen: ScreenChoice::None,
            text: "Reply only with the word ready.".into(),
        })
        .map_err(|_| anyhow!("typed submit failed"))?;
    let until = (sent + WAIT).min(end);
    join_turn(main, turn, target, sent, eligible, || {
        client.next_fact(until)
    })
}

fn join_turn(
    main: ThreadId,
    turn: u32,
    target: usize,
    sent: Instant,
    eligible: &mut [bool; 2],
    mut read: impl FnMut() -> Result<Seen<Fact>>,
) -> Result<Option<Sample>> {
    let (mut run, mut first, mut satisfied) = (None, None, 0);
    let mut refused = false;
    while satisfied != 7 {
        let Seen { at, frame } = read()?;
        if frame.update_eligibility(eligible) {
            refused |= !eligible[target];
            continue;
        }
        ensure!(at >= sent, "turn observation predates submit");
        let (id, flag) = match frame {
            Fact::Named(number, id, thread) if number == turn && thread == main => (id, 1),
            Fact::Terminal(id, thread, done) if thread == main && satisfied & 1 != 0 => {
                refused |= !done;
                (id, 2)
            }
            Fact::TurnRefused(number, Some(thread))
                if number == turn && thread == main && run.is_none() =>
            {
                return Ok(None);
            }
            Fact::Refused => {
                refused = true;
                continue;
            }
            Fact::End(id) => (id, 4),
            Fact::Text(id) if satisfied & 4 == 0 => {
                first.get_or_insert(at);
                (id, 0)
            }
            _ => bail!("unexpected turn observation"),
        };
        ensure!(
            id.0 != 0 && *run.get_or_insert(id) == id && satisfied & flag == 0,
            "wrong or duplicate run"
        );
        satisfied |= flag;
    }
    if refused {
        return Ok(None);
    }
    let first = first
        .and_then(|at| at.checked_duration_since(sent))
        .ok_or_else(|| anyhow!("no first text after submit"))?;
    Ok(Some(Sample {
        run: run.ok_or_else(|| anyhow!("no admitted run"))?,
        target,
        pipe_us: first.as_micros(),
    }))
}

fn report(samples: &[Sample], ledger: &ledger::Ledger, outcomes: &Outcomes) -> Result<String> {
    ensure!(
        ledger.finished && ledger.attempts.len() == samples.len(),
        "incomplete or extra reservation"
    );
    let mut targets = [Vec::new(), Vec::new()];
    for (sample, attempt) in samples.iter().zip(&ledger.attempts) {
        ensure!(
            sample.run == attempt.run && u64::try_from(sample.target + 1)? == attempt.target,
            "reservation does not match typed turn"
        );
        let ticks = attempt.text.ok_or_else(|| anyhow!("missing parser mark"))? - attempt.reserved;
        targets[sample.target].push((sample.pipe_us, u128::from(ticks)));
    }
    ensure!(
        targets
            .iter()
            .all(|samples| matches!(samples.len(), 0 | 10)),
        "incomplete target"
    );
    let [luna, sol] = targets.map(|samples| {
        if samples.is_empty() { return r#"{"status":"not-run","completed":0}"#.to_owned(); }
        let percentiles = |column: usize| {
            let mut values: Vec<_> = samples.iter().map(|&(a, b)| [a, b][column]).collect();
            values.sort_unstable();
            [values[4], values[9], values[9]]
        };
        format!(r#"{{"status":"completed","completed":10,"pipe_us_p50_p95_max":{:?},"dispatch_qpc_ticks_p50_p95_max":{:?}}}"#,
            percentiles(0), percentiles(1))
    });
    Ok(format!(
        r#"{{"status":"completed","model_charge":{},"auxiliary":{:?},"targets":[{luna},{sol}],"work":"{}","restore":"{}","exit":"{}"}}"#,
        samples.len(),
        ledger.auxiliary,
        outcomes.work,
        outcomes.restore,
        outcomes.exit
    ))
}
