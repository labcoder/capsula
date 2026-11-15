use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::Secret;
use kumochi_protocol::message::KeyView;
use tokio::time::timeout;
use zeroize::Zeroizing;

use super::*;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, HttpError, Request, Response, SharedHttp};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::engines::claude_code::lane::LANE_QUEUE;
use crate::engines::claude_code::lane::tests::{
    Base, V, host, host_finding, locked, marked, soon, started,
};
use crate::platform::Job;
use crate::runtime::account::{Accounts, Net};
use crate::runtime::connections::ConnectionEvent;
use crate::test_support::{self, Scratch};

pub(in crate::runtime) const READY: ClaudeCodeView = ClaudeCodeView::Ready {
    version: V,
    analytics_on: false,
};

/// Runs the lane's results through the connection until no job is out.
async fn settle(claude: &mut Claude) {
    timeout(Duration::from_secs(30), async {
        while claude.out.is_some() {
            let wake = claude.next().await;
            claude.wake(wake);
        }
    })
    .await
    .expect("discovery settles");
}

/// What discovery shows for the fake in `mode` under `sign_in`.
async fn found(name: &str, mode: &str, sign_in: SignInStore) -> ClaudeCodeView {
    let base = Base::new(name);
    let (host, _switches) = host(&base, mode);
    let mut claude = Claude::start(Lane::start(host).expect("lane"), sign_in);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    claude.view()
}

#[tokio::test]
async fn a_found_install_shows_by_its_sign_in_and_the_store() {
    assert_eq!(
        found("view-undecided", "out", SignInStore::Undecided).await,
        ClaudeCodeView::Unsupported(Unsupported::SignInStore)
    );
    assert_eq!(
        found("view-shipped", "out", SIGN_IN).await,
        ClaudeCodeView::SignedOut { version: V }
    );
    assert_eq!(found("view-in", "in", SIGN_IN).await, READY);
}

#[test]
fn finding_without_a_lane_keeps_not_found() {
    let mut claude = Claude::default();
    claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(claude.view(), ClaudeCodeView::NotFound);
}

#[tokio::test]
async fn finding_preserves_ready_and_blocks_connect_until_discovery_settles() {
    let base = Base::new("finding-view");
    let (host, _switches) = host(&base, "out");
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
    assert_eq!(claude.view(), ClaudeCodeView::Finding);
    claude.op(ConnectionOp::ConnectClaudeCode);
    assert_eq!(claude.out, Some(Work::Version));
    claude.op(ConnectionOp::FindClaudeCode);
    let done = claude.next().await;
    assert!(!claude.wake(done));
    assert_eq!(claude.view(), ClaudeCodeView::Finding);
    let done = claude.next().await;
    assert!(claude.wake(done));
    assert_eq!(claude.view(), ClaudeCodeView::Finding);
    settle(&mut claude).await;
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(claude.view(), ClaudeCodeView::Finding);
    settle(&mut claude).await;
    claude.view = READY;
    claude.op(ConnectionOp::FindClaudeCode);
    assert_eq!(claude.view(), READY);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert!(base.calls().iter().all(|call| call != "auth login"));
}

#[tokio::test]
async fn finds_asked_for_while_one_runs_coalesce_into_one_more() {
    let base = Base::new("coalesce");
    let (host, _switches) = host(&base, "out");
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
    claude.op(ConnectionOp::FindClaudeCode);
    claude.op(ConnectionOp::FindClaudeCode);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(
        base.calls(),
        [
            "--version",
            "auth status --json",
            "--version",
            "auth status --json"
        ]
    );
}

#[tokio::test]
async fn under_the_undecided_gate_connect_has_no_effect() {
    let scratch = Scratch::new("claude-connect");
    let mut brain = test_support::brain(&scratch, "");
    let base = Base::new("connect");
    let finds = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&finds);
    let (host, _switches) = host_finding(
        &base,
        Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(FindError::NotFound)
        }),
    );
    let lane = Lane::start(host).expect("lane");
    brain.attach_claude(Claude::new(
        Some(lane),
        SignInStore::Undecided,
        ClaudeCodeView::NotFound,
    ));
    let before = brain.connections_frame();
    for op in [
        ConnectionOp::ConnectClaudeCode,
        ConnectionOp::CancelClaudeSignIn,
        ConnectionOp::DisconnectClaudeCode,
    ] {
        for _ in 0..2 {
            brain.claude_op(op, None).expect("no link");
        }
    }
    assert_eq!(brain.connections_frame(), before);
    brain
        .claude_op(ConnectionOp::FindClaudeCode, None)
        .expect("no link");
    settle(&mut brain.claude).await;
    assert!(brain.claude.close(soon()).await);
    assert_eq!(
        finds.load(Ordering::SeqCst),
        1,
        "only the Find ran: the lane is in order, so a job from Connect would have run first"
    );
    assert!(!base.owned().exists(), "no folder was made");
    assert!(!base.marker().exists(), "nothing was marked for deletion");
    assert_eq!(brain.connections_frame(), before);
}

/// Claude Code is reported when it's attached, before any shell connects, so a turn never meets
/// the coordinator's start placeholder: with no lane it reads not found at once.
#[test]
fn attaching_claude_code_reports_it_at_once() {
    let scratch = Scratch::new("claude-attach");
    let mut brain = test_support::brain(&scratch, "");
    brain.attach_claude(Claude::default());
    assert_eq!(
        brain.coordinator.claude_code(),
        &Availability::Unavailable(
            kumochi_protocol::message::UserError::new(
                ErrorKey::ClaudeCodeNotFound,
                Vec::new(),
                vec![kumochi_protocol::message::Action::OpenSettings],
            )
            .expect("one action")
        )
    );
}

struct EmptyCatalog;

impl Http for EmptyCatalog {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> {
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            meta: crate::connections::http_client::ResponseMeta::default(),
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        }))
    }
}

/// A brain with a saved Anthropic key, so `CheckAnthropicKey` takes it to Ready.
pub(in crate::runtime) fn brain_with_a_saved_key(scratch: &Scratch) -> Brain {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    Vault::with_service(store.clone(), "key-test", &lock)
        .expect("seed vault")
        .save_anthropic_key(b"sk-ant-api-fake-saved-value")
        .expect("saved key");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let accounts = Accounts::spawn_unconfigured(
        worker,
        Net {
            http: Arc::new(|| Ok(SharedHttp::new(EmptyCatalog))),
            ..test_support::net()
        },
    )
    .expect("account worker");
    test_support::brain_with(scratch, "", accounts)
}

/// Takes the key to Ready through A1's own events, and returns the frames its last one sent.
async fn key_ready(brain: &mut Brain) -> Vec<ControlToShell> {
    brain
        .account
        .connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    timeout(Duration::from_secs(10), async {
        loop {
            let event = brain.account.next().await;
            let frames = brain.account.handle(event);
            if brain.account.connections.anthropic() == KeyView::Ready {
                return frames;
            }
        }
    })
    .await
    .expect("the key checks out")
}

/// Claude Code with a Status out under generation 1, which `release` answers signed in.
fn status_out() -> Claude {
    let mut claude = Claude::new(None, SignInStore::FileWithDacl, ClaudeCodeView::NotFound);
    claude.generation = 1;
    claude.out = Some(Work::Status);
    claude
}

fn release(brain: &mut Brain) {
    let done = Done {
        generation: 1,
        result: Ok(Outcome::Status {
            version: V,
            signed_in: true,
        }),
    };
    brain.claude_done(Wake::Lane(done), None).expect("no link");
}

const BOTH_READY: ControlToShell = ControlToShell::Connections(ConnectionsView {
    anthropic: KeyView::Ready,
    claude_code: READY,
});

#[tokio::test]
async fn claudes_frame_carries_the_key_that_moved_while_its_status_was_out() {
    let scratch = Scratch::new("claude-compose-key-first");
    let mut brain = brain_with_a_saved_key(&scratch);
    brain.attach_claude(status_out());
    key_ready(&mut brain).await;
    release(&mut brain);
    assert_eq!(brain.connections_frame(), BOTH_READY);
}

#[tokio::test]
async fn the_keys_frame_carries_claude_codes_view() {
    let scratch = Scratch::new("claude-compose-claude-first");
    let mut brain = brain_with_a_saved_key(&scratch);
    brain.attach_claude(status_out());
    release(&mut brain);
    let frames = key_ready(&mut brain).await;
    let sent: Vec<ControlToShell> = frames.iter().map(|frame| brain.composed(frame)).collect();
    assert_eq!(sent, [BOTH_READY]);
}

/// A connection that may sign in, on the fake in `mode`, found signed out.
async fn signed_out(base: &Base, mode: &str) -> Claude {
    let (host, _switches) = host(base, mode);
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    claude
}

const FOUND: [&str; 2] = ["--version", "auth status --json"];

#[tokio::test]
async fn connect_signs_in_once_however_often_it_is_asked() {
    let base = Base::new("connect-once");
    let mut claude = signed_out(&base, "out").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    assert_eq!(claude.view(), ClaudeCodeView::SigningIn);
    claude.op(ConnectionOp::ConnectClaudeCode);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), READY);
    assert_eq!(base.calls(), [FOUND[0], FOUND[1], "auth login", FOUND[1]]);
}

#[tokio::test]
async fn cancel_ends_the_sign_in_and_shows_it_signed_out() {
    let base = Base::new("cancel");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    let asked = Instant::now();
    claude.op(ConnectionOp::CancelClaudeSignIn);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "not the fake's 60 s"
    );
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert_eq!(
        base.calls(),
        [
            FOUND[0],
            FOUND[1],
            "auth login",
            FOUND[0],
            FOUND[1],
            FOUND[0],
            FOUND[1]
        ],
        "the Status, then the find after the folder's removal"
    );
}

#[tokio::test]
async fn disconnect_while_signing_in_drops_the_sign_in_and_cleans_up_once() {
    let base = Base::new("disconnect-signing-in");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    claude.op(ConnectionOp::DisconnectClaudeCode);
    claude.op(ConnectionOp::DisconnectClaudeCode);
    assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert_eq!(
        base.calls(),
        [
            FOUND[0],
            FOUND[1],
            "auth login",
            "auth logout",
            FOUND[0],
            FOUND[1]
        ]
    );
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn a_status_from_before_a_disconnect_cant_show_it_ready() {
    let base = Base::new("stale-after-disconnect");
    let (host, _switches) = host(&base, "in");
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    assert_eq!(claude.view(), READY);
    let before = claude.generation;
    claude.op(ConnectionOp::DisconnectClaudeCode);
    let late = Done {
        generation: before,
        result: Ok(Outcome::Status {
            version: V,
            signed_in: true,
        }),
    };
    assert!(!claude.done(late), "dropped by its generation");
    assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
}

#[tokio::test]
async fn while_cleanup_is_pending_find_and_connect_start_nothing() {
    let base = Base::new("cleanup-pending");
    let mut claude = signed_out(&base, "out").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    settle(&mut claude).await;
    assert_eq!(claude.view(), READY);
    let busy = base.owned().join("config").join("busy.json");
    std::fs::write(&busy, b"{}").expect("a file Claude Code keeps open");
    let held = locked(&busy);
    claude.op(ConnectionOp::DisconnectClaudeCode);
    settle(&mut claude).await;
    assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    let calls = base.calls();
    for op in [
        ConnectionOp::FindClaudeCode,
        ConnectionOp::ConnectClaudeCode,
        ConnectionOp::DisconnectClaudeCode,
    ] {
        claude.op(op);
        settle(&mut claude).await;
        assert_eq!(claude.view(), ClaudeCodeView::CleanupPending, "{op:?}");
    }
    assert_eq!(base.calls(), calls, "nothing started");
    assert!(base.marker().exists());
    drop(held);
    claude.op(ConnectionOp::FindClaudeCode);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(
        claude.view(),
        ClaudeCodeView::SignedOut { version: V },
        "the sign-in went with the folder"
    );
    assert_eq!(
        base.calls()
            .iter()
            .filter(|call| *call == "auth logout")
            .count(),
        1
    );
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn with_the_off_switch_only_a_marked_deletion_runs() {
    let base = Base::new("off-cleans");
    let left = marked(&base);
    let (host, _switches) = host(&base, "in");
    let mut claude = Claude::turned_off(Some(Lane::start(host).expect("lane")));
    settle(&mut claude).await;
    claude.op(ConnectionOp::FindClaudeCode);
    claude.op(ConnectionOp::ConnectClaudeCode);
    claude.op(ConnectionOp::DisconnectClaudeCode);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::TurnedOff);
    assert!(!left.exists());
    assert!(!base.marker().exists());
    assert_eq!(base.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn another_kumochi_holding_claude_code_shows_it_in_use() {
    let base = Base::new("in-use");
    let (host, switches) = host(&base, "in");
    switches.vault_held.store(false, Ordering::SeqCst);
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::InUse);
    assert_eq!(base.calls(), Vec::<String>::new());
}

/// Held by a test while its lane's worker must wait to end a call's processes.
static ENDING: Mutex<()> = Mutex::new(());

fn end_once_let_go(job: &Job) -> io::Result<()> {
    drop(ENDING.lock().unwrap_or_else(PoisonError::into_inner));
    Job::terminate(job)
}

#[tokio::test]
async fn cancel_pressed_past_the_queue_then_disconnect_still_signs_out_and_cleans_up() {
    let base = Base::new("cancel-burst");
    let (mut host, _switches) = host(&base, "loginhang");
    host.terminate = end_once_let_go;
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    {
        let _ending = ENDING.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..=LANE_QUEUE + 1 {
            claude.op(ConnectionOp::CancelClaudeSignIn);
        }
        assert_eq!(claude.view(), ClaudeCodeView::SigningIn);
        claude.op(ConnectionOp::DisconnectClaudeCode);
        assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    }
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert_eq!(
        base.calls(),
        [
            FOUND[0],
            FOUND[1],
            "auth login",
            "auth logout",
            FOUND[0],
            FOUND[1]
        ]
    );
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn a_disconnect_the_lane_cant_take_marks_nothing_and_can_be_asked_again() {
    let base = Base::new("disconnect-refused");
    let (mut host, _switches) = host(&base, "loginhang");
    host.terminate = end_once_let_go;
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    {
        let _ending = ENDING.lock().unwrap_or_else(PoisonError::into_inner);
        let lane = claude.lane.as_ref().expect("a lane");
        for _ in 0..LANE_QUEUE {
            assert!(lane.submit(0, Work::Version), "an old job fills a slot");
        }
        claude.op(ConnectionOp::DisconnectClaudeCode);
        assert_eq!(claude.view(), ClaudeCodeView::Unavailable);
        assert!(!claude.cleaning, "nothing to finish");
        assert_eq!(claude.out, None);
        // M1: a full queue drains by itself, so it's Busy with Retry, never the stuck lane's
        // restart, for the view and for a question refused meanwhile.
        let busy = kumochi_protocol::message::UserError::new(
            ErrorKey::ClaudeCodeBusy,
            Vec::new(),
            vec![kumochi_protocol::message::Action::Retry],
        )
        .expect("one action");
        assert_eq!(
            claude.availability(),
            Availability::Unavailable(busy.clone())
        );
        assert_eq!(
            claude.refused(LaunchRefused::NotReady),
            crate::engines::EngineFailure::Claude(busy)
        );
    }
    assert!(!base.marker().exists(), "nothing was marked");
    timeout(Duration::from_secs(10), async {
        while !claude.cleaning {
            tokio::time::sleep(Duration::from_millis(10)).await;
            claude.op(ConnectionOp::DisconnectClaudeCode);
        }
    })
    .await
    .expect("taken once the old jobs are gone");
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert_eq!(
        base.calls()
            .iter()
            .filter(|call| *call == "auth logout")
            .count(),
        1
    );
    assert!(!base.marker().exists());
}

/// Leaves every teardown unconfirmed once set; only the test below sets it.
static STUCK_AFTER_FULL: AtomicBool = AtomicBool::new(false);

fn end_once_let_go_unless_stuck(job: &Job) -> io::Result<()> {
    drop(ENDING.lock().unwrap_or_else(PoisonError::into_inner));
    if STUCK_AFTER_FULL.load(Ordering::SeqCst) {
        return Err(io::ErrorKind::Other.into());
    }
    Job::terminate(job)
}

/// L3: a full queue reads Busy only until the lane takes a job, so a lane that's stuck after it
/// says to restart.
#[tokio::test]
async fn a_full_queue_reads_busy_only_until_the_lane_takes_a_job() {
    let base = Base::new("full-then-stuck");
    let (mut host, _switches) = host(&base, "loginhang");
    host.terminate = end_once_let_go_unless_stuck;
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    let reason = |key, action| {
        kumochi_protocol::message::UserError::new(key, Vec::new(), vec![action])
            .expect("one action")
    };
    {
        let _ending = ENDING.lock().unwrap_or_else(PoisonError::into_inner);
        let lane = claude.lane.as_ref().expect("a lane");
        for _ in 0..LANE_QUEUE {
            assert!(lane.submit(0, Work::Version), "an old job fills a slot");
        }
        claude.op(ConnectionOp::DisconnectClaudeCode);
        assert_eq!(
            claude.availability(),
            Availability::Unavailable(reason(
                ErrorKey::ClaudeCodeBusy,
                kumochi_protocol::message::Action::Retry
            ))
        );
        STUCK_AFTER_FULL.store(true, Ordering::SeqCst);
    }
    timeout(Duration::from_secs(10), async {
        while !claude.cleaning {
            tokio::time::sleep(Duration::from_millis(10)).await;
            claude.op(ConnectionOp::DisconnectClaudeCode);
        }
    })
    .await
    .expect("taken once the old jobs are gone");
    settle(&mut claude).await;
    assert_eq!(claude.view(), ClaudeCodeView::Unavailable);
    assert_eq!(
        claude.availability(),
        Availability::Unavailable(reason(
            ErrorKey::ClaudeCodeUnavailable,
            kumochi_protocol::message::Action::OpenSettings
        ))
    );
    // shortcut: the kept lock lives until the test process ends, leaving this scratch folder.
    assert!(!claude.close(soon()).await, "a stuck lane keeps its lock");
}

/// Makes every call's processes unconfirmed once set; only the test below sets it.
static UNCONFIRMED: AtomicBool = AtomicBool::new(false);

fn end_unless_unconfirmed(job: &Job) -> io::Result<()> {
    if UNCONFIRMED.load(Ordering::SeqCst) {
        return Err(io::ErrorKind::Other.into());
    }
    Job::terminate(job)
}

/// A lane on the fake in `mode`, found and settled, whose later calls can't be confirmed ended
/// once [`UNCONFIRMED`] is set.
async fn unconfirmable(base: &Base, mode: &str) -> Claude {
    let (mut host, _switches) = host(base, mode);
    host.terminate = end_unless_unconfirmed;
    let mut claude = Claude::start(Lane::start(host).expect("lane"), SignInStore::FileWithDacl);
    settle(&mut claude).await;
    claude
}

#[tokio::test]
async fn a_stuck_lane_shows_unavailable_never_signing_in_or_ready() {
    let signing = Base::new("stuck-signing-in");
    let ready = Base::new("stuck-ready");
    let mut signing_in = unconfirmable(&signing, "out").await;
    let mut found = unconfirmable(&ready, "in").await;
    assert_eq!(found.view(), READY);
    UNCONFIRMED.store(true, Ordering::SeqCst);

    signing_in.op(ConnectionOp::ConnectClaudeCode);
    assert_eq!(signing_in.view(), ClaudeCodeView::SigningIn);
    settle(&mut signing_in).await;
    assert_eq!(signing_in.view(), ClaudeCodeView::Unavailable);
    found.op(ConnectionOp::FindClaudeCode);
    settle(&mut found).await;
    assert_eq!(found.view(), ClaudeCodeView::Unavailable);

    for (claude, base, calls) in [
        (
            &mut signing_in,
            &signing,
            vec![FOUND[0], FOUND[1], "auth login"],
        ),
        (&mut found, &ready, vec![FOUND[0], FOUND[1], FOUND[0]]),
    ] {
        claude.op(ConnectionOp::FindClaudeCode);
        settle(claude).await;
        assert_eq!(claude.view(), ClaudeCodeView::Unavailable, "still");
        assert_eq!(base.calls(), calls, "nothing started after");
        // shortcut: the kept lock lives until the test process ends, leaving this scratch folder.
        assert!(!claude.close(soon()).await, "close can't confirm it");
    }
}

/// Question launches through the core: who may launch, the core's bounds, and the revocations.
mod launches {
    use std::time::Duration;

    use kumochi_protocol::message::ThreadId;

    use super::*;
    use crate::coordinator::{Effect, StoreOp};
    use crate::engines::claude_code::supervisor::tests::{
        end, failed, pipes, question, questions, until,
    };
    use crate::engines::claude_code::supervisor::{
        LaunchFailed, LaunchRefused, MAX_LAUNCH, Question, Teardown,
    };

    fn on(thread: u64) -> Question {
        Question {
            thread: ThreadId(thread),
            ..question(Duration::from_secs(20))
        }
    }

    /// Ready on the fake in `mode`: found signed out, then signed in through Connect.
    async fn ready(base: &Base, mode: &str) -> Claude {
        let (host, _switches) = host(base, mode);
        let mut claude = Claude::start(Lane::start(host).expect("lane"), SIGN_IN);
        settle(&mut claude).await;
        claude.op(ConnectionOp::ConnectClaudeCode);
        settle(&mut claude).await;
        assert_eq!(claude.view(), READY);
        claude
    }

    fn control_of(claude: &Claude, at: usize) -> Arc<supervisor::Control> {
        Arc::clone(&claude.launches[at].control)
    }

    /// S11.
    #[tokio::test]
    async fn launches_need_ready_and_the_decided_store() {
        let base = Base::new("launch-signed-out");
        let (signed_out, _switches) = host(&base, "deaf");
        let mut claude = Claude::start(Lane::start(signed_out).expect("lane"), SIGN_IN);
        settle(&mut claude).await;
        assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
        assert_eq!(claude.launch(on(1)).err(), Some(LaunchRefused::NotReady));
        assert!(claude.close(soon()).await);

        let undecided_base = Base::new("launch-undecided");
        let (undecided, _switches) = host(&undecided_base, "deaf");
        let lane = Lane::start(undecided).expect("lane");
        let mut claude = Claude::new(Some(lane), SignInStore::Undecided, READY);
        assert_eq!(claude.launch(on(1)).err(), Some(LaunchRefused::NotReady));
        claude.sign_in = SIGN_IN;
        claude.finds = false;
        assert_eq!(claude.launch(on(1)).err(), Some(LaunchRefused::NotReady));
        claude.finds = true;
        claude.cleaning = true;
        assert_eq!(claude.launch(on(1)).err(), Some(LaunchRefused::NotReady));
        assert!(claude.close(soon()).await);
        assert_eq!(questions(&undecided_base), 0);

        assert_eq!(
            Claude::default().launch(on(1)).err(),
            Some(LaunchRefused::NotReady),
            "no lane"
        );
    }

    /// The core's bounds: the lifetime and the lane's room. Nothing refused is queued.
    #[tokio::test]
    async fn the_core_admits_launches_by_lifetime_and_room() {
        let base = Base::new("launch-core-bounds");
        let mut claude = ready(&base, "deaf").await;
        let over = Question {
            lifetime: MAX_LAUNCH + Duration::from_millis(1),
            ..on(1)
        };
        assert_eq!(claude.launch(over).err(), Some(LaunchRefused::Lifetime));
        let longest = Question {
            lifetime: MAX_LAUNCH,
            ..on(1)
        };
        let mut first = claude.launch(longest).expect("admitted");
        let _pipes = pipes(&mut first).await;
        until("the first runs", || questions(&base) == 1).await;
        let mut waiting = Vec::new();
        for _ in 0..LANE_QUEUE {
            waiting.push(claude.launch(on(1)).expect("queued"));
        }
        assert_eq!(claude.launch(on(1)).err(), Some(LaunchRefused::Busy));
        assert_eq!(claude.launches.len(), 1 + LANE_QUEUE);
        drop(waiting);
        assert_eq!(end(first).await, Teardown::Done);
        assert!(claude.close(soon()).await);
        assert_eq!(questions(&base), 1);
    }

    /// S9.
    #[tokio::test]
    async fn disconnect_revokes_before_its_logout() {
        let base = Base::new("launch-disconnect");
        let mut claude = ready(&base, "deaf").await;
        let mut launch = claude.launch(on(1)).expect("admitted");
        let control = control_of(&claude, 0);
        let _pipes = pipes(&mut launch).await;
        until("it runs", || questions(&base) == 1).await;
        claude.op(ConnectionOp::DisconnectClaudeCode);
        until("the Disconnect ended the launch", || control.is_ended()).await;
        settle(&mut claude).await;
        let calls = base.calls();
        let question = calls.iter().position(|call| call.starts_with("-p "));
        let logout = calls.iter().position(|call| call == "auth logout");
        assert!(question < logout, "{calls:?}");
        assert_eq!(end(launch).await, Teardown::Done);
        assert!(claude.close(soon()).await);
    }

    /// S10: deleting a thread revokes its launches and no other.
    #[tokio::test]
    async fn deleting_a_thread_revokes_only_its_launch() {
        let scratch = Scratch::new("claude-delete-launch");
        let mut brain = test_support::brain(&scratch, "");
        let base = Base::new("launch-delete");
        brain.attach_claude(ready(&base, "deaf").await);
        let mut kept = brain.claude.launch(on(1)).expect("admitted");
        let kept_control = control_of(&brain.claude, 0);
        let mut deleted = brain.claude.launch(on(2)).expect("queued");
        let _pipes = pipes(&mut kept).await;
        until("it runs", || questions(&base) == 1).await;
        let delete = |thread| {
            vec![Effect::Store(StoreOp::Delete {
                thread: ThreadId(thread),
            })]
        };
        assert!(brain.apply(delete(2), None).is_ok());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!kept_control.is_ended(), "the other thread's launch lives");
        assert!(brain.apply(delete(1), None).is_ok());
        until("its own deletion ends it", || kept_control.is_ended()).await;
        assert_eq!(failed(&mut deleted).await, LaunchFailed::Revoked);
        assert_eq!(end(kept).await, Teardown::Done);
        assert!(brain.claude.close(soon()).await);
        assert_eq!(questions(&base), 1);
    }

    /// S21: finding again and a round still in progress leave a launch alone; a settled view
    /// that isn't Ready ends it.
    #[tokio::test]
    async fn finding_and_rounds_in_progress_never_revoke() {
        let base = Base::new("launch-finding");
        let mut claude = ready(&base, "deaf").await;
        let mut launch = claude.launch(on(1)).expect("admitted");
        let control = control_of(&claude, 0);
        let _pipes = pipes(&mut launch).await;
        claude.op(ConnectionOp::FindClaudeCode);
        let generation = claude.generation;
        assert!(!claude.done(Done {
            generation,
            result: Ok(Outcome::Version(V)),
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!control.is_ended(), "a Version and a Status in progress");
        assert_eq!(claude.view(), READY);
        assert!(claude.done(Done {
            generation,
            result: Ok(Outcome::Status {
                version: V,
                signed_in: false,
            }),
        }));
        until("signed out ends it", || control.is_ended()).await;
        assert_eq!(end(launch).await, Teardown::Done);
        assert!(claude.close(soon()).await);
    }
}

/// A file Claude Code may leave in its config folder when it's ended part way through a save.
fn partial(base: &Base) -> std::path::PathBuf {
    base.owned().join("config").join("partial")
}

/// A cancel that doesn't end signed in removes Kumochi's Claude Code folder, without a logout,
/// so no sign-in file outlives a view that says signed out.
#[tokio::test]
async fn a_cancel_that_ends_signed_out_removes_claude_codes_folder() {
    let base = Base::new("cancel-removes");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    std::fs::write(partial(&base), b"").expect("a partial save");
    claude.op(ConnectionOp::CancelClaudeSignIn);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert!(!partial(&base).exists());
    assert!(!base.marker().exists());
    assert_eq!(
        base.calls(),
        [
            FOUND[0],
            FOUND[1],
            "auth login",
            FOUND[0],
            FOUND[1],
            FOUND[0],
            FOUND[1]
        ]
    );
}

#[tokio::test]
async fn a_cancel_whose_status_fails_removes_claude_codes_folder() {
    let base = Base::new("cancel-status-fails");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    std::fs::write(partial(&base), b"").expect("a partial save");
    claude.op(ConnectionOp::CancelClaudeSignIn);
    timeout(Duration::from_secs(30), async {
        loop {
            let wake = claude.next().await;
            if let Wake::Lane(Done {
                generation,
                result: Ok(Outcome::Status { .. }),
            }) = wake
            {
                claude.wake(Wake::Lane(Done {
                    generation,
                    result: Err(Failure::Deadline),
                }));
                return;
            }
            claude.wake(wake);
        }
    })
    .await
    .expect("the cancel's Status");
    assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert!(!partial(&base).exists());
}

/// Claude Code finished saving before the cancel landed: the sign-in stands, as Cancel says.
#[tokio::test]
async fn a_cancel_that_ends_signed_in_keeps_the_sign_in() {
    let base = Base::new("cancel-keeps");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    let token = base.owned().join("config").join("signed-in");
    std::fs::write(&token, b"").expect("a finished save");
    claude.op(ConnectionOp::CancelClaudeSignIn);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), READY);
    assert!(token.exists());
    assert_eq!(
        base.calls(),
        [FOUND[0], FOUND[1], "auth login", FOUND[0], FOUND[1]]
    );
}

/// The cleanup belongs to the cancel's own Status: a later job starts clear of it.
#[tokio::test]
async fn a_job_after_a_cancel_starts_without_its_cleanup() {
    let base = Base::new("cancel-then-find");
    let mut claude = signed_out(&base, "out").await;
    claude.cancelled = Some(claude.generation);
    claude.op(ConnectionOp::FindClaudeCode);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert_eq!(base.calls(), [FOUND[0], FOUND[1], FOUND[0], FOUND[1]]);
}

#[tokio::test]
async fn a_cancel_whose_version_fails_removes_claude_codes_folder() {
    let base = Base::new("cancel-version-fails");
    let mut claude = signed_out(&base, "loginhang").await;
    claude.op(ConnectionOp::ConnectClaudeCode);
    started(&base, "auth login").await;
    std::fs::write(partial(&base), b"").expect("a partial save");
    claude.op(ConnectionOp::CancelClaudeSignIn);
    timeout(Duration::from_secs(30), async {
        loop {
            let wake = claude.next().await;
            if let Wake::Lane(Done {
                generation,
                result: Ok(Outcome::Version(_)),
            }) = wake
                && generation == claude.generation
            {
                claude.wake(Wake::Lane(Done {
                    generation,
                    result: Err(Failure::Deadline),
                }));
                return;
            }
            claude.wake(wake);
        }
    })
    .await
    .expect("the cancel's Version");
    assert_eq!(claude.view(), ClaudeCodeView::CleanupPending);
    settle(&mut claude).await;
    assert!(claude.close(soon()).await);
    assert_eq!(claude.view(), ClaudeCodeView::SignedOut { version: V });
    assert!(!partial(&base).exists());
}

/// The pipe a test shell reads: control frames go unread, data frames wait in the queue.
struct Pipe {
    out: tokio::sync::mpsc::Sender<Vec<u8>>,
    _control: tokio::sync::mpsc::Receiver<Vec<u8>>,
    queue: std::cell::RefCell<crate::runtime::data::DataQueue>,
    ready: tokio::sync::Notify,
    tap: crate::tap::Tap,
}

impl Pipe {
    fn new() -> Self {
        let (out, control) = tokio::sync::mpsc::channel(256);
        Self {
            out,
            _control: control,
            queue: std::cell::RefCell::new(crate::runtime::data::DataQueue::new(
                crate::runtime::window(),
            )),
            ready: tokio::sync::Notify::new(),
            tap: crate::tap::Tap::from_env(),
        }
    }

    fn link(&mut self) -> Link<'_, '_> {
        Link::new(&self.out, &self.queue, &self.ready, &mut self.tap)
    }

    /// Claude Code's model lists the shell heard since the last look, oldest first.
    fn heard(&self) -> Vec<Vec<ModelInfo>> {
        let mut queue = self.queue.borrow_mut();
        queue.grant(u32::MAX);
        std::iter::from_fn(|| queue.pop(Instant::now()))
            .filter_map(|bytes| {
                match kumochi_protocol::frame::decode(&bytes[kumochi_protocol::frame::HEADER_LEN..])
                    .expect("a data frame")
                {
                    DataToShell::Models {
                        connection: ConnectionKind::ClaudeCode,
                        models,
                    } => Some(models),
                    _ => None,
                }
            })
            .collect()
    }
}

/// The reviewed row's models, in its order, as the picker shows them.
fn row() -> Vec<ModelInfo> {
    [
        ("sonnet", "Sonnet"),
        ("opus", "Opus"),
        ("haiku", "Haiku"),
        ("fable", "Fable"),
    ]
    .into_iter()
    .map(|(slug, name)| ModelInfo {
        slug: ModelSlug::new(slug).expect("valid"),
        display_name: ModelName::new(name).expect("valid"),
    })
    .collect()
}

const NOTHING: [Vec<ModelInfo>; 0] = [];

/// A brain whose Claude Code is finding the lane's fake, signed in; nothing is reported yet.
fn finding(scratch: &Scratch, base: &Base) -> Brain {
    let mut brain = test_support::brain(scratch, "");
    let (host, _switches) = host(base, "in");
    brain.attach_claude(Claude::start(
        Lane::start(host).expect("lane"),
        SignInStore::FileWithDacl,
    ));
    brain
}

/// Runs the lane's results through the brain with a shell connected, as the runtime does.
async fn reported(brain: &mut Brain, pipe: &mut Pipe) {
    timeout(Duration::from_secs(30), async {
        while brain.claude.out.is_some() {
            let wake = brain.claude.next().await;
            brain
                .claude_done(wake, Some(&mut pipe.link()))
                .expect("room");
        }
    })
    .await
    .expect("the lane settles");
}

/// The picker's catalog: the reviewed row's models go out, in its order, once Claude Code can
/// answer, and not while it's still being found.
#[tokio::test]
async fn claude_codes_models_go_out_in_the_reviewed_rows_order_once_it_can_answer() {
    let scratch = Scratch::new("claude-models-ready");
    let base = Base::new("claude-models-ready");
    let mut brain = finding(&scratch, &base);
    let mut pipe = Pipe::new();
    reported(&mut brain, &mut pipe).await;
    assert_eq!(brain.claude.view(), READY);
    assert_eq!(pipe.heard(), [row()]);
    assert!(brain.claude.close(soon()).await);
}

/// A report that leaves the list as it was sends nothing.
#[tokio::test]
async fn a_report_that_changes_nothing_sends_claude_codes_models_no_more() {
    let scratch = Scratch::new("claude-models-repeat");
    let base = Base::new("claude-models-repeat");
    let mut brain = finding(&scratch, &base);
    let mut pipe = Pipe::new();
    reported(&mut brain, &mut pipe).await;
    assert_eq!(pipe.heard(), [row()]);
    brain
        .claude_op(ConnectionOp::FindClaudeCode, Some(&mut pipe.link()))
        .expect("room");
    reported(&mut brain, &mut pipe).await;
    assert_eq!(base.calls(), [FOUND[0], FOUND[1], FOUND[0], FOUND[1]]);
    assert_eq!(brain.claude.view(), READY);
    assert_eq!(pipe.heard(), NOTHING);
    assert!(brain.claude.close(soon()).await);
}

/// Once a launch wouldn't be admitted, the shell hears an empty list, which takes Claude Code off
/// the picker.
#[tokio::test]
async fn a_disconnect_sends_claude_codes_models_empty() {
    let scratch = Scratch::new("claude-models-disconnect");
    let base = Base::new("claude-models-disconnect");
    let mut brain = finding(&scratch, &base);
    let mut pipe = Pipe::new();
    reported(&mut brain, &mut pipe).await;
    assert_eq!(pipe.heard(), [row()]);
    brain
        .claude_op(ConnectionOp::DisconnectClaudeCode, Some(&mut pipe.link()))
        .expect("room");
    assert_eq!(brain.claude.view(), ClaudeCodeView::CleanupPending);
    assert_eq!(pipe.heard(), [Vec::new()]);
    assert!(brain.claude.close(soon()).await);
}

/// A shell that connects hears the models on offer, whatever an earlier shell heard.
#[tokio::test]
async fn a_new_shell_hears_claude_codes_models_again() {
    let scratch = Scratch::new("claude-models-reconnect");
    let base = Base::new("claude-models-reconnect");
    let mut brain = finding(&scratch, &base);
    let mut pipe = Pipe::new();
    reported(&mut brain, &mut pipe).await;
    assert_eq!(pipe.heard(), [row()]);
    let mut next = Pipe::new();
    brain.open(&mut next.link()).expect("room");
    assert_eq!(next.heard(), [row()]);
    assert!(brain.claude.close(soon()).await);
}

/// Changes while no shell is linked: a shell that connects hears only the list on offer then, and
/// nothing once a launch wouldn't be admitted.
#[tokio::test]
async fn a_shell_that_connects_hears_only_the_models_on_offer_now() {
    let scratch = Scratch::new("claude-models-unlinked");
    let base = Base::new("claude-models-unlinked");
    let mut brain = finding(&scratch, &base);
    timeout(Duration::from_secs(30), async {
        while brain.claude.out.is_some() {
            let wake = brain.claude.next().await;
            brain.claude_done(wake, None).expect("no link");
        }
    })
    .await
    .expect("the lane settles");
    assert_eq!(brain.claude.view(), READY);
    let mut first = Pipe::new();
    brain.open(&mut first.link()).expect("room");
    assert_eq!(first.heard(), [row()]);
    brain
        .claude_op(ConnectionOp::DisconnectClaudeCode, None)
        .expect("no link");
    assert_eq!(brain.claude.view(), ClaudeCodeView::CleanupPending);
    let mut second = Pipe::new();
    brain.open(&mut second.link()).expect("room");
    assert_eq!(second.heard(), NOTHING);
    assert!(brain.claude.close(soon()).await);
}

/// A shell that connects before Claude Code's first report hears nothing of its models until
/// that report.
#[tokio::test]
async fn a_shell_hears_nothing_of_claude_codes_models_before_its_first_report() {
    let scratch = Scratch::new("claude-models-early");
    let base = Base::new("claude-models-early");
    let mut brain = finding(&scratch, &base);
    let mut pipe = Pipe::new();
    brain.open(&mut pipe.link()).expect("room");
    assert_eq!(pipe.heard(), NOTHING);
    reported(&mut brain, &mut pipe).await;
    assert_eq!(pipe.heard(), [row()]);
    assert!(brain.claude.close(soon()).await);
}
