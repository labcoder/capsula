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
            let done = claude.next().await;
            claude.done(done);
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
    let accounts = Accounts::spawn(
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
    brain.claude_done(done, None).expect("no link");
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
        [FOUND[0], FOUND[1], "auth login", FOUND[0], FOUND[1]]
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
        Arc::clone(&claude.launches[at].1)
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
