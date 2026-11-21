//! C5b-1b through the core, on the lane's fake in `replay` mode: the warm slot, `take`, the
//! round, and each attempt's one report. Every positive fixture is synthetic (§8); no network, no
//! account and no credential store.

use std::fs;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{CLAUDE_CODE_QUARANTINE, ENGINE_DIR, MCP_SERVER_NAME};
use kumochi_protocol::message::{ModelChoice, PolicyFailed, RunId};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::tests::READY;
use super::*;
use crate::context::Origin;
use crate::coordinator::{Input, LeaseId, SendKey};
use crate::engines::claude_code::lane::Host;
use crate::engines::claude_code::lane::tests::{Base, V, host, soon};
use crate::engines::claude_code::supervisor::tests::{WAIT, until};
use crate::engines::{
    self, Answer, Asks, Engine, EngineFailure, Finish, RunRequest, SendGate, SendLease, TurnEvent,
};
use crate::platform::Job;
use crate::test_support::{Captured, SENTINEL, Scratch};

mod one_turn;
mod pictures;

const FACTS: &str = include_str!("../../../fixtures/claude-code/facts.jsonl");
const TEXT: &str = include_str!("../../../fixtures/claude-code/text.jsonl");

fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect()
}

/// What the fake answers each stdin line with, by kind.
#[derive(Clone)]
struct Script {
    initialize: Vec<String>,
    mcp_status: Vec<String>,
    /// The answers to the asks after the first, one each, the last repeating.
    mcp_again: Vec<String>,
    user: Vec<Value>,
}

/// P0's recordings made synthetic and signed in (§8). The edits: Kumochi's request ids; 2.1.306's
/// signed-in `account` shape with made-up values; the fixed draw-only MCP tool set in
/// `mcp_status` and init; and init's `apiKeySource` set to `none`.
fn synthetic_signed_in() -> Script {
    let facts = lines(FACTS);
    let mut initialize = facts[0].clone();
    initialize["response"]["request_id"] = json!("k-initialize");
    initialize["response"]["response"]["account"] =
        json!({"subscriptionType": "max", "apiProvider": "firstParty"});
    let mut mcp = facts[1].clone();
    mcp["response"]["request_id"] = json!("k-mcp-status");
    mcp["response"]["response"]["mcpServers"][0]["tools"] = json!([{"name":"draw"}]);
    let mut user = lines(TEXT);
    user[0]["tools"] = json!([format!("mcp__{MCP_SERVER_NAME}__draw")]);
    user[0]["apiKeySource"] = json!("none");
    Script {
        initialize: vec![initialize.to_string()],
        mcp_status: vec![mcp.to_string()],
        mcp_again: Vec::new(),
        user,
    }
}

impl Script {
    /// The first `mcp_status` answer reads `pending` with no tools, as 2.1.306's does while
    /// Kumochi's server connects; `again` holds the answers to the asks after it.
    fn pending_first(mut self, again: Vec<String>) -> Self {
        let mut line: Value = serde_json::from_str(&self.mcp_status[0]).expect("json");
        let server = &mut line["response"]["response"]["mcpServers"][0];
        server["status"] = json!("pending");
        server.as_object_mut().expect("object").remove("tools");
        self.mcp_status[0] = line.to_string();
        self.mcp_again = again;
        self
    }

    /// The control answers wait until the base holds `name`.
    fn held_until(mut self, name: &str) -> Self {
        self.mcp_status.insert(0, format!("!wait {name}"));
        self
    }

    fn initialize(mut self, edit: impl FnOnce(&mut Value)) -> Self {
        let mut line: Value = serde_json::from_str(&self.initialize[0]).expect("json");
        edit(&mut line["response"]["response"]);
        self.initialize[0] = line.to_string();
        self
    }

    fn init(mut self, edit: impl FnOnce(&mut Value)) -> Self {
        edit(&mut self.user[0]);
        self
    }

    fn pre_tools(mut self, names: &[&str]) -> Self {
        let mut mcp: Value = serde_json::from_str(&self.mcp_status[0]).expect("json");
        mcp["response"]["response"]["mcpServers"][0]["tools"] = json!(
            names
                .iter()
                .map(|name| json!({"name":name}))
                .collect::<Vec<_>>()
        );
        self.mcp_status[0] = mcp.to_string();
        self
    }

    fn post_tools(self, names: &[&str]) -> Self {
        self.init(|init| {
            init["tools"] = json!(
                names
                    .iter()
                    .map(|name| format!("mcp__{MCP_SERVER_NAME}__{name}"))
                    .collect::<Vec<_>>()
            );
        })
    }

    /// The seed's answer waits until the base holds `name`; edit init before this.
    fn held_at_user(mut self, name: &str) -> Self {
        self.user.insert(0, Value::String(format!("!wait {name}")));
        self
    }

    /// Written as the base's script, or as the Nth question's.
    fn write(&self, base: &Base, question: Option<usize>) {
        let user: Vec<String> = self
            .user
            .iter()
            .map(|line| {
                line.as_str()
                    .map_or_else(|| line.to_string(), str::to_owned)
            })
            .collect();
        let again = self
            .mcp_again
            .iter()
            .map(|line| format!("@mcp_status\n{line}\n"))
            .collect::<Vec<_>>()
            .concat();
        let text = format!(
            "@initialize\n{}\n@mcp_status\n{}\n{again}@user\n{}\n",
            self.initialize.join("\n"),
            self.mcp_status.join("\n"),
            user.join("\n")
        );
        let name = question.map_or_else(|| "replay".to_owned(), |n| format!("replay.{n}"));
        fs::write(base.0.join(name), text).expect("the script");
    }
}

/// The recorded answer's events, read from the fixture.
fn deltas() -> Vec<TurnEvent> {
    lines(TEXT)
        .iter()
        .filter_map(|line| line["event"]["delta"]["text"].as_str())
        .map(|text| TurnEvent::TextDelta(text.to_owned()))
        .collect()
}

/// A connection Ready on the fake, with a clock the test sets.
struct Rig {
    claude: Claude,
    base: Base,
    t0: Instant,
    now: Arc<Mutex<Instant>>,
    /// Every report the core heard, in order.
    ended: Vec<(LaunchId, launch::Outcome)>,
    refreshes: usize,
    /// Every launch a round asked to authorize, which the rig's core always does.
    binds: Vec<LaunchId>,
}

impl Rig {
    async fn new(name: &str, script: &Script) -> Self {
        Self::with(name, script, |_| {}).await
    }

    /// As [`new`](Self::new), on a host `edit` changed first.
    async fn with(name: &str, script: &Script, edit: impl FnOnce(&mut Host)) -> Self {
        let base = Base::new(name);
        script.write(&base, None);
        let (mut host, _switches) = host(&base, "replay");
        edit(&mut host);
        let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
        let t0 = Instant::now();
        let now = Arc::new(Mutex::new(t0));
        let read = Arc::clone(&now);
        claude.clock = Clock(Arc::new(move || {
            *read.lock().unwrap_or_else(PoisonError::into_inner)
        }));
        let mut rig = Self {
            claude,
            base,
            t0,
            now,
            ended: Vec::new(),
            refreshes: 0,
            binds: Vec::new(),
        };
        rig.settle().await;
        assert_eq!(rig.claude.view(), READY);
        rig
    }

    fn at(&self, after: Duration) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) = self.t0 + after;
    }

    fn snapshot(&self) -> LaunchSnapshot {
        self.claude
            .snapshot(
                ThreadId(1),
                "claude-test".to_owned(),
                None,
                Offer::for_tests(Origin::Kumo, vec![crate::engines::ToolName::Draw]),
            )
            .expect("ready")
    }

    fn last(&self) -> (LaunchId, Arc<Control>) {
        let held = self.claude.launches.last().expect("a launch");
        (held.id, Arc::clone(&held.control))
    }

    fn wake(&mut self, wake: Wake) {
        if let Wake::Note(Note::ProviderFailed {
            id,
            key,
            until,
            signed_out,
        }) = wake
        {
            self.claude
                .recheck_failed(id, key, Some(Lifetime::new(until)), signed_out);
            return;
        }
        if let Wake::Note(Note::Bind(bind)) = wake {
            self.binds.push(bind.launch);
            bind.reply.send(Some(bind.until)).expect("the round waits");
            return;
        }
        match &wake {
            Wake::Note(Note::Ended { id, outcome }) => self.ended.push((*id, *outcome)),
            Wake::Note(Note::Refresh(_)) => self.refreshes += 1,
            _ => {}
        }
        self.claude.wake(wake);
    }

    /// Authorizes the round's launch as the core would, taking only its bind note, for a test
    /// that orders every other note itself.
    async fn authorize(&mut self) {
        let note = timeout(WAIT, self.claude.notes.1.recv())
            .await
            .expect("a note in time")
            .expect("notes open");
        let Note::Bind(bind) = note else {
            panic!("{note:?}");
        };
        self.binds.push(bind.launch);
        bind.reply.send(Some(bind.until)).expect("the round waits");
    }

    async fn settle(&mut self) {
        timeout(Duration::from_secs(30), async {
            while self.claude.out.is_some() {
                let wake = self.claude.next().await;
                self.wake(wake);
            }
        })
        .await
        .expect("discovery settles");
    }

    /// Runs `work` while the core takes what wakes it.
    async fn drive<T>(&mut self, work: impl Future<Output = T>) -> T {
        timeout(Duration::from_secs(30), async {
            tokio::pin!(work);
            loop {
                let wake = tokio::select! {
                    out = &mut work => return out,
                    wake = self.claude.next() => wake,
                };
                self.wake(wake);
            }
        })
        .await
        .expect("in time")
    }

    /// Takes what wakes the core until `id` reported, then whatever is queued.
    async fn reported(&mut self, id: LaunchId) -> Vec<launch::Outcome> {
        timeout(Duration::from_secs(30), async {
            while !self.ended.iter().any(|(ended, _)| *ended == id) {
                let wake = self.claude.next().await;
                self.wake(wake);
            }
        })
        .await
        .expect("reported");
        while let Ok(note) = self.claude.notes.1.try_recv() {
            self.wake(Wake::Note(note));
        }
        self.ended
            .iter()
            .filter(|(ended, _)| *ended == id)
            .map(|(_, outcome)| *outcome)
            .collect()
    }

    fn stdin(&self) -> Vec<String> {
        fs::read_to_string(self.base.0.join("stdin.log"))
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The Nth question's argv, as the fake recorded it.
    fn argv(&self, question: usize) -> String {
        fs::read_to_string(self.base.0.join("starts.log"))
            .expect("starts")
            .lines()
            .filter(|start| start.starts_with("-p "))
            .nth(question - 1)
            .and_then(|start| start.split('\t').next())
            .expect("the question's start")
            .to_owned()
    }

    /// The first text block of the Nth question's seed.
    fn seed(&self, question: usize) -> String {
        fs::read_to_string(self.base.0.join(format!("seed.{question}"))).expect("a seed")
    }

    fn markers(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.base.0.path())
            .expect("the base")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(CLAUDE_CODE_QUARANTINE))
            .collect();
        names.sort();
        names
    }

    fn go(&self, name: &str) {
        fs::write(self.base.0.join(name), b"").expect("go");
    }

    async fn close(mut self) {
        assert!(self.claude.close(soon()).await);
    }
}

/// One round of a taken question, as the coordinator would run it.
struct Played {
    key: SendKey,
    asks: Asks,
    inputs_tx: mpsc::UnboundedSender<Input>,
    inputs: mpsc::UnboundedReceiver<Input>,
    events: mpsc::Receiver<TurnEvent>,
    round: JoinHandle<()>,
}

fn play(question: &ClaudeQuestion) -> Played {
    play_with(
        question,
        RunRequest::for_tests(RunId(1), ThreadId(1), "Be brief."),
    )
}

fn play_with(question: &ClaudeQuestion, request: RunRequest) -> Played {
    let key = request.key;
    let asks = Asks::default();
    let (inputs_tx, inputs) = mpsc::unbounded_channel();
    let (events_tx, events) = mpsc::channel(64);
    let gate = SendGate::new(key, Arc::clone(&asks), inputs_tx.clone());
    let round = tokio::spawn(question.start(request, events_tx, gate));
    Played {
        key,
        asks,
        inputs_tx,
        inputs,
        events,
        round,
    }
}

impl Played {
    /// A grant for its ask.
    fn lease(&self) -> Answer {
        Answer::Grant(SendLease::new(
            LeaseId {
                key: self.key,
                send: 1,
            },
            self.inputs_tx.clone(),
        ))
    }

    /// The round's ask, answered with `answer`; the hold it asked for.
    async fn asked(&mut self, answer: Answer) -> Duration {
        let Some(Input::SendReady { key, hold }) = self.inputs.recv().await else {
            panic!("an ask");
        };
        assert_eq!(key, self.key);
        engines::answer(&self.asks, self.key, answer);
        hold.expect("a held send")
    }

    /// Every event until the round ends.
    async fn events(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.events.recv().await {
            events.push(event);
        }
        events
    }
}

fn claude_failed(key: ErrorKey) -> TurnEvent {
    TurnEvent::Finished(Finish::Failed(EngineFailure::Claude(key.into())))
}

/// The answer's text, then its usage, then Completed.
fn answered(events: &[TurnEvent]) {
    let [
        text @ ..,
        TurnEvent::Usage(_),
        TurnEvent::Finished(Finish::Completed),
    ] = events
    else {
        panic!("{events:?}");
    };
    assert_eq!(text, deltas());
}

#[tokio::test]
async fn a_taken_question_is_audited_seeded_and_answered_with_one_report() {
    let mut rig = Rig::new("question-answered", &synthetic_signed_in()).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    let hold = rig.drive(played.asked(lease)).await;
    assert!(
        hold > Duration::from_secs(160) && hold <= Duration::from_secs(165),
        "the launch's deadline plus SETTLE: {hold:?}"
    );
    answered(&rig.drive(played.events()).await);
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status", "1 user"]);
    assert_eq!(rig.binds, [id], "authorized before its seed");
    assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
    assert_eq!(
        rig.base
            .calls()
            .iter()
            .filter(|call| *call == "auth status --json")
            .count(),
        1,
        "a successful question does not recheck status"
    );
    assert_eq!(
        rig.claude.view(),
        READY,
        "init's analytics_disabled is true"
    );
    rig.close().await;
}

fn provider_failure() -> Script {
    let mut script = synthetic_signed_in().init(|init| init["analytics_disabled"] = json!(false));
    script.user.truncate(1);
    script
        .user
        .push(json!({"type": "result", "subtype": "provider_error", "is_error": true}));
    script
}

async fn provider_failed(rig: &mut Rig) -> Note {
    timeout(WAIT, async {
        loop {
            let note = rig.claude.notes.1.recv().await.unwrap();
            if matches!(note, Note::ProviderFailed { .. }) {
                return note;
            }
            rig.wake(Wake::Note(note));
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_recheck_that_expires_in_preflight_never_starts_status() {
    let armed = Arc::new(AtomicBool::new(false));
    let hold = Arc::clone(&armed);
    let (reached, mut waiting) = mpsc::unbounded_channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let mut rig = Rig::with(
        "question-recheck-no-late-spawn",
        &provider_failure(),
        |host| {
            let managed = std::mem::replace(
                &mut host.managed,
                Box::new(|| unreachable!("replaced below")),
            );
            host.managed = Box::new(move || {
                if hold.swap(false, Ordering::SeqCst) {
                    reached.send(()).unwrap();
                    blocked.recv_timeout(WAIT).unwrap();
                }
                managed()
            });
        },
    )
    .await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let failure = provider_failed(&mut rig).await;
    armed.store(true, Ordering::SeqCst);
    rig.wake(Wake::Note(failure));
    assert_eq!(timeout(WAIT, waiting.recv()).await.unwrap(), Some(()));
    rig.at(MAX_LAUNCH);
    release.send(()).unwrap();
    rig.settle().await;
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(
        rig.base
            .calls()
            .iter()
            .filter(|call| *call == "auth status --json")
            .count(),
        1,
        "only discovery started status"
    );
    rig.close().await;
}

#[tokio::test]
async fn a_recheck_expiring_at_the_windows_spawn_lock_never_creates_a_process() {
    use crate::platform::{self, SpawnEdge};

    let armed = Arc::new(AtomicBool::new(false));
    let watch = Arc::clone(&armed);
    let (probe, mut spawn) = mpsc::unbounded_channel();
    let mut rig = Rig::with(
        "question-recheck-held-spawn-lock",
        &provider_failure(),
        |host| {
            let managed = std::mem::replace(
                &mut host.managed,
                Box::new(|| unreachable!("replaced below")),
            );
            host.managed = Box::new(move || {
                if watch.swap(false, Ordering::SeqCst) {
                    platform::observe_next_spawn(probe.clone());
                }
                managed()
            });
        },
    )
    .await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let failure = provider_failed(&mut rig).await;
    let (held, locked) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = platform::spawn_lock();
        held.send(()).unwrap();
        blocked.recv_timeout(WAIT).unwrap();
    });
    timeout(WAIT, locked).await.unwrap().unwrap();
    armed.store(true, Ordering::SeqCst);
    rig.wake(Wake::Note(failure));
    assert_eq!(
        timeout(WAIT, spawn.recv()).await.unwrap(),
        Some(SpawnEdge::Lock)
    );
    rig.at(MAX_LAUNCH);
    release.send(()).unwrap();
    holder.join().unwrap();
    rig.settle().await;

    assert_eq!(spawn.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    rig.close().await;
}

#[tokio::test]
async fn a_provider_failed_question_checks_status_once_and_only_confirmed_sign_out_changes_it() {
    for (case, status, expected, view) in [
        (
            "out",
            Some(json!({"loggedIn": false})),
            claude_failed(ErrorKey::ClaudeCodeSignedOut),
            ClaudeCodeView::SignedOut { version: V },
        ),
        (
            "in",
            None,
            TurnEvent::Finished(Finish::Failed(EngineFailure::Provider)),
            ClaudeCodeView::Ready {
                version: V,
                analytics_on: true,
            },
        ),
        (
            "malformed",
            Some(json!({"loggedIn": 42})),
            TurnEvent::Finished(Finish::Failed(EngineFailure::Provider)),
            ClaudeCodeView::Ready {
                version: V,
                analytics_on: true,
            },
        ),
    ] {
        let mut rig = Rig::new(&format!("question-provider-{case}"), &provider_failure()).await;
        if matches!(view, ClaudeCodeView::SignedOut { .. }) {
            rig.claude.cancelled = Some(rig.claude.generation);
        }
        if let Some(mut status) = status {
            status["configDirectory"] = json!(rig.base.owned().join("config"));
            fs::write(rig.base.0.join("replay-auth-status"), status.to_string()).unwrap();
        }
        let question = rig.claude.take(&rig.snapshot()).expect("taken");
        let mut played = play(&question);
        let lease = played.lease();
        rig.drive(played.asked(lease)).await;
        assert_eq!(rig.drive(played.events()).await, [expected]);
        assert_eq!(rig.claude.view(), view);
        let calls = rig.base.calls();
        let [version, discovery, question, recheck] = calls.as_slice() else {
            panic!("exactly discovery, one question and one status recheck: {calls:?}");
        };
        assert_eq!(version, "--version");
        assert_eq!(discovery, "auth status --json");
        assert!(question.starts_with("-p "));
        assert_eq!(recheck, "auth status --json");
        assert_eq!(
            fs::read_dir(rig.base.owned().join(ENGINE_DIR))
                .unwrap()
                .count(),
            0
        );
        rig.close().await;
    }
}

#[tokio::test]
async fn a_new_find_does_not_take_an_old_questions_provider_recheck() {
    let mut rig = Rig::new("question-provider-replaced", &provider_failure()).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let failure = timeout(WAIT, async {
        loop {
            let note = rig.claude.notes.1.recv().await.unwrap();
            if matches!(note, Note::ProviderFailed { .. }) {
                break note;
            }
            rig.wake(Wake::Note(note));
        }
    })
    .await
    .unwrap();
    rig.claude.op(ConnectionOp::FindClaudeCode);
    rig.wake(Wake::Note(failure));
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    rig.settle().await;
    let calls = rig.base.calls();
    assert_eq!(&calls[3..], ["--version", "auth status --json"]);
    assert_eq!(rig.claude.view(), READY);
    rig.close().await;
}

#[tokio::test]
async fn a_late_provider_recheck_updates_the_account_without_relabeling_the_question() {
    let mut rig = Rig::new("question-provider-recheck-deadline", &provider_failure()).await;
    fs::write(
        rig.base.0.join("replay-auth-status"),
        json!({
            "loggedIn": false,
            "configDirectory": rig.base.owned().join("config")
        })
        .to_string(),
    )
    .unwrap();
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let done = timeout(WAIT, async {
        loop {
            let wake = rig.claude.next().await;
            if let Wake::Lane(done) = wake {
                break done;
            }
            rig.wake(wake);
        }
    })
    .await
    .unwrap();
    assert_eq!(
        done.result,
        Ok(Outcome::Status {
            version: V,
            signed_in: false
        })
    );
    rig.at(MAX_LAUNCH);
    rig.wake(Wake::Lane(done));
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(rig.claude.view(), ClaudeCodeView::SignedOut { version: V });
    rig.close().await;
}

#[tokio::test]
async fn a_new_find_after_recheck_submission_ignores_the_old_answer() {
    let mut rig = Rig::new("question-find-after-recheck", &provider_failure()).await;
    let status = rig.base.0.join("replay-auth-status");
    fs::write(
        &status,
        json!({
            "loggedIn": false,
            "configDirectory": rig.base.owned().join("config")
        })
        .to_string(),
    )
    .unwrap();
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let failure = provider_failed(&mut rig).await;
    rig.wake(Wake::Note(failure));
    let old = timeout(WAIT, rig.claude.lane.as_mut().unwrap().next())
        .await
        .unwrap();
    assert_eq!(
        old.result,
        Ok(Outcome::Status {
            version: V,
            signed_in: false
        })
    );
    fs::remove_file(status).unwrap();
    rig.claude.op(ConnectionOp::FindClaudeCode);
    let generation = rig.claude.generation;
    assert_ne!(generation, old.generation);
    rig.wake(Wake::Lane(old));
    assert_eq!(rig.claude.out, Some(Work::Version));
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    rig.settle().await;
    assert_eq!(rig.claude.view(), READY);
    assert_eq!(
        &rig.base.calls()[3..],
        ["auth status --json", "--version", "auth status --json"]
    );
    rig.close().await;
}

/// T2.
#[tokio::test]
async fn a_warm_launch_writes_only_its_two_control_lines() {
    let mut rig = Rig::new("question-warm", &synthetic_signed_in()).await;
    rig.claude.warm(rig.snapshot()).expect("warm");
    let mut warm = rig.claude.warm.take().expect("the warm attempt");
    let ready = timeout(WAIT, warm.warmed()).await.expect("in time");
    assert!(ready.is_ok(), "the audit before input passed");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    drop(ready);
    drop(warm);
    rig.close().await;
}

type Change = fn(&mut LaunchSnapshot);

/// [`synthetic_signed_in`] with no tool on the server or in init.
fn offering_nothing() -> Script {
    synthetic_signed_in().pre_tools(&[]).post_tools(&[])
}

/// A run's request whose choice is `model` at `effort` on `connection`.
fn choosing(model: &str, effort: Effort, connection: ConnectionKind) -> RunRequest {
    let mut request = RunRequest::for_tests(RunId(1), ThreadId(1), "Be brief.");
    request.request.choice = Some(ModelChoice {
        model: ModelSlug::new(model).expect("valid"),
        effort,
        connection,
    });
    request
}

/// Plays `request`'s question to its end: the round's events, and the launch's reports.
async fn answer(rig: &mut Rig, request: RunRequest) -> (Vec<TurnEvent>, Vec<launch::Outcome>) {
    let question = rig.claude.question(&request).expect("admitted");
    let (id, _) = rig.last();
    let mut played = play_with(&question, request);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let events = rig.drive(played.events()).await;
    (events, rig.reported(id).await)
}

/// Tests 5 and 6, and L1: a Claude Code choice is answered on the row's own alias with its
/// effort's CLI word and no tool, and the whole command line is exactly that. A model the row
/// doesn't list, another connection's choice or none starts nothing.
#[tokio::test]
async fn a_question_takes_only_a_listed_alias_its_effort_word_and_no_tool() {
    let mut rig = Rig::new("question-choice", &offering_nothing()).await;
    let words = [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ];
    for (n, (effort, word)) in words.into_iter().enumerate() {
        let (events, reports) = answer(
            &mut rig,
            choosing("haiku", effort, ConnectionKind::ClaudeCode),
        )
        .await;
        answered(&events);
        assert_eq!(reports, [launch::Outcome::Clean]);
        let argv = rig.argv(n + 1);
        let (head, rest) = argv.split_once(" --mcp-config ").expect("an MCP config");
        let (path, tail) = rest.split_once(' ').expect("more after it");
        let launches = rig.base.owned().join(ENGINE_DIR);
        assert!(
            path.starts_with(launches.to_str().expect("utf-8"))
                && path.ends_with(r"\work\mcp.json"),
            "{path}"
        );
        assert_eq!(
            format!("{head} {tail}"),
            format!(
                "-p --restricted --strict-mcp-config --disable-slash-commands --tools  \
                 --no-session-persistence --input-format stream-json --output-format stream-json \
                 --verbose --include-partial-messages --max-turns 8 \
                 --exclude-dynamic-system-prompt-sections --model haiku --effort {word}"
            ),
            "{effort:?}"
        );
        assert!(
            rig.seed(n + 1).ends_with("Tools:\n[]"),
            "{}",
            rig.seed(n + 1)
        );
    }
    let launches = rig.claude.launches.len();
    for request in [
        choosing("claude-test", Effort::Low, ConnectionKind::ClaudeCode),
        choosing("haiku", Effort::Low, ConnectionKind::ChatGpt),
        RunRequest::for_tests(RunId(1), ThreadId(1), "Be brief."),
    ] {
        assert!(
            matches!(rig.claude.question(&request), Err(LaunchRefused::NotReady)),
            "{request:?}"
        );
    }
    assert_eq!(rig.claude.launches.len(), launches, "nothing started");
    rig.close().await;
}

/// Test 13's own half: the row names its default, haiku at Low, though its list starts with
/// sonnet. It replaces only an unset choice or a Claude Code alias the row doesn't list, and
/// only while a launch would be admitted.
#[tokio::test]
async fn claude_codes_default_is_the_rows_own_and_moves_only_an_unset_or_unlisted_choice() {
    let mut rig = Rig::new("question-default", &synthetic_signed_in()).await;
    assert_eq!(policy::reviewed(V).expect("reviewed").models[0].0, "sonnet");
    let choice = |model: &str, effort, connection| ModelChoice {
        model: ModelSlug::new(model).expect("valid"),
        effort,
        connection,
    };
    let haiku = choice("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    assert_eq!(rig.claude.repaired_default(None), Some(haiku.clone()));
    let unlisted = choice("claude-test", Effort::High, ConnectionKind::ClaudeCode);
    assert_eq!(rig.claude.repaired_default(Some(&unlisted)), Some(haiku));
    for kept in [
        choice("opus", Effort::High, ConnectionKind::ClaudeCode),
        choice("gpt-6-luna", Effort::Low, ConnectionKind::ChatGpt),
    ] {
        assert_eq!(rig.claude.repaired_default(Some(&kept)), None, "{kept:?}");
    }
    rig.claude.view = ClaudeCodeView::NotFound;
    assert_eq!(rig.claude.repaired_default(None), None, "not admitted");
    rig.claude.view = READY;
    rig.close().await;
}

/// Test 9 and OSC-754: a refused question fails with Claude Code's own reason while no launch
/// would be admitted, and with the refusal's own otherwise; never with none, and never with
/// `Access(None)`, which reads as a `ChatGPT` sign-in.
#[tokio::test]
async fn a_refused_question_fails_with_claude_codes_reason_or_the_refusals_own() {
    let mut rig = Rig::new("question-refused", &synthetic_signed_in()).await;
    let cases = [
        (LaunchRefused::NotReady, open(ErrorKey::ModelUnavailable)),
        (
            LaunchRefused::Busy,
            UserError::new(ErrorKey::ClaudeCodeBusy, Vec::new(), vec![Action::Retry])
                .expect("one action"),
        ),
        (LaunchRefused::Identity, ErrorKey::RequestRefused.into()),
        (LaunchRefused::Lifetime, ErrorKey::RequestRefused.into()),
    ];
    for (refused, reason) in cases {
        assert_eq!(
            rig.claude.refused(refused),
            EngineFailure::Claude(reason),
            "{refused:?}"
        );
    }
    rig.claude.view = ClaudeCodeView::NotFound;
    assert_eq!(
        rig.claude.refused(LaunchRefused::NotReady),
        EngineFailure::Claude(open(ErrorKey::ClaudeCodeNotFound))
    );
    rig.claude.view = READY;
    rig.close().await;
}

fn inline_question(mut request: RunRequest) -> RunRequest {
    use crate::context::RunSnapshot;
    use crate::store::context::Snapshot;
    use crate::store::{Generation, Store};
    use crate::test_support::Scratch;
    use kumochi_protocol::message::{AssistantName, EventId};

    let scratch = Scratch::new("question-inline-offer");
    let mut db = Store::open(&scratch.db()).expect("store");
    db.recover().expect("recover");
    let thread = db.main_thread().expect("main");
    let last = EventId(1);
    db.begin_turn(thread, None, RunId(1), "earlier question")
        .expect("turn");
    assert!(
        db.store_summary(thread, Generation(0), (last, last), "earlier summary", None)
            .expect("summary")
    );
    let run = RunSnapshot {
        origin: Origin::Kumo,
        assistant: AssistantName::new("Kumo").expect("name"),
        user: None,
        choice: request.request.choice.clone(),
        connection: ConnectionTag(1),
    };
    let read = Snapshot::begin(db.conn_for_tests()).expect("read");
    let (compact, _) = crate::context::compact::rewrite(&read, &run, thread, 4096)
        .expect("valid build")
        .expect("rewrite");
    read.end().expect("end read");
    request.request = compact.built.request;
    request
}

const BOTH_TOOLS: &[&str] = &["draft_issue", "draw"];

fn assert_tool_seed(seed: &str, expected: &[&str]) {
    let (_, inventory) = seed
        .rsplit_once(
            "Tools:
",
        )
        .expect("seed inventory");
    let tools: Vec<Value> = serde_json::from_str(inventory).expect("schemas");
    assert_eq!(
        tools
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected,
    );
    for tool in &tools {
        if tool["name"] == "draft_issue" {
            crate::test_support::assert_mcp_draft_schema(&tool["description"], &tool["parameters"]);
        } else {
            assert_eq!(
                tool["parameters"]["$defs"]["Surface"],
                json!({"type":"string","const":"overlay"})
            );
            assert!(
                !tool["description"]
                    .as_str()
                    .unwrap()
                    .contains("control-mark")
            );
        }
    }
    assert!(!inventory.contains("untrusted request schema"));
    assert!(!inventory.contains("untrusted-parameters"));
}

#[tokio::test]
async fn a_draw_only_question_replaces_a_wider_warm_offer() {
    let wide = synthetic_signed_in()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new("question-narrow-offer", &wide).await;
    synthetic_signed_in().write(&rig.base, Some(2));
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.request.tools = crate::context::tools(Origin::Kumo);
    let snapshot = rig
        .claude
        .snapshot(
            request.thread,
            "haiku".to_owned(),
            Some("low".to_owned()),
            Offer::authorized(Origin::Kumo, &request.request.tools),
        )
        .unwrap();
    let warm = rig.claude.warm(snapshot).unwrap();
    let (_, old) = rig.last();
    until("the warm child reads its audit", || rig.stdin().len() == 2).await;
    request
        .request
        .tools
        .retain(|tool| tool.name != "draft_issue");
    let question = rig.claude.question(&request).unwrap();
    assert_ne!(rig.last().0, warm);
    assert!(old.is_revoked());
    let mut played = play_with(&question, request);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    assert!(!rig.argv(2).contains("draft_issue"));
    assert_tool_seed(&rig.seed(2), &["draw"]);
    assert!(!rig.base.0.join("seed.1").exists());
    rig.close().await;
}

#[tokio::test]
async fn both_draft_offer_audits_reject_missing_and_extra_names() {
    let missing = &["draw"][..];
    let extra = &["draft_issue", "draw", "unexpected"][..];
    for (pre, post) in [
        (missing, BOTH_TOOLS),
        (extra, BOTH_TOOLS),
        (BOTH_TOOLS, missing),
        (BOTH_TOOLS, extra),
    ] {
        let script = synthetic_signed_in().pre_tools(pre).post_tools(post);
        let mut rig = Rig::new("question-draft-audit", &script).await;
        let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
        request.request.tools = crate::context::tools(Origin::Kumo);
        let question = rig.claude.question(&request).unwrap();
        let (id, control) = rig.last();
        let mut played = play_with(&question, request);
        let seeded = pre == BOTH_TOOLS;
        if seeded {
            let lease = played.lease();
            rig.drive(played.asked(lease)).await;
        }
        assert_eq!(
            rig.drive(played.events()).await,
            [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
        );
        assert_eq!(
            rig.reported(id).await,
            [launch::Outcome::Audit(AuditFailed::Tools)]
        );
        assert_eq!(rig.base.0.join("seed.1").is_file(), seeded);
        assert!(!control.may_call());
        assert_eq!(rig.binds.len(), usize::from(seeded));
        rig.close().await;
    }
}

#[tokio::test]
async fn foreground_launches_expose_only_their_trusted_tools() {
    let mut script = synthetic_signed_in()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    script.user.insert(
        1,
        json!({"type":"assistant", "message":{"content":[{
            "type":"tool_use", "id":"tool_echo", "name":format!("mcp__{MCP_SERVER_NAME}__draft_issue"),
            "input":{"title":"stdout is not a call","body":"not dispatched"}
        }]}}),
    );
    let mut rig = Rig::new("question-production-offer", &script).await;
    synthetic_signed_in()
        .pre_tools(&["draft_issue"])
        .post_tools(&["draft_issue"])
        .write(&rig.base, Some(3));
    synthetic_signed_in().write(&rig.base, Some(4));
    for n in 5..=7 {
        offering_nothing().write(&rig.base, Some(n));
    }
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.request.tools = crate::context::tools(Origin::Kumo);
    for tool in &mut request.request.tools {
        tool.description = "untrusted request schema";
        tool.parameters = json!({"untrusted-parameters":true});
    }
    request.request.instructions.push_str(
        "
A quote: draft_issue is not a permission.",
    );
    let mut task = request.clone();
    task.origin = Origin::Task(ThreadId(2));
    task.thread = ThreadId(2);
    let mut draft_only = task.clone();
    draft_only.request.tools.retain(|tool| tool.name != "draw");
    let mut draw_only = request.clone();
    draw_only
        .request
        .tools
        .retain(|tool| tool.name != "draft_issue");
    let mut job = request.clone();
    job.key = SendKey::Job(crate::coordinator::JobKey {
        thread: request.thread,
        serial: 2,
    });
    let mut tool_free = request.clone();
    tool_free.request.tools.clear();
    let inline = inline_question(request.clone());
    for (i, (request, names)) in [
        (request, BOTH_TOOLS),
        (task, BOTH_TOOLS),
        (draft_only, &["draft_issue"][..]),
        (draw_only, &["draw"][..]),
        (job, &[][..]),
        (tool_free, &[][..]),
        (inline, &[][..]),
    ]
    .into_iter()
    .enumerate()
    {
        let question = rig.claude.question(&request).expect("admitted");
        let (id, _) = rig.last();
        let mut played = play_with(&question, request);
        let initialize = format!("{} initialize", i + 1);
        until("the fake child took its first control line", || {
            rig.stdin().contains(&initialize)
        })
        .await;
        let argv = rig.argv(i + 1);
        let allowed = argv
            .split_once(" --allowedTools ")
            .and_then(|(_, tail)| tail.split(" --").next());
        let expected = names
            .iter()
            .map(|name| format!("mcp__{MCP_SERVER_NAME}__{name}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            allowed,
            (!names.is_empty()).then_some(expected.as_str()),
            "launch {i}"
        );
        assert!(!argv.contains("untrusted request schema"));
        let lease = played.lease();
        rig.drive(played.asked(lease)).await;
        answered(&rig.drive(played.events()).await);
        assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
        assert_tool_seed(&rig.seed(i + 1), names);
    }
    assert_eq!(
        rig.binds.len(),
        4,
        "only the tool-enabled foreground builds bind"
    );
    rig.close().await;
}

/// Test 8: Stop mid-answer drops the round, and the launch's teardown confirms its job empty
/// and deletes its folder.
#[tokio::test]
async fn a_question_stopped_mid_answer_leaves_no_process_and_no_folder() {
    let mut rig = Rig::new("question-stopped", &offering_nothing().held_at_user("go")).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    let (id, control) = rig.last();
    let mut played = play_with(&question, request);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    until("seeded", || rig.stdin().len() == 3).await;
    played.round.abort();
    drop(played);
    drop(question);
    assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
    until("the teardown confirmed", || control.is_ended()).await;
    let launches = rig.base.owned().join(ENGINE_DIR);
    until("the launch folder deleted", || {
        fs::read_dir(&launches).expect("the engine folder").count() == 0
    })
    .await;
    rig.close().await;
}

/// A marker in the prompt and in the answer reaches no log line on the round's thread and no
/// Debug of the question, its request or its reports, as text or as bytes.
#[tokio::test]
async fn a_question_logs_and_debugs_no_prompt_or_answer_text() {
    let captured = Captured::default();
    let _capturing = captured.install();
    let mut script = offering_nothing();
    for line in &mut script.user {
        if let Some(text) = line["event"]["delta"]["text"].as_str() {
            let marked = format!("{text}{SENTINEL}");
            line["event"]["delta"]["text"] = json!(marked);
        }
    }
    let mut rig = Rig::new("question-sentinel", &script).await;
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.request.instructions = format!("Be brief. {SENTINEL}");
    let question = rig.claude.question(&request).expect("admitted");
    let (id, _) = rig.last();
    let debugs = [format!("{question:?}"), format!("{request:?}")];
    let mut played = play_with(&question, request);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    let events = rig.drive(played.events()).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TurnEvent::TextDelta(text) if text.contains(SENTINEL))),
        "the marked answer came through"
    );
    let reports = format!("{:?}", rig.reported(id).await);
    let bytes = format!("{:?}", SENTINEL.as_bytes());
    let bytes = bytes.trim_start_matches('[').trim_end_matches(']');
    for text in debugs.iter().chain([&reports, &captured.text()]) {
        assert!(!text.contains(SENTINEL), "{text}");
        assert!(!text.contains(bytes), "{text}");
    }
    rig.close().await;
}

/// A condition that alone stops admission: its name, how to set it and how to clear it.
type Condition = (&'static str, fn(&mut Claude), fn(&mut Claude));

/// A launch that offers nothing starts with no allow list, passes an audit that expects no tool
/// before input and after the seed, and seeds an empty tool list.
#[tokio::test]
async fn a_launch_that_offers_nothing_has_no_tool_anywhere() {
    let mut rig = Rig::new("question-offers-nothing", &offering_nothing()).await;
    let snapshot = rig
        .claude
        .snapshot(ThreadId(1), "claude-test".to_owned(), None, Offer::Nothing)
        .expect("ready");
    let question = rig.claude.take(&snapshot).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
    let argv = rig.argv(1);
    assert!(!argv.contains("--allowedTools"), "{argv}");
    assert!(
        argv.contains(" --tools  --no-session-persistence "),
        "the built-in tools stay off: {argv}"
    );
    assert!(rig.seed(1).ends_with("Tools:\n[]"), "{}", rig.seed(1));
    rig.close().await;
}

/// T3.
#[tokio::test]
async fn any_change_to_the_snapshot_replaces_the_warm_launch() {
    let mut rig = Rig::new("question-replaced", &synthetic_signed_in()).await;
    let changes: [(&str, Change); 4] = [
        ("model", |s| s.model = "claude-other".to_owned()),
        ("effort", |s| s.effort = Some("high".to_owned())),
        ("thread", |s| s.thread = ThreadId(2)),
        ("offer", |s| {
            s.offer = Offer::for_tests(
                Origin::Task(ThreadId(1)),
                vec![crate::engines::ToolName::Draw],
            );
        }),
    ];
    for (what, change) in changes {
        let snapshot = rig.snapshot();
        let warm = rig.claude.warm(snapshot.clone()).expect("warm");
        let mut other = snapshot;
        change(&mut other);
        let question = rig.claude.take(&other).expect("taken");
        let (taken, _) = rig.last();
        assert_ne!(taken, warm, "{what}");
        let mut played = play(&question);
        let lease = played.lease();
        rig.drive(played.asked(lease)).await;
        answered(&rig.drive(played.events()).await);
    }
    // A find queues behind the live warm launch on the lane, so it's taken before it settles.
    let warm = rig.claude.warm(rig.snapshot()).expect("warm");
    rig.claude.op(ConnectionOp::FindClaudeCode);
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    assert_ne!(rig.last().0, warm, "account");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    rig.settle().await;
    let users = rig
        .stdin()
        .iter()
        .filter(|line| line.ends_with(" user"))
        .count();
    assert_eq!(users, 5);
    rig.close().await;
}

/// T4: a replaced warm attempt's late audit failure changes nothing. The report is sent ahead
/// of the abort's own, as a late one from the replaced launch would be.
#[tokio::test]
async fn a_replaced_warm_attempts_late_failure_changes_nothing() {
    let mut rig = Rig::new("question-late", &synthetic_signed_in()).await;
    let first = rig.claude.warm(rig.snapshot()).expect("warm");
    let other = LaunchSnapshot {
        model: "claude-other".to_owned(),
        ..rig.snapshot()
    };
    rig.claude.warm(other).expect("replaced");
    Report::new(first, rig.claude.notes.0.clone())
        .report(launch::Outcome::Audit(AuditFailed::Tools));
    let wake = timeout(WAIT, rig.claude.next()).await.expect("the note");
    rig.wake(wake);
    assert_eq!(rig.claude.view(), READY);
    rig.settle().await;
    assert_eq!(rig.markers(), Vec::<String>::new());
    rig.close().await;
}

/// T4b: taken while it audits, its audit failure quarantines and fails the run.
#[tokio::test]
async fn a_failure_while_a_taken_attempt_audits_quarantines_and_fails_the_run() {
    let script = synthetic_signed_in().held_until("go");
    let mut script = script;
    let mut mcp: Value = serde_json::from_str(&script.mcp_status[1]).expect("json");
    mcp["response"]["response"]["mcpServers"][0]["tools"] = json!([{"name": "ping"}]);
    script.mcp_status[1] = mcp.to_string();
    let mut rig = Rig::new("question-taken-fails", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    until("auditing", || rig.stdin().len() == 2).await;
    rig.go("go");
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Tools)]
    );
    assert_eq!(
        rig.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    rig.settle().await;
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    rig.close().await;
}

/// OSC-756: the core marks an audit failure as it takes the report, so a brain that quits
/// straight after, before the lane has run its write, keeps the marker.
#[tokio::test]
async fn an_audit_failure_is_marked_before_the_brain_can_quit() {
    let mut script = synthetic_signed_in().held_until("go");
    let mut mcp: Value = serde_json::from_str(&script.mcp_status[1]).expect("json");
    mcp["response"]["response"]["mcpServers"][0]["tools"] = json!([{"name": "ping"}]);
    script.mcp_status[1] = mcp.to_string();
    let mut rig = Rig::new("question-quits-after-failure", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    until("auditing", || rig.stdin().len() == 2).await;
    rig.go("go");
    assert_eq!(
        timeout(WAIT, played.events()).await.expect("ended"),
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Tools)]
    );
    assert!(rig.claude.close(soon()).await, "the brain quits");
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
}

/// T8 and T4b's sign-out: nothing is seeded, the run fails signed out and so does the view.
#[tokio::test]
async fn a_signed_out_launch_fails_the_run_signed_out_before_any_user_line() {
    let script = synthetic_signed_in().initialize(|body| {
        body["account"] = json!({"tokenSource": "none", "apiProvider": "firstParty"});
    });
    let mut rig = Rig::new("question-signed-out", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeSignedOut)]
    );
    assert_eq!(rig.reported(id).await, [launch::Outcome::SignedOut]);
    assert_eq!(rig.claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    rig.close().await;
}

/// T4c.
#[tokio::test]
async fn a_round_cancelled_while_its_attempt_audits_ends_the_audit_and_the_launch() {
    let mut rig = Rig::new(
        "question-cancelled",
        &synthetic_signed_in().held_until("go"),
    )
    .await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, control) = rig.last();
    let played = play(&question);
    drop(question);
    until("auditing", || rig.stdin().len() == 2).await;
    played.round.abort();
    assert!(played.round.await.is_err(), "cancelled");
    rig.go("go");
    assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
    until("the launch's teardown", || control.is_ended()).await;
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    rig.close().await;
}

/// T4d, revision 4's schedule: the audit before input passes and hands its report on, the
/// launch's end reaches the core first, then the post-seed audit's failure does. The core still
/// quarantines, and hears exactly one report for the attempt.
#[tokio::test]
async fn a_late_post_seed_failure_still_quarantines_with_one_report() {
    let script = synthetic_signed_in().init(|init| init["tools"] = json!(["mcp__kumochi__ping"]));
    let mut rig = Rig::new("question-late-failure", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, control) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.authorize().await;
    timeout(WAIT, played.asked(lease)).await.expect("asked");
    assert_eq!(
        timeout(WAIT, played.events()).await.expect("ended"),
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    until("the launch's teardown", || control.is_ended()).await;
    let reports = rig.reported(id).await;
    assert_eq!(
        rig.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools),
        "the report was matched"
    );
    assert_eq!(
        reports,
        [launch::Outcome::Audit(AuditFailed::Tools)],
        "exactly one report"
    );
    rig.settle().await;
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
    rig.close().await;
}

/// T5: warm expiry by the injected clock, at take and again at bind.
#[tokio::test]
async fn a_warm_launch_is_reused_only_before_its_expiry() {
    let mut rig = Rig::new("question-expiry", &synthetic_signed_in()).await;
    for (after, reused) in [
        (Duration::from_millis(29_999), true),
        (Duration::from_secs(30), false),
        (Duration::from_millis(30_001), false),
    ] {
        rig.at(Duration::ZERO);
        let warm = rig.claude.warm(rig.snapshot()).expect("warm");
        rig.at(after);
        let question = rig.claude.take(&rig.snapshot()).expect("taken");
        assert_eq!(rig.last().0 == warm, reused, "{after:?}");
        drop(question);
        let controls: Vec<_> = rig
            .claude
            .launches
            .iter()
            .map(|held| Arc::clone(&held.control))
            .collect();
        until("every teardown", || {
            controls.iter().all(|control| control.is_ended())
        })
        .await;
    }
    rig.close().await;
}

/// A warm launch nobody takes ends at its expiry, so it can't hold the lane.
#[tokio::test]
async fn an_untaken_warm_launch_ends_at_its_expiry() {
    let mut rig = Rig::new("question-warm-expires", &synthetic_signed_in()).await;
    rig.claude.warm(rig.snapshot()).expect("warm");
    let (_, warm) = rig.last();
    rig.at(Duration::from_millis(29_999));
    if let Ok(wake) = timeout(Duration::from_millis(500), rig.claude.next()).await {
        rig.wake(wake);
    }
    assert!(rig.claude.warm.is_some(), "kept before its expiry");
    rig.at(Duration::from_secs(30));
    let wake = timeout(WAIT, rig.claude.next()).await.expect("its expiry");
    rig.wake(wake);
    assert!(rig.claude.warm.is_none());
    until("the warm launch's teardown", || warm.is_ended()).await;
    rig.close().await;
}

/// T5's late Ready: taken at 29.9 s, ready at 30.1 s, so it's replaced at bind through one
/// refresh, and the run is seeded on the second launch only.
#[tokio::test]
async fn a_warm_launch_ready_after_its_expiry_is_replaced_at_bind() {
    let mut rig = Rig::new("question-late-ready", &synthetic_signed_in()).await;
    synthetic_signed_in()
        .held_until("go")
        .write(&rig.base, Some(1));
    let warm = rig.claude.warm(rig.snapshot()).expect("warm");
    let (_, first) = rig.last();
    rig.at(Duration::from_millis(29_900));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    assert_eq!(rig.last().0, warm, "handed off before its expiry");
    let mut played = play(&question);
    until("auditing", || rig.stdin().len() == 2).await;
    rig.at(Duration::from_millis(30_100));
    rig.go("go");
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    assert_eq!(rig.refreshes, 1);
    let stdin = rig.stdin();
    assert!(stdin.contains(&"2 user".to_owned()), "{stdin:?}");
    assert!(!stdin.contains(&"1 user".to_owned()), "{stdin:?}");
    until("the first launch's teardown", || first.is_ended()).await;
    rig.close().await;
}

/// T5b.
#[tokio::test]
async fn a_repeated_warm_never_extends_the_window() {
    let mut rig = Rig::new("question-repeat", &synthetic_signed_in()).await;
    let warm = rig.claude.warm(rig.snapshot()).expect("warm");
    rig.at(Duration::from_secs(20));
    assert_eq!(rig.claude.warm(rig.snapshot()), Ok(warm));
    rig.at(Duration::from_secs(30));
    let _question = rig.claude.take(&rig.snapshot()).expect("taken");
    assert_ne!(rig.last().0, warm);
    rig.close().await;
}

/// T5c: the round is cancelled while its refresh waits; the fresh attempt the core can't
/// deliver is revoked and ends with nothing seeded.
#[tokio::test]
async fn a_refresh_nobody_waits_for_is_revoked() {
    let mut rig = Rig::new("question-refresh-cancelled", &synthetic_signed_in()).await;
    synthetic_signed_in()
        .held_until("go")
        .write(&rig.base, Some(1));
    rig.claude.warm(rig.snapshot()).expect("warm");
    let (_, first) = rig.last();
    rig.at(Duration::from_millis(29_900));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let played = play(&question);
    until("auditing", || rig.stdin().len() == 2).await;
    rig.at(Duration::from_millis(30_100));
    rig.go("go");
    until("the expired launch's teardown", || first.is_ended()).await;
    played.round.abort();
    assert!(played.round.await.is_err(), "cancelled");
    while rig.refreshes == 0 {
        let wake = timeout(WAIT, rig.claude.next()).await.expect("the refresh");
        rig.wake(wake);
    }
    let (_, fresh) = rig.last();
    until("the fresh launch's teardown", || fresh.is_ended()).await;
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    rig.close().await;
}

/// T6.
#[tokio::test]
async fn with_no_question_left_the_lease_goes_back_unused_and_nothing_is_seeded() {
    let mut rig = Rig::new("question-budget", &synthetic_signed_in()).await;
    rig.claude.budget = Arc::new(Budget::limited(Some(0)));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeBudget)]
    );
    let lease = LeaseId {
        key: played.key,
        send: 1,
    };
    assert_eq!(
        played.inputs.try_recv().ok(),
        Some(Input::SendUnsent { lease }),
        "settled before the round ended"
    );
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    rig.close().await;
}

/// T6b.
#[tokio::test]
async fn a_rebuild_spends_no_question() {
    let mut rig = Rig::new("question-rebuild", &synthetic_signed_in()).await;
    rig.claude.budget = Arc::new(Budget::limited(Some(1)));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    rig.drive(played.asked(Answer::Rebuild)).await;
    assert_eq!(rig.drive(played.events()).await, []);
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    rig.close().await;
}

/// T7.
#[tokio::test]
async fn an_init_with_an_extra_plugin_quarantines_the_version() {
    let script = synthetic_signed_in().init(|init| {
        init["plugins"]
            .as_array_mut()
            .expect("plugins")
            .push(json!({"name": "extra", "path": "builtin"}));
    });
    let mut rig = Rig::new("question-plugin", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Plugins)]
    );
    rig.settle().await;
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
    rig.close().await;
}

/// T7b: a later init that differs fails the question, and a valid one after it changes nothing.
#[tokio::test]
async fn a_second_different_init_fails_the_question() {
    let mut script = synthetic_signed_in();
    let first = script.user[0].clone();
    let mut different = first.clone();
    different["analytics_disabled"] = json!(false);
    script.user.insert(1, different);
    script.user.insert(2, first);
    let mut rig = Rig::new("question-second-init", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Malformed)]
    );
    assert_eq!(
        rig.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Malformed)
    );
    rig.close().await;
}

/// T16: initialize's and init's `analytics_disabled`, init's replacing initialize's when present.
#[tokio::test]
async fn analytics_shows_inits_value_else_initializes() {
    let rows = [
        (Some(true), None, false),
        (Some(false), None, true),
        (None, Some(false), true),
        (Some(true), Some(false), true),
        (Some(false), Some(true), false),
        (None, None, false),
    ];
    let mut rig = Rig::new("question-analytics", &synthetic_signed_in()).await;
    for (n, (initialize, init, on)) in rows.into_iter().enumerate() {
        let set = |value: &mut Value, to: Option<bool>| match to {
            Some(to) => value["analytics_disabled"] = json!(to),
            None => {
                value
                    .as_object_mut()
                    .expect("an object")
                    .remove("analytics_disabled");
            }
        };
        synthetic_signed_in()
            .initialize(|body| set(body, initialize))
            .init(|line| set(line, init))
            .write(&rig.base, Some(n + 1));
        let question = rig.claude.take(&rig.snapshot()).expect("taken");
        let (id, _) = rig.last();
        let mut played = play(&question);
        let lease = played.lease();
        rig.drive(played.asked(lease)).await;
        answered(&rig.drive(played.events()).await);
        assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
        assert_eq!(
            rig.claude.view(),
            ClaudeCodeView::Ready {
                version: V,
                analytics_on: on
            },
            "{initialize:?} {init:?}"
        );
    }
    rig.close().await;
}

/// A snapshot cached before a find is stale after it: neither a repeat `warm` nor `take` reuses
/// the warm launch, and each revokes it.
#[tokio::test]
async fn a_snapshot_cached_before_a_find_reuses_no_warm_launch() {
    let mut rig = Rig::new("question-stale-snapshot", &synthetic_signed_in()).await;
    let cached = rig.snapshot();
    rig.claude.warm(cached.clone()).expect("warm");
    let (_, warm) = rig.last();
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(
        rig.claude.take(&cached).err(),
        Some(LaunchRefused::NotReady)
    );
    until("the warm launch's teardown", || warm.is_ended()).await;
    rig.settle().await;
    let cached = rig.snapshot();
    rig.claude.warm(cached.clone()).expect("warm");
    let (_, warm) = rig.last();
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(rig.claude.warm(cached), Err(LaunchRefused::NotReady));
    until("the warm launch's teardown", || warm.is_ended()).await;
    rig.settle().await;
    rig.close().await;
}

#[test]
fn a_snapshots_debug_shows_no_model_or_effort_text() {
    let marker = "zq-marker-7";
    let snapshot = LaunchSnapshot {
        thread: ThreadId(1),
        model: marker.to_owned(),
        effort: Some(marker.to_owned()),
        account: 3,
        offer: Offer::for_tests(Origin::Kumo, vec![crate::engines::ToolName::Draw]),
        version: V,
    };
    let text = format!("{snapshot:?}");
    assert!(!text.contains(marker), "{text}");
    let bytes = format!("{:?}", marker.as_bytes());
    assert!(!text.contains(bytes.trim_matches(['[', ']'])), "{text}");
    assert_eq!(
        text,
        "LaunchSnapshot { thread: ThreadId(1), model_bytes: 11, effort_bytes: Some(11), account: 3, offer: Mcp(Kumo, [Draw]), version: ClaudeVersion { major: 2, minor: 1, patch: 306 } }"
    );
}

/// T1 through the round: the gate the listener admits a `tools/call` by stays shut while the
/// attempt audits, at the ask (bound, not seeded) and after the seed until init's audit, and opens
/// after it, while the answer is held. The listener's text for each state is asserted by
/// `bridge::listener::tests::a_call_needs_a_bound_run_and_a_passed_gate_and_a_failure_stays`.
#[tokio::test]
async fn the_gate_opens_only_after_inits_audit() {
    let mut script = synthetic_signed_in().held_until("go");
    script
        .user
        .insert(1, Value::String("!wait answer".to_owned()));
    let script = script.held_at_user("init");
    let mut rig = Rig::new("question-gate", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (_, control) = rig.last();
    let mut played = play(&question);
    until("auditing", || rig.stdin().len() == 2).await;
    assert!(!control.may_call(), "auditing");
    rig.go("go");
    let ask = rig.drive(played.inputs.recv()).await;
    assert!(matches!(ask, Some(Input::SendReady { .. })), "an ask");
    assert!(!control.may_call(), "bound, not seeded");
    engines::answer(&played.asks, played.key, played.lease());
    until("seeded", || rig.stdin().iter().any(|line| line == "1 user")).await;
    assert!(!control.may_call(), "seeded, before init's audit");
    rig.go("init");
    until("init's audit", || control.may_call()).await;
    rig.go("answer");
    answered(&rig.drive(played.events()).await);
    rig.close().await;
}

/// A model other than the snapshot's, so a second attempt is admitted beside the first.
fn other(snapshot: LaunchSnapshot) -> LaunchSnapshot {
    LaunchSnapshot {
        model: "claude-other".to_owned(),
        ..snapshot
    }
}

/// T16's stale half: an admitted attempt's Analytics note changes nothing unless the view is
/// Ready at the attempt's generation. Each case sends the opposite of the shown value, or `true`
/// where nothing is shown.
#[tokio::test]
async fn analytics_from_a_stale_or_unready_attempt_changes_nothing() {
    let script = synthetic_signed_in().held_until("go");
    let analytics = |rig: &mut Rig, id, on| {
        rig.claude.wake(Wake::Note(Note::Analytics { id, on }));
        rig.claude.view()
    };

    let mut rig = Rig::new("question-analytics-newer", &script).await;
    let old = rig.claude.warm(rig.snapshot()).expect("warm");
    rig.claude.op(ConnectionOp::FindClaudeCode);
    let ClaudeCodeView::Ready { analytics_on, .. } = rig.claude.view() else {
        panic!("Ready while the find is out");
    };
    assert_eq!(
        analytics(&mut rig, old, !analytics_on),
        ClaudeCodeView::Ready {
            version: V,
            analytics_on
        },
        "a newer generation"
    );
    rig.close().await;

    let mut rig = Rig::new("question-analytics-signed-out", &script).await;
    let taken = rig.claude.take(&rig.snapshot()).expect("taken");
    let (signed_out, _) = rig.last();
    let warm = rig.claude.warm(other(rig.snapshot())).expect("warm");
    rig.claude.wake(Wake::Note(Note::Ended {
        id: signed_out,
        outcome: launch::Outcome::SignedOut,
    }));
    assert_eq!(
        analytics(&mut rig, warm, true),
        ClaudeCodeView::SignedOut { version: V },
        "signed out at its generation"
    );
    drop(taken);
    rig.close().await;

    let mut rig = Rig::new("question-analytics-audit-failed", &script).await;
    let taken = rig.claude.take(&rig.snapshot()).expect("taken");
    let (failed, _) = rig.last();
    let warm = rig.claude.warm(other(rig.snapshot())).expect("warm");
    rig.claude.wake(Wake::Note(Note::Ended {
        id: failed,
        outcome: launch::Outcome::Audit(AuditFailed::Tools),
    }));
    assert_eq!(
        analytics(&mut rig, warm, true),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools),
        "after an audit failure"
    );
    drop(taken);
    rig.close().await;

    let mut rig = Rig::new("question-analytics-disconnect", &script).await;
    let warm = rig.claude.warm(rig.snapshot()).expect("warm");
    rig.claude.op(ConnectionOp::DisconnectClaudeCode);
    assert_eq!(
        analytics(&mut rig, warm, true),
        ClaudeCodeView::CleanupPending,
        "after a Disconnect"
    );
    rig.close().await;
}

/// Fixes the question's deadline `left` after bind, independently of child audit time.
fn deadline_in(rig: &mut Rig, left: Duration) {
    rig.claude.clock = Clock(Arc::new(move || {
        Instant::now()
            .checked_sub(launch::QUESTION.saturating_sub(left))
            .expect("the machine has been up for two minutes")
    }));
}

/// A Grant after the question's deadline, which bind fixed: nothing is seeded, the lease goes
/// back unused and the attempt reports `Deadline` once.
#[tokio::test]
async fn a_grant_after_the_question_deadline_seeds_nothing() {
    let mut rig = Rig::new("question-late-grant", &synthetic_signed_in()).await;
    deadline_in(&mut rig, Duration::from_millis(300));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let ask = rig.drive(played.inputs.recv()).await;
    assert!(matches!(ask, Some(Input::SendReady { .. })), "an ask");
    tokio::time::sleep(Duration::from_millis(600)).await;
    engines::answer(&played.asks, played.key, played.lease());
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(rig.reported(id).await, [launch::Outcome::Deadline]);
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    rig.close().await;
}

/// An ask nobody answers ends at the question's deadline: nothing is seeded and the attempt
/// reports `Deadline` once.
#[tokio::test]
async fn an_unanswered_ask_ends_at_the_question_deadline() {
    let mut rig = Rig::new("question-unanswered", &synthetic_signed_in()).await;
    deadline_in(&mut rig, Duration::from_millis(300));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let ask = rig.drive(played.inputs.recv()).await;
    assert!(matches!(ask, Some(Input::SendReady { .. })), "an ask");
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(rig.reported(id).await, [launch::Outcome::Deadline]);
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    rig.close().await;
}

/// C5B1B2-H1, a held grant: a find while the round waits for its ask leaves the launch alone
/// (C5a's S21), so the grant seeds it; the post-seed audit failure that follows, from a launch of
/// an older generation, still quarantines the version.
#[tokio::test]
async fn a_failure_after_a_grant_held_across_a_find_still_quarantines() {
    let script = synthetic_signed_in().init(|init| init["tools"] = json!(["mcp__kumochi__ping"]));
    let mut rig = Rig::new("question-find-held-grant", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let ask = rig.drive(played.inputs.recv()).await;
    assert!(matches!(ask, Some(Input::SendReady { .. })), "an ask");
    rig.claude.op(ConnectionOp::FindClaudeCode);
    engines::answer(&played.asks, played.key, played.lease());
    assert_eq!(
        rig.drive(played.events()).await,
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Tools)]
    );
    assert_eq!(
        rig.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    rig.settle().await;
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
    rig.close().await;
}

/// C5B1B2-H1, a late failure after the seed: the post-seed audit failed before the find, and its
/// report reaches the core after it. It still quarantines the version.
#[tokio::test]
async fn a_post_seed_failure_reported_after_a_find_still_quarantines() {
    let script = synthetic_signed_in().init(|init| init["tools"] = json!(["mcp__kumochi__ping"]));
    let mut rig = Rig::new("question-find-late-failure", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, control) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.authorize().await;
    timeout(WAIT, played.asked(lease)).await.expect("asked");
    assert_eq!(
        timeout(WAIT, played.events()).await.expect("ended"),
        [claude_failed(ErrorKey::ClaudeCodeAuditFailed)]
    );
    until("the launch's teardown", || control.is_ended()).await;
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(
        rig.reported(id).await,
        [launch::Outcome::Audit(AuditFailed::Tools)]
    );
    assert_eq!(
        rig.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    rig.settle().await;
    assert_eq!(rig.markers(), ["claude-code.quarantine-2.1.306"]);
    rig.close().await;
}

/// A warm launch that's revoked, or whose teardown is over, isn't reused: the question gets a
/// fresh launch and is answered.
#[tokio::test]
async fn a_dead_warm_launch_is_not_reused() {
    let mut rig = Rig::new("question-dead-warm", &synthetic_signed_in()).await;
    for (ended, repeat) in [(false, false), (true, false), (false, true), (true, true)] {
        let warm = rig.claude.warm(rig.snapshot()).expect("warm");
        let (_, control) = rig.last();
        control.withdraw();
        if ended {
            until("the warm launch's teardown", || control.is_ended()).await;
        }
        if repeat {
            assert_ne!(
                rig.claude.warm(rig.snapshot()),
                Ok(warm),
                "a repeat, ended: {ended}"
            );
        }
        let question = rig.claude.take(&rig.snapshot()).expect("taken");
        assert_ne!(
            rig.last().0,
            warm,
            "taken, ended: {ended}, repeat: {repeat}"
        );
        let mut played = play(&question);
        let lease = played.lease();
        rig.drive(played.asked(lease)).await;
        answered(&rig.drive(played.events()).await);
    }
    rig.close().await;
}

/// M2's ready-after-deadline schedule: the round is polled to its ask, then not polled again until
/// both the Grant and the question's deadline are ready. The expired Grant seeds nothing, spends
/// no question and settles unused, and the attempt reports `Deadline` once.
#[tokio::test]
async fn a_grant_ready_with_its_expired_deadline_seeds_nothing() {
    let mut rig = Rig::new("question-ready-late-grant", &synthetic_signed_in()).await;
    rig.claude.budget = Arc::new(Budget::limited(Some(1)));
    deadline_in(&mut rig, Duration::from_millis(300));
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let request = RunRequest::for_tests(RunId(1), ThreadId(1), "Be brief.");
    let key = request.key;
    let asks = Asks::default();
    let (inputs_tx, mut inputs) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::channel(64);
    let gate = SendGate::new(key, Arc::clone(&asks), inputs_tx.clone());
    let mut round = question.start(request, events_tx, gate);
    let ask = rig
        .drive(async {
            tokio::select! {
                () = &mut round => panic!("the round ended before its ask"),
                ask = inputs.recv() => ask,
            }
        })
        .await;
    assert!(matches!(ask, Some(Input::SendReady { .. })), "an ask");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let lease = LeaseId { key, send: 1 };
    engines::answer(
        &asks,
        key,
        Answer::Grant(SendLease::new(lease, inputs_tx.clone())),
    );
    rig.drive(&mut round).await;
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    assert_eq!(
        seen,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(inputs.try_recv().ok(), Some(Input::SendUnsent { lease }));
    assert_eq!(rig.reported(id).await, [launch::Outcome::Deadline]);
    assert!(rig.stdin().iter().all(|line| !line.ends_with(" user")));
    assert!(rig.claude.budget.reserve(), "no question was spent");
    rig.close().await;
}

fn open(key: ErrorKey) -> UserError {
    UserError::new(key, Vec::new(), vec![Action::OpenSettings]).expect("one action")
}

fn unavailable(key: ErrorKey) -> Availability {
    Availability::Unavailable(open(key))
}

fn tag(availability: &Availability) -> ConnectionTag {
    let Availability::Ready(ready) = availability else {
        panic!("{availability:?}");
    };
    ready.connection
}

const UNREVIEWED: ClaudeCodeView = ClaudeCodeView::Ready {
    version: ClaudeVersion {
        major: 2,
        minor: 1,
        patch: 307,
    },
    analytics_on: false,
};

/// Each view's reason, with Open Settings only. None is `ConnectionNeeded`, which only the
/// coordinator's rule for a turn nothing can answer sends.
#[test]
fn each_view_reports_its_own_reason_with_open_settings() {
    let cases = [
        (ClaudeCodeView::NotFound, ErrorKey::ClaudeCodeNotFound),
        (
            ClaudeCodeView::SignedOut { version: V },
            ErrorKey::ClaudeCodeSignedOut,
        ),
        (ClaudeCodeView::SigningIn, ErrorKey::ClaudeCodeSignedOut),
        (
            ClaudeCodeView::Unsupported(Unsupported::Shim),
            ErrorKey::ClaudeCodeUnsupported,
        ),
        (
            ClaudeCodeView::PolicyFailed(PolicyFailed::ManagedPolicy),
            ErrorKey::ClaudeCodePolicy,
        ),
        (
            ClaudeCodeView::AuditFailed(AuditFailed::Agents),
            ErrorKey::ClaudeCodeAuditFailed,
        ),
        (
            ClaudeCodeView::CleanupPending,
            ErrorKey::ClaudeCodeCleanupPending,
        ),
        (ClaudeCodeView::TurnedOff, ErrorKey::ModelUnavailable),
        (ClaudeCodeView::InUse, ErrorKey::ClaudeCodeUnsupported),
        (ClaudeCodeView::Unavailable, ErrorKey::ClaudeCodeUnavailable),
        // Ready with no lane: a launch would be refused, so it doesn't read ready either.
        (READY, ErrorKey::ClaudeCodeUnsupported),
    ];
    for (view, key) in cases {
        let mut claude = Claude::new(None, SIGN_IN, view);
        assert_eq!(claude.availability(), unavailable(key), "{view:?}");
    }
    // Being found is its own state, so a typed turn waits for the next report (OSC-753).
    let mut claude = Claude::new(None, SIGN_IN, ClaudeCodeView::Finding);
    assert_eq!(claude.availability(), Availability::Finding);
}

/// Reads unavailable, and refuses a launch for a snapshot taken now.
fn refuses(claude: &mut Claude, what: &str) {
    assert!(
        matches!(claude.availability(), Availability::Unavailable(_)),
        "{what}"
    );
    let snapshot = claude
        .snapshot(
            ThreadId(1),
            "claude-test".to_owned(),
            None,
            Offer::for_tests(Origin::Kumo, vec![crate::engines::ToolName::Draw]),
        )
        .expect("the view still reads Ready");
    assert!(
        matches!(claude.take(&snapshot), Err(LaunchRefused::NotReady)),
        "{what}"
    );
}

/// `availability` and `launch` ask one check: for each condition, a Ready view where only it
/// fails reads unavailable and refuses the launch; with none failing, both say yes.
#[tokio::test]
async fn availability_and_launch_agree_on_every_condition() {
    let mut rig = Rig::new("question-agree", &synthetic_signed_in()).await;
    let Availability::Ready(ready) = rig.claude.availability() else {
        panic!("ready");
    };
    assert_eq!(ready.connection, ConnectionTag(1));
    let slugs: Vec<&str> = ready
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .collect();
    assert_eq!(slugs, ["sonnet", "opus", "haiku", "fable"]);
    assert!(!ready.trusted_draw);
    drop(rig.claude.take(&rig.snapshot()).expect("admitted"));

    let conditions: [Condition; 5] = [
        (
            "a pending Disconnect",
            |c| c.cleaning = true,
            |c| c.cleaning = false,
        ),
        (
            "a quarantine",
            |c| {
                c.quarantine = Some(Quarantine {
                    version: V,
                    reason: AuditFailed::Agents,
                    durable: true,
                });
            },
            |c| c.quarantine = None,
        ),
        ("turned off", |c| c.finds = false, |c| c.finds = true),
        (
            "an undecided sign-in store",
            |c| c.sign_in = SignInStore::Undecided,
            |c| c.sign_in = SignInStore::FileWithDacl,
        ),
        (
            "an unreviewed version",
            |c| c.view = UNREVIEWED,
            |c| c.view = READY,
        ),
    ];
    for (what, set, undo) in conditions {
        set(&mut rig.claude);
        refuses(&mut rig.claude, what);
        undo(&mut rig.claude);
        assert!(
            matches!(rig.claude.availability(), Availability::Ready(_)),
            "back after {what}"
        );
    }
    let lane = rig.claude.lane.take();
    refuses(&mut rig.claude, "no lane");
    rig.claude.lane = lane;
    rig.close().await;
}

/// A Find while ready keeps the tag, so a run bound to it goes on; a sign-out ends it and the
/// next ready state draws a new one, as an unreviewed version in between does.
#[tokio::test]
async fn the_tag_survives_a_find_and_never_outlives_a_sign_out() {
    let mut rig = Rig::new("question-epoch", &synthetic_signed_in()).await;
    let first = tag(&rig.claude.availability());
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(
        tag(&rig.claude.availability()),
        first,
        "while the Find runs"
    );
    rig.settle().await;
    assert_eq!(rig.claude.view(), READY);
    assert_eq!(tag(&rig.claude.availability()), first, "after it");

    rig.claude.view = ClaudeCodeView::SignedOut { version: V };
    assert_eq!(
        rig.claude.availability(),
        unavailable(ErrorKey::ClaudeCodeSignedOut)
    );
    rig.claude.view = READY;
    let second = tag(&rig.claude.availability());
    assert_ne!(
        second, first,
        "a later sign-in never answers a run bound before"
    );

    rig.claude.view = UNREVIEWED;
    assert_eq!(
        rig.claude.availability(),
        unavailable(ErrorKey::ClaudeCodeUnsupported)
    );
    rig.claude.view = READY;
    assert_ne!(
        tag(&rig.claude.availability()),
        second,
        "nor across a version"
    );
    rig.close().await;
}

/// Makes every teardown unconfirmed once set; only the test below sets it.
static STUCK: AtomicBool = AtomicBool::new(false);

fn end_unless_stuck(job: &Job) -> io::Result<()> {
    if STUCK.load(Ordering::SeqCst) {
        return Err(io::ErrorKind::Other.into());
    }
    Job::terminate(job)
}

/// A lane a teardown left stuck refuses the next launch: that question fails with Claude Code's
/// reason, never with none, and Claude Code stops reading ready.
#[tokio::test]
async fn a_stuck_lane_fails_the_next_question_with_a_reason_and_stops_reading_ready() {
    let mut rig = Rig::with("question-stuck", &synthetic_signed_in(), |host| {
        host.terminate = end_unless_stuck;
    })
    .await;
    assert!(matches!(rig.claude.availability(), Availability::Ready(_)));
    STUCK.store(true, Ordering::SeqCst);
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (_, control) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    drop(question);
    until("the stuck teardown", || control.is_ended()).await;

    let question = rig
        .claude
        .take(&rig.snapshot())
        .expect("the core can't see the lane is stuck yet");
    let (second, _) = rig.last();
    let mut played = play(&question);
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Claude(
            open(ErrorKey::ClaudeCodeUnavailable)
        )))]
    );
    assert_eq!(rig.reported(second).await, [launch::Outcome::Refused]);
    assert_eq!(rig.claude.view(), ClaudeCodeView::Unavailable);
    assert_eq!(
        rig.claude.availability(),
        unavailable(ErrorKey::ClaudeCodeUnavailable)
    );
    // shortcut: the kept lock lives until the test process ends, leaving this scratch folder.
    assert!(
        !rig.claude.close(soon()).await,
        "a stuck lane keeps its lock"
    );
}

/// OSC-767: a question refused while the queue is full fails Busy with Retry. Once the lane takes
/// a job off the queue the core finds again by itself, so the Retry is admitted and answered.
#[tokio::test]
async fn a_full_queue_clears_itself_once_it_drains_and_the_retry_is_answered() {
    let mut rig = Rig::new("question-busy", &offering_nothing().held_at_user("go")).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    let (first, _) = rig.last();
    let mut played = play_with(&question, request.clone());
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    until("seeded", || rig.stdin().len() == 3).await;
    let lane = rig.claude.lane.as_ref().expect("a lane");
    for _ in 0..crate::engines::claude_code::lane::LANE_QUEUE {
        assert!(lane.submit(0, Work::Version), "an old job fills a slot");
    }
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(rig.claude.view(), ClaudeCodeView::Busy);
    let refused = rig
        .claude
        .question(&request)
        .expect_err("refused while full");
    assert_eq!(
        rig.claude.refused(refused),
        EngineFailure::Claude(
            UserError::new(ErrorKey::ClaudeCodeBusy, Vec::new(), vec![Action::Retry])
                .expect("one action")
        )
    );
    rig.go("go");
    rig.drive(played.events()).await;
    drop(question);
    rig.reported(first).await;
    timeout(Duration::from_secs(30), async {
        while rig.claude.view() != READY {
            let wake = rig.claude.next().await;
            rig.wake(wake);
        }
    })
    .await
    .expect("found again once the queue drained");
    let (events, outcomes) = answer(&mut rig, request).await;
    answered(&events);
    assert_eq!(outcomes, [launch::Outcome::Clean]);
    rig.close().await;
}

/// What woke a test brain's core: one of its inputs, or Claude Code's.
enum Next {
    Input(Input),
    Wake(Wake),
}

/// Runs `brain`'s Claude Code core and grants every ask, until an input `done` picks.
async fn run_brain<T>(brain: &mut Brain, mut done: impl FnMut(Input) -> Option<T>) -> T {
    timeout(Duration::from_secs(30), async {
        loop {
            let next = tokio::select! {
                input = brain.inputs.recv() => Next::Input(input.expect("the brain holds its sender")),
                input = brain.engine_events.recv() => Next::Input(input.expect("the brain holds its sender")),
                wake = brain.claude.next() => Next::Wake(wake),
            };
            match next {
                Next::Wake(wake) => brain.claude_done(wake, None).expect("no link"),
                Next::Input(Input::SendReady { key, .. }) => {
                    let lease = SendLease::new(LeaseId { key, send: 1 }, brain.inputs_tx.clone());
                    engines::answer(&brain.asks, key, Answer::Grant(lease));
                }
                Next::Input(input) => {
                    if let Some(found) = done(input) {
                        return found;
                    }
                }
            }
        }
    })
    .await
    .expect("in time")
}

/// Runs `brain`'s Claude Code core and grants every ask until `done` holds.
async fn drive_until(brain: &mut Brain, done: impl Fn() -> bool) {
    timeout(Duration::from_secs(30), async {
        while !done() {
            let next = tokio::select! {
                input = brain.inputs.recv() => Next::Input(input.expect("the brain holds its sender")),
                wake = brain.claude.next() => Next::Wake(wake),
                () = tokio::time::sleep(Duration::from_millis(20)) => continue,
            };
            match next {
                Next::Wake(wake) => brain.claude_done(wake, None).expect("no link"),
                Next::Input(Input::SendReady { key, .. }) => {
                    let lease = SendLease::new(LeaseId { key, send: 1 }, brain.inputs_tx.clone());
                    engines::answer(&brain.asks, key, Answer::Grant(lease));
                }
                Next::Input(_) => {}
            }
        }
    })
    .await
    .expect("in time");
}

/// A brain whose Claude Code is `rig`'s Ready connection; the rig keeps the fake's base.
fn brain_on(rig: &mut Rig, scratch: &Scratch) -> Brain {
    let mut brain = crate::test_support::brain(scratch, "");
    std::mem::swap(&mut brain.claude, &mut rig.claude);
    brain
}

async fn started_provider_recheck(brain: &mut Brain, base: &Base) -> RunId {
    use crate::coordinator::Effect;

    brain.claude_reported(None).expect("no shell");
    let started = brain
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .unwrap();
    let run = brain.coordinator.current_run().unwrap();
    assert!(
        started
            .iter()
            .any(|effect| matches!(effect, Effect::ArmDeadline { run: id, .. } if *id == run))
    );
    let effects = admit_provider_build(brain, run);
    brain.apply(effects, None).unwrap();
    drive_until(brain, || base.0.join("seed.1").is_file()).await;
    timeout(WAIT, async {
        while brain.claude.recheck.is_none() {
            let wake = brain.claude.next().await;
            brain.claude_done(wake, None).unwrap();
        }
    })
    .await
    .unwrap();
    run
}

fn admit_provider_build(brain: &mut Brain, run: RunId) -> Vec<crate::coordinator::Effect> {
    use crate::context::{Built, tools};
    use crate::coordinator::{ContextBuilt, Effect, StoreOp};

    let effects = brain.handle(Input::TurnStored { run }).unwrap();
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .unwrap();
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode).request;
    request.tools = tools(Origin::Kumo);
    let mut snapshot = brain.snapshot(Origin::Kumo);
    snapshot.choice.clone_from(&request.choice);
    brain
        .handle(Input::ContextBuilt(ContextBuilt {
            key,
            snapshot: Some(snapshot),
            result: Ok(Built {
                request,
                sources: Vec::new(),
            }),
        }))
        .unwrap()
}

#[tokio::test]
async fn parsed_provider_failure_survives_a_deadline_with_usage_backpressured() {
    use crate::coordinator::Effect;
    use crate::test_support::{self, Scratch};

    let name = "question-recheck-usage-backpressure";
    let mut script = provider_failure()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    script.user.last_mut().unwrap()["usage"] = json!({"input_tokens": 4, "output_tokens": 2});
    let mut rig = Rig::new(name, &script).await;
    fs::write(rig.base.0.join("watch-stdin-eof"), b"").unwrap();
    let scratch = Scratch::new(name);
    let mut brain = test_support::brain(&scratch, "");
    std::mem::swap(&mut brain.claude, &mut rig.claude);
    brain.claude_reported(None).unwrap();
    brain.offline(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    });
    let run = brain.coordinator.current_run().unwrap();
    assert_eq!(
        timeout(WAIT, brain.inputs.recv()).await.unwrap(),
        Some(Input::TurnStored { run })
    );
    let request = admit_provider_build(&mut brain, run)
        .into_iter()
        .find_map(|effect| match effect {
            Effect::StartEngine { request } => Some(request),
            _ => None,
        })
        .unwrap();
    let question = brain.claude.question(&request).unwrap();
    let gate = SendGate::new(
        request.key,
        Arc::clone(&brain.asks),
        brain.inputs_tx.clone(),
    );
    let (events, mut received) = mpsc::channel(1);
    let lease = events.reserve().await.unwrap();
    let round = tokio::spawn(question.start(request, events.clone(), gate));
    drive_until(&mut brain, || rig.base.0.join("stdin-eof.1").is_file()).await;
    while let Ok(note) = brain.claude.notes.1.try_recv() {
        brain.claude_done(Wake::Note(note), None).unwrap();
    }
    assert_eq!(events.capacity(), 0);
    assert_eq!(received.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(!round.is_finished());

    brain.offline(Input::Deadline { run });
    let stored = timeout(WAIT, async {
        loop {
            let input = brain.inputs.recv().await.unwrap();
            if matches!(input, Input::RunStored { .. }) {
                break input;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(stored, Input::RunStored { run });
    brain.offline(stored);
    round.abort();
    assert!(round.await.unwrap_err().is_cancelled());
    drop(lease);
    brain.close().await;
    let db = rusqlite::Connection::open(scratch.db()).unwrap();
    let outcome: (String, String) = db
        .query_row("SELECT state, reason FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(outcome, ("failed".to_owned(), "engine".to_owned()));
}

#[tokio::test]
async fn a_turn_deadline_during_recheck_keeps_the_observed_provider_failure() {
    use crate::coordinator::{Effect, StoreOp};
    use crate::store::{Failure as RunFailure, RunEnd};

    let name = "question-recheck-turn-deadline";
    let script = provider_failure()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new(name, &script).await;
    let scratch = Scratch::new(name);
    let mut brain = brain_on(&mut rig, &scratch);
    let run = started_provider_recheck(&mut brain, &rig.base).await;
    let effects = brain.handle(Input::Deadline { run }).unwrap();
    assert_eq!(
        effects.iter().find_map(|effect| match effect {
            Effect::Store(StoreOp::EndRun { run: id, end, .. }) if *id == run => Some(*end),
            _ => None,
        }),
        Some(RunEnd::Failed(RunFailure::Engine))
    );
    brain.apply(effects, None).unwrap();
    assert!(brain.claude.close(soon()).await);
    brain.close().await;
}

#[tokio::test]
async fn full_provider_terminal_does_not_replay_signed_out_models_on_reconnect() {
    use std::cell::RefCell;

    use kumochi_protocol::frame;
    use tokio::sync::Notify;

    use super::super::{Seen, data::DataQueue, window};
    use super::tests::{Pipe, row};
    use crate::coordinator::StoreOp;
    use crate::store::{Failure as RunFailure, RunEnd};
    use crate::tap::Tap;
    use crate::test_support::{self, Scratch};

    let name = "question-recheck-full-terminal";
    let script = provider_failure()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new(name, &script).await;
    fs::write(
        rig.base.0.join("replay-auth-status"),
        json!({ "loggedIn": false, "configDirectory": rig.base.owned().join("config") })
            .to_string(),
    )
    .unwrap();
    let scratch = Scratch::new(name);
    let mut brain = test_support::brain(&scratch, "");
    std::mem::swap(&mut brain.claude, &mut rig.claude);
    let run = started_provider_recheck(&mut brain, &rig.base).await;
    let mut first_shell = Pipe::new();
    brain.claude_models(&mut first_shell.link()).unwrap();
    assert_eq!(first_shell.heard(), [row()]);
    let done = timeout(WAIT, brain.claude.lane.as_mut().unwrap().next())
        .await
        .unwrap();
    assert_eq!(
        done.result,
        Ok(Outcome::Status {
            version: V,
            signed_in: false
        })
    );
    rig.at(MAX_LAUNCH);

    let mut observed = brain.observe();
    let (out, mut unread) = mpsc::channel(1);
    out.try_send(Vec::new())
        .expect("fill the old control queue");
    let queue = RefCell::new(DataQueue::new(window()));
    let (ready, mut tap) = (Notify::new(), Tap::from_env());
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert!(
        brain
            .claude_done(Wake::Lane(done), Some(&mut link))
            .is_err()
    );
    assert_eq!(unread.try_recv(), Ok(Vec::new()));
    assert!(matches!(
        observed.try_recv(),
        Ok(Seen::StoreOpQueued(StoreOp::EndRun {
            run: id, end: RunEnd::Failed(RunFailure::Engine), ..
        })) if run == id
    ));

    let mut reconnect = Pipe::new();
    brain.open(&mut reconnect.link()).unwrap();
    let mut observed_view = None;
    while let Ok(bytes) = reconnect.control.try_recv() {
        if let ControlToShell::Connections(view) =
            frame::decode(&bytes[frame::HEADER_LEN..]).unwrap()
        {
            observed_view = Some(view.claude_code);
        }
    }
    assert_eq!(
        observed_view,
        Some(ClaudeCodeView::SignedOut { version: V })
    );
    assert_eq!(reconnect.heard(), Vec::<Vec<ModelInfo>>::new());
    assert!(brain.claude.epoch.is_none());
    brain.close().await;
}

#[tokio::test]
async fn stop_beats_the_recheck_reply_but_does_not_hide_confirmed_account_status() {
    use crate::coordinator::{Effect, StoreOp};
    use crate::store::{Interruption, RunEnd};

    let name = "question-recheck-after-stop";
    let script = provider_failure()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new(name, &script).await;
    fs::write(
        rig.base.0.join("replay-auth-status"),
        json!({ "loggedIn": false, "configDirectory": rig.base.owned().join("config") })
            .to_string(),
    )
    .unwrap();
    let scratch = Scratch::new(name);
    let mut brain = brain_on(&mut rig, &scratch);
    let run = started_provider_recheck(&mut brain, &rig.base).await;
    let done = timeout(WAIT, brain.claude.lane.as_mut().unwrap().next())
        .await
        .unwrap();
    assert_eq!(
        done.result,
        Ok(Outcome::Status {
            version: V,
            signed_in: false
        })
    );

    let (send, mut reply) = oneshot::channel();
    brain.claude.recheck.as_mut().unwrap().signed_out = send;
    let stopped = brain
        .handle(Input::Stop {
            run,
            revealed: Vec::new(),
        })
        .unwrap();
    assert!(stopped.iter().any(|effect| matches!(
        effect, Effect::Store(StoreOp::EndRun { run: id, end: RunEnd::Interrupted(Interruption::Stop), .. }) if *id == run
    )));
    brain.apply(stopped, None).unwrap();
    brain.claude_done(Wake::Lane(done), None).unwrap();
    assert_eq!(reply.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    assert_eq!(brain.coordinator.current_run(), None);
    assert_eq!(
        brain.claude.view(),
        ClaudeCodeView::SignedOut { version: V }
    );
    assert!(brain.claude.close(soon()).await);
    brain.close().await;
}

#[tokio::test]
async fn an_admitted_foreground_offer_binds_before_seeding() {
    use crate::context::{Built, tools};
    use crate::coordinator::{ContextBuilt, Effect, StoreOp};

    let script = synthetic_signed_in()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS)
        .held_at_user("go");
    let mut rig = Rig::new("question-admitted-draw", &script).await;
    let scratch = Scratch::new("question-admitted-draw");
    let mut brain = brain_on(&mut rig, &scratch);
    brain.claude_reported(None).expect("no shell");
    brain
        .coordinator
        .handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn");
    let run = brain.coordinator.current_run().expect("current run");
    let effects = brain
        .coordinator
        .handle(Input::TurnStored { run })
        .expect("a stored turn");
    let key = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
            _ => None,
        })
        .expect("a build");
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode).request;
    request.tools = tools(Origin::Kumo);
    let mut snapshot = brain.snapshot(Origin::Kumo);
    snapshot.choice.clone_from(&request.choice);
    let admitted = brain
        .coordinator
        .handle(Input::ContextBuilt(ContextBuilt {
            key,
            snapshot: Some(snapshot),
            result: Ok(Built {
                request,
                sources: Vec::new(),
            }),
        }))
        .expect("the build is current");
    assert!(admitted.iter().any(|effect| matches!(
        effect,
        Effect::StartEngine { request } if request.key == SendKey::Build(key)
    )));
    brain.apply(admitted, None).expect("no shell");
    drive_until(&mut brain, || rig.base.0.join("seed.1").is_file()).await;
    assert!(
        brain.claude.launches.last().expect("the launch").bound,
        "the real brain accepted the launch's offer for the admitted build"
    );

    rig.go("go");
    let mut events = Vec::new();
    run_brain(&mut brain, |input| match input {
        Input::Engine { key: found, event } if key == found => {
            let finished = matches!(event, TurnEvent::Finished(_));
            events.push(event);
            finished.then_some(())
        }
        _ => None,
    })
    .await;
    answered(&events);
    assert_tool_seed(&rig.seed(1), BOTH_TOOLS);
    assert!(brain.claude.close(soon()).await);
}

/// A title job's request on haiku at Low over Claude Code.
fn title_job(job: crate::coordinator::JobKey) -> RunRequest {
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.key = SendKey::Job(job);
    request
}

/// Test 7: a title job on a Claude Code choice asks Claude Code on the row's own alias, its
/// effort's word and no tool, the same command line a typed turn takes, and reports the title.
#[tokio::test]
async fn a_title_job_on_claude_code_asks_like_a_turn_and_reports_its_title() {
    use crate::coordinator::{JobKey, JobKind, JobOutput};
    let mut rig = Rig::new("job-title", &offering_nothing()).await;
    let scratch = Scratch::new("job-title");
    let mut brain = brain_on(&mut rig, &scratch);
    let job = JobKey {
        thread: ThreadId(1),
        serial: 2,
    };
    brain.start_job(job, JobKind::Title, title_job(job));
    let output = run_brain(&mut brain, |input| match input {
        Input::JobDone { key, output } if key == job => Some(output),
        _ => None,
    })
    .await;
    assert_eq!(
        output,
        Some(JobOutput::Title(
            kumochi_protocol::message::ThreadTitle::new("Hello from the fake. Café ✓")
                .expect("valid")
        ))
    );
    let argv = rig.argv(1);
    let (head, rest) = argv.split_once(" --mcp-config ").expect("an MCP config");
    let (_, tail) = rest.split_once(' ').expect("more after it");
    assert_eq!(
        format!("{head} {tail}"),
        "-p --restricted --strict-mcp-config --disable-slash-commands --tools  \
         --no-session-persistence --input-format stream-json --output-format stream-json \
         --verbose --include-partial-messages --max-turns 8 \
         --exclude-dynamic-system-prompt-sections --model haiku --effort low"
    );
    assert!(rig.seed(1).ends_with("Tools:\n[]"), "{}", rig.seed(1));
    assert!(brain.claude.close(soon()).await);
}

/// Test 9: a typed turn arrives while a Claude Code title job is mid-answer. The runtime gets the
/// job's cancel and then the turn's round, in the order a new turn sends them: the job's launch
/// is torn down and its folder deleted, and the turn's question launches behind it and answers.
#[tokio::test]
async fn a_turn_during_a_claude_code_title_job_tears_the_job_down_and_is_answered() {
    use crate::coordinator::{Effect, JobKey, JobKind};
    let mut rig = Rig::new("job-then-turn", &offering_nothing().held_at_user("go")).await;
    offering_nothing().write(&rig.base, Some(2));
    let scratch = Scratch::new("job-then-turn");
    let mut brain = brain_on(&mut rig, &scratch);
    let job = JobKey {
        thread: ThreadId(1),
        serial: 2,
    };
    brain.start_job(job, JobKind::Title, title_job(job));
    let control = Arc::clone(
        &brain
            .claude
            .launches
            .last()
            .expect("the job's launch")
            .control,
    );
    drive_until(&mut brain, || rig.stdin().len() == 3).await;
    let turn = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let SendKey::Build(round) = turn.key else {
        unreachable!("a turn's request is a build's");
    };
    brain
        .apply(
            vec![
                Effect::CancelJob(job),
                Effect::StartEngine { request: turn },
            ],
            None,
        )
        .expect("no link");
    let mut events = Vec::new();
    run_brain(&mut brain, |input| match input {
        Input::Engine { key, event } if key == round => {
            let finished = matches!(event, TurnEvent::Finished(_));
            events.push(event);
            finished.then_some(())
        }
        _ => None,
    })
    .await;
    answered(&events);
    assert_eq!(
        fs::read_to_string(rig.base.0.join("starts.log"))
            .expect("starts")
            .lines()
            .filter(|start| start.starts_with("-p "))
            .count(),
        2,
        "the job's question, then the turn's"
    );
    let launches = rig.base.owned().join(ENGINE_DIR);
    drive_until(&mut brain, || {
        control.is_ended() && fs::read_dir(&launches).expect("the engine folder").count() == 0
    })
    .await;
    assert!(brain.claude.close(soon()).await);
}

/// The probe on Rez (2026-11-15): 2.1.306 answers `mcp_status` with `pending` until Kumochi's
/// server has connected, so the audit asks again and the question goes on once it reads connected.
#[tokio::test]
async fn a_server_still_connecting_is_asked_again_and_the_question_is_answered() {
    let script = synthetic_signed_in();
    let connected = script.mcp_status[0].clone();
    let script = script.pending_first(vec![connected]);
    let mut rig = Rig::new("question-pending", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    let lease = played.lease();
    rig.drive(played.asked(lease)).await;
    answered(&rig.drive(played.events()).await);
    assert_eq!(
        rig.stdin(),
        ["1 initialize", "1 mcp_status", "1 mcp_status", "1 user"]
    );
    assert_eq!(rig.reported(id).await, [launch::Outcome::Clean]);
    assert_eq!(rig.claude.view(), READY);
    rig.close().await;
}

/// A server that stays `pending` ends the launch at `AUDIT`, as a late answer does: no seed, no
/// quarantine, and the asks stay within `MAX_CONTROL`. The log names the `Deadline` check before
/// stdin.
#[tokio::test]
async fn a_server_pending_until_the_audit_deadline_ends_the_launch_unseeded() {
    let captured = crate::test_support::Captured::default();
    let _capturing = captured.install();
    let script = synthetic_signed_in().pending_first(Vec::new());
    let mut rig = Rig::new("question-pending-deadline", &script).await;
    let question = rig.claude.take(&rig.snapshot()).expect("taken");
    let (id, _) = rig.last();
    let mut played = play(&question);
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    assert_eq!(rig.reported(id).await, [launch::Outcome::Deadline]);
    let stdin = rig.stdin();
    let asks = stdin.iter().filter(|line| *line == "1 mcp_status").count();
    assert_eq!(stdin[0], "1 initialize");
    assert!(
        !stdin.iter().any(|line| line == "1 user"),
        "nothing but mcp_status after initialize: {stdin:?}"
    );
    assert!((2..=31).contains(&asks), "{asks} asks in AUDIT");
    assert_eq!(rig.markers(), Vec::<String>::new());
    assert_eq!(rig.claude.view(), READY);
    let logs = captured.text();
    assert!(
        logs.contains(r#"claude code's audit failed check=Deadline phase="before stdin""#),
        "{logs}"
    );
    rig.close().await;
}

/// A find starts a new generation, which ends a bound launch's authority; an unbound launch runs
/// on.
#[tokio::test]
async fn a_new_generation_revokes_only_a_bound_launch() {
    let script = synthetic_signed_in().held_until("go");
    for bound in [true, false] {
        let mut rig = Rig::new(&format!("question-generation-{bound}"), &script).await;
        let id = rig.claude.warm(rig.snapshot()).expect("warm");
        rig.claude
            .launches
            .iter_mut()
            .filter(|held| held.id == id)
            .for_each(|held| held.bound = bound);
        rig.claude.op(ConnectionOp::FindClaudeCode);
        let held = rig.claude.launches.iter().find(|held| held.id == id);
        assert_eq!(
            held.expect("held").control.is_revoked(),
            bound,
            "bound: {bound}"
        );
        rig.close().await;
    }
}

/// A new generation closes a picture grant even while its question's launch, unbound, runs on.
#[tokio::test]
async fn a_new_generation_closes_a_picture_grant_while_its_unbound_launch_runs_on() {
    let script = synthetic_signed_in().held_until("go");
    let mut rig = Rig::new("question-picture-generation", &script).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    let grant = crate::coordinator::PictureGrant::open_for_tests();
    rig.claude
        .picture_granted(request.key, Some(ConnectionKind::ClaudeCode), grant.clone());
    let (inputs, _settled) = mpsc::unbounded_channel();
    let lease = crate::engines::SendLease::new(
        LeaseId {
            key: request.key,
            send: 1,
        },
        inputs,
    )
    .with_picture(Some(grant));
    assert!(crate::connections::model::Lease::admit(&lease).is_some());
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert!(crate::connections::model::Lease::admit(&lease).is_none());
    assert!(
        rig.claude
            .launches
            .iter()
            .all(|held| !held.control.is_revoked()),
        "the unbound launch runs on"
    );
    drop(question);
    rig.close().await;
}

/// A kept picture grant whose send has settled, so only this core holds it, is let go at the
/// next grant; one a lease still holds is kept.
#[tokio::test]
async fn a_picture_grant_whose_send_settled_is_not_kept() {
    let script = synthetic_signed_in().held_until("go");
    let mut rig = Rig::new("question-picture-settled", &script).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    rig.claude.picture_granted(
        request.key,
        Some(ConnectionKind::ClaudeCode),
        crate::coordinator::PictureGrant::open_for_tests(),
    );
    let held = crate::coordinator::PictureGrant::open_for_tests();
    rig.claude
        .picture_granted(request.key, Some(ConnectionKind::ClaudeCode), held.clone());
    assert_eq!(rig.claude.pictures.len(), 1);
    rig.claude.picture_granted(
        request.key,
        Some(ConnectionKind::ClaudeCode),
        crate::coordinator::PictureGrant::open_for_tests(),
    );
    assert_eq!(rig.claude.pictures.len(), 2);
    assert!(held.is_open());
    drop(question);
    rig.close().await;
}

#[tokio::test]
async fn a_busy_picture_grant_survives_pruning_until_generation_close() {
    let script = synthetic_signed_in().held_until("go");
    let mut rig = Rig::new("question-picture-busy", &script).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    let grant = crate::coordinator::PictureGrant::open_for_tests();
    let kind = Some(ConnectionKind::ClaudeCode);
    rig.claude.picture_granted(request.key, kind, grant.clone());
    let write = grant.reserve_write().expect("first admission");
    assert!(grant.is_open());
    assert!(grant.reserve_write().is_none(), "one write at a time");
    rig.claude.picture_granted(
        request.key,
        kind,
        crate::coordinator::PictureGrant::open_for_tests(),
    );
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert!(!grant.is_open(), "the kept grant closes while busy");
    drop(write);
    assert!(!grant.is_open(), "the permit cannot reopen the grant");
    assert!(grant.reserve_write().is_none());
    drop(question);
    rig.close().await;
}

/// A picture grant for a question asked under an older generation is closed as it's granted,
/// and so is a Claude Code grant for any other build; a key route's grant is never touched.
#[tokio::test]
async fn a_picture_grant_for_a_question_of_an_older_generation_is_closed_at_once() {
    let script = synthetic_signed_in().held_until("go");
    let mut rig = Rig::new("question-picture-older", &script).await;
    let request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    let question = rig.claude.question(&request).expect("admitted");
    rig.claude.op(ConnectionOp::FindClaudeCode);
    let grant = crate::coordinator::PictureGrant::open_for_tests();
    rig.claude
        .picture_granted(request.key, Some(ConnectionKind::ClaudeCode), grant.clone());
    assert!(!grant.is_open());
    let other = crate::coordinator::PictureGrant::open_for_tests();
    let SendKey::Build(build) = request.key else {
        panic!("a build's question")
    };
    let elsewhere = SendKey::Build(crate::coordinator::BuildKey {
        attempt: build.attempt + 1,
        ..build
    });
    rig.claude
        .picture_granted(elsewhere, Some(ConnectionKind::ClaudeCode), other.clone());
    assert!(!other.is_open());
    let key = crate::coordinator::PictureGrant::open_for_tests();
    rig.claude
        .picture_granted(elsewhere, Some(ConnectionKind::AnthropicKey), key.clone());
    assert!(key.is_open());
    drop(question);
    rig.close().await;
}

/// Only an actual move of the generation reports a picture loss, once; a Find that only queues
/// behind one already out doesn't.
#[tokio::test]
async fn only_a_generation_that_moved_reports_a_picture_loss_once() {
    let script = synthetic_signed_in().held_until("go");
    let mut rig = Rig::new("question-picture-loss", &script).await;
    rig.claude.take_picture_loss();
    assert!(!rig.claude.take_picture_loss());
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert!(rig.claude.take_picture_loss());
    assert!(!rig.claude.take_picture_loss());
    rig.claude.op(ConnectionOp::FindClaudeCode);
    assert!(
        !rig.claude.take_picture_loss(),
        "queued behind the Find already out"
    );
    rig.close().await;
}

/// F6 through the brain's own grant: a Claude Code picture grant for the asked build is kept
/// until a generation bump closes that same permit, one for an unasked Claude Code build is
/// closed at once, and an Anthropic key's grant is never touched by either.
#[tokio::test]
async fn the_brains_grant_keeps_only_the_asked_claude_code_picture_and_never_a_keys() {
    use crate::coordinator::{Effect, PictureRoutes};
    for (kind, ask, granted_open, bumped_open) in [
        (ConnectionKind::ClaudeCode, true, true, false),
        (ConnectionKind::ClaudeCode, false, false, false),
        (ConnectionKind::AnthropicKey, false, true, true),
    ] {
        let script = synthetic_signed_in().held_until("go");
        let mut rig = Rig::new("question-picture-brain", &script).await;
        let scratch = crate::test_support::Scratch::new("question-picture-brain");
        let mut brain = crate::test_support::brain(&scratch, "");
        std::mem::swap(&mut brain.claude, &mut rig.claude);
        let (core, built) = crate::coordinator::pictured_run(kind, PictureRoutes::new(true, true));
        *brain.coordinator = core;
        let mut request = built
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartEngine { request } => Some(request),
                _ => None,
            })
            .expect("started");
        request.request.choice = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode)
            .request
            .choice;
        let question = ask.then(|| brain.claude.question(&request).expect("admitted"));
        let granted = brain
            .coordinator
            .handle(Input::SendReady {
                key: request.key,
                hold: None,
            })
            .expect("current");
        let lease = granted
            .iter()
            .find_map(|effect| match effect {
                Effect::Grant(lease) => Some(*lease),
                _ => None,
            })
            .expect("granted");
        let permit = brain
            .coordinator
            .picture_grant(lease)
            .expect("a revocable grant");
        brain.apply(granted, None).expect("no link");
        assert_eq!(permit.is_open(), granted_open, "{kind:?} asked {ask}");
        brain
            .claude_op(ConnectionOp::FindClaudeCode, None)
            .expect("no link");
        assert_eq!(permit.is_open(), bumped_open, "{kind:?} asked {ask}");
        drop(question);
        std::mem::swap(&mut brain.claude, &mut rig.claude);
        rig.close().await;
        brain.close().await;
    }
}

/// A generation move is a new offer of Claude Code's only: a Claude Code offer that the new
/// generation finds unchanged goes again once under a new revision, and an unchanged `ChatGPT`
/// offer doesn't move.
#[tokio::test]
async fn a_generation_move_revises_only_a_claude_code_offer() {
    use crate::context::ConnectionTag;
    use crate::coordinator::{Availability, PictureRoutes, Ready};
    use crate::runtime::hold::tests::{sees_on_sol, set};
    use kumochi_protocol::message::{
        ControlToShell, Effort, ModelInfo, ModelName, ModelSlug, NoticeRevision, ScreenHolds,
        SettingsOp,
    };
    let seen = ModelSlug::new("seen").expect("slug");
    let claude_ready = || {
        Input::Engines(
            ConnectionKind::ClaudeCode,
            Availability::Ready(Ready {
                connection: ConnectionTag(5),
                models: vec![ModelInfo {
                    slug: seen.clone(),
                    display_name: ModelName::new("Seen").expect("name"),
                    supported_efforts: None,
                }],
                images: vec![seen.clone()],
                trusted_draw: false,
            }),
        )
    };
    let mut moved = Vec::new();
    for claude in [false, true] {
        let script = synthetic_signed_in().held_until("go");
        let mut rig = Rig::new("question-gate-generation", &script).await;
        rig.claude.take_picture_loss();
        let scratch = crate::test_support::Scratch::new("question-gate-generation");
        let mut brain = crate::test_support::brain(&scratch, "");
        std::mem::swap(&mut brain.claude, &mut rig.claude);
        let (_screen, _sol, _luna) = sees_on_sol(&mut brain);
        if claude {
            brain
                .coordinator
                .set_pictures(PictureRoutes::new(false, true));
            brain
                .coordinator
                .handle(claude_ready())
                .expect("an availability is never stale");
            set(
                &mut brain,
                SettingsOp::SetDefaultModel(ModelChoice {
                    model: seen.clone(),
                    effort: Effort::Low,
                    connection: ConnectionKind::ClaudeCode,
                }),
            );
        }
        let (out, mut output) = mpsc::channel(16);
        let queue = std::cell::RefCell::new(crate::runtime::data::DataQueue::new(
            crate::runtime::window(),
        ));
        let ready = tokio::sync::Notify::new();
        let mut tap = crate::tap::Tap::from_env();
        let mut link = crate::runtime::Link::new(&out, &queue, &ready, &mut tap);
        let mut gates = || {
            std::iter::from_fn(|| output.try_recv().ok())
                .filter_map(|bytes| {
                    match kumochi_protocol::frame::decode(
                        &bytes[kumochi_protocol::frame::HEADER_LEN..],
                    ) {
                        Ok(ControlToShell::ScreenHolds(ScreenHolds::On { revision, .. })) => {
                            Some(Some(revision))
                        }
                        Ok(ControlToShell::ScreenHolds(ScreenHolds::Off(_))) => Some(None),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        brain.screen_holds_changed(&mut link).expect("room");
        assert_eq!(gates(), [Some(NoticeRevision::FIRST)], "claude {claude}");
        brain
            .claude_op(ConnectionOp::FindClaudeCode, None)
            .expect("no link");
        // The new generation finds the same model on the same sign-in.
        brain
            .coordinator
            .handle(claude_ready())
            .expect("an availability is never stale");
        brain.screen_holds_changed(&mut link).expect("room");
        moved.push(gates());
        brain.screen_holds_changed(&mut link).expect("room");
        assert_eq!(gates(), [], "an identical wake sends nothing");
        drop(link);
        std::mem::swap(&mut brain.claude, &mut rig.claude);
        rig.close().await;
        brain.close().await;
    }
    assert_eq!(moved, [vec![], vec![Some(NoticeRevision::FIRST.next())]]);
}
