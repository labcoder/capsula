//! C5b-1b1 through the core, on the lane's fake in `replay` mode: the warm slot, `take` and
//! each attempt's one report. Every positive fixture is synthetic (§8); no network, no account
//! and no credential store.

use std::fs;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::CLAUDE_CODE_QUARANTINE;
use serde_json::{Value, json};
use tokio::time::timeout;

use super::tests::READY;
use super::*;
use crate::engines::claude_code::lane::tests::{Base, host, soon};
use crate::engines::claude_code::supervisor::tests::{WAIT, until};

const FACTS: &str = include_str!("../../../fixtures/claude-code/facts.jsonl");
const TEXT: &str = include_str!("../../../fixtures/claude-code/text.jsonl");

fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect()
}

/// What the fake answers each stdin line with, by kind.
struct Script {
    initialize: Vec<String>,
    mcp_status: Vec<String>,
    user: Vec<Value>,
}

/// P0's recordings made synthetic and signed in (§8). The edits: Kumochi's request ids; 2.1.306's
/// signed-in `account` shape with made-up values; the launch's frozen tool set, empty for both
/// origins, in `mcp_status` and in init; and init's `apiKeySource` set to `none`.
fn synthetic_signed_in() -> Script {
    let facts = lines(FACTS);
    let mut initialize = facts[0].clone();
    initialize["response"]["request_id"] = json!("k-initialize");
    initialize["response"]["response"]["account"] =
        json!({"subscriptionType": "max", "apiProvider": "firstParty"});
    let mut mcp = facts[1].clone();
    mcp["response"]["request_id"] = json!("k-mcp-status");
    mcp["response"]["response"]["mcpServers"][0]["tools"] = json!([]);
    let mut user = lines(TEXT);
    user[0]["tools"] = json!([]);
    user[0]["apiKeySource"] = json!("none");
    Script {
        initialize: vec![initialize.to_string()],
        mcp_status: vec![mcp.to_string()],
        user,
    }
}

impl Script {
    /// Written as the base's script.
    fn write(&self, base: &Base) {
        let user: Vec<String> = self.user.iter().map(Value::to_string).collect();
        let text = format!(
            "@initialize\n{}\n@mcp_status\n{}\n@user\n{}\n",
            self.initialize.join("\n"),
            self.mcp_status.join("\n"),
            user.join("\n")
        );
        fs::write(base.0.join("replay"), text).expect("the script");
    }
}

/// A connection Ready on the fake, with a clock the test sets.
struct Rig {
    base: Base,
    claude: Claude,
    t0: Instant,
    now: Arc<Mutex<Instant>>,
}

impl Rig {
    async fn new(name: &str, script: &Script) -> Self {
        let base = Base::new(name);
        script.write(&base);
        let (host, _switches) = host(&base, "replay");
        let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
        let t0 = Instant::now();
        let now = Arc::new(Mutex::new(t0));
        let read = Arc::clone(&now);
        claude.clock = Clock(Arc::new(move || {
            *read.lock().unwrap_or_else(PoisonError::into_inner)
        }));
        let mut rig = Self {
            base,
            claude,
            t0,
            now,
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
            .snapshot(ThreadId(1), "claude-test".to_owned(), None, Origin::Kumo)
            .expect("ready")
    }

    fn last(&self) -> (LaunchId, Arc<Control>) {
        let held = self.claude.launches.last().expect("a launch");
        (held.id, Arc::clone(&held.control))
    }

    async fn settle(&mut self) {
        timeout(Duration::from_secs(30), async {
            while self.claude.out.is_some() {
                let wake = self.claude.next().await;
                self.claude.wake(wake);
            }
        })
        .await
        .expect("discovery settles");
    }

    fn stdin(&self) -> Vec<String> {
        fs::read_to_string(self.base.0.join("stdin.log"))
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn markers(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.base.0)
            .expect("the base")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(CLAUDE_CODE_QUARANTINE))
            .collect();
        names.sort();
        names
    }

    async fn close(mut self) {
        assert!(self.claude.close(soon()).await);
    }
}

/// T2.
#[tokio::test]
async fn a_warm_launch_writes_only_its_two_control_lines() {
    let mut rig = Rig::new("question-warm", &synthetic_signed_in()).await;
    rig.claude.warm(rig.snapshot()).expect("warm");
    until("audited", || rig.stdin().len() == 2).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    rig.close().await;
}

type Change = fn(&mut LaunchSnapshot);

/// T3's take half: any change to the snapshot, or a find queued behind the warm launch, takes a
/// fresh attempt and ends the warm one.
#[tokio::test]
async fn any_change_to_the_snapshot_replaces_the_warm_launch() {
    let mut rig = Rig::new("question-replaced", &synthetic_signed_in()).await;
    let changes: [(&str, Change); 4] = [
        ("model", |s| s.model = "claude-other".to_owned()),
        ("effort", |s| s.effort = Some("high".to_owned())),
        ("thread", |s| s.thread = ThreadId(2)),
        ("origin", |s| s.origin = Origin::Task(ThreadId(1))),
    ];
    for (what, change) in changes {
        let snapshot = rig.snapshot();
        let warm = rig.claude.warm(snapshot.clone()).expect("warm");
        let (_, warm_control) = rig.last();
        let mut other = snapshot;
        change(&mut other);
        let attempt = rig.claude.take(&other).expect("taken");
        assert_ne!(rig.last().0, warm, "{what}");
        until("the warm launch's teardown", || warm_control.is_ended()).await;
        drop(attempt);
    }
    let warm = rig.claude.warm(rig.snapshot()).expect("warm");
    rig.claude.op(ConnectionOp::FindClaudeCode);
    let attempt = rig.claude.take(&rig.snapshot()).expect("taken");
    assert_ne!(rig.last().0, warm, "account");
    drop(attempt);
    rig.settle().await;
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
    rig.claude
        .notes
        .0
        .send(Note::Ended {
            id: first,
            outcome: launch::Outcome::Audit(AuditFailed::Tools),
        })
        .expect("open");
    let wake = timeout(WAIT, rig.claude.next()).await.expect("the note");
    rig.claude.wake(wake);
    assert_eq!(rig.claude.view(), READY);
    rig.settle().await;
    assert_eq!(rig.markers(), Vec::<String>::new());
    rig.close().await;
}

/// T5's take half: warm expiry by the injected clock.
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
        let attempt = rig.claude.take(&rig.snapshot()).expect("taken");
        assert_eq!(rig.last().0 == warm, reused, "{after:?}");
        drop(attempt);
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
        rig.claude.wake(wake);
    }
    assert!(rig.claude.warm.is_some(), "kept before its expiry");
    rig.at(Duration::from_secs(30));
    let wake = timeout(WAIT, rig.claude.next()).await.expect("its expiry");
    rig.claude.wake(wake);
    assert!(rig.claude.warm.is_none());
    until("the warm launch's teardown", || warm.is_ended()).await;
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
    let _attempt = rig.claude.take(&rig.snapshot()).expect("taken");
    assert_ne!(rig.last().0, warm);
    rig.close().await;
}
