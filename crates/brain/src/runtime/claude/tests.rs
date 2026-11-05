use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::KeyView;
use tokio::time::timeout;
use zeroize::Zeroizing;

use super::*;
use crate::connections::account::Worker;
use crate::connections::clock::FixedClock;
use crate::connections::http_client::{Http, HttpError, Request, Response, SharedHttp};
use crate::connections::vault::{Vault, fake::FakeStore};
use crate::engines::claude_code::lane::tests::{Base, V, host, host_finding, soon};
use crate::platform::credential::CredentialStore;
use crate::runtime::account::{Accounts, Net};
use crate::runtime::connections::ConnectionEvent;
use crate::test_support::{self, Scratch};

const READY: ClaudeCodeView = ClaudeCodeView::Ready {
    version: V,
    analytics_on: false,
};

/// Runs the lane's results through the connection until no discovery is out.
async fn settle(claude: &mut Claude) {
    timeout(Duration::from_secs(30), async {
        while claude.finding {
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
        found("view-undecided", "out", SIGN_IN).await,
        ClaudeCodeView::Unsupported(Unsupported::SignInStore)
    );
    assert_eq!(
        found("view-file", "out", SignInStore::FileWithDacl).await,
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
async fn under_the_shipped_gate_connect_has_no_effect() {
    assert_eq!(SIGN_IN, SignInStore::Undecided, "the gate under test ships");
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
    brain.attach_claude(Claude::new(Some(lane), SIGN_IN, ClaudeCodeView::NotFound));
    let before = brain.connections_frame();
    for _ in 0..2 {
        brain
            .claude_op(ConnectionOp::ConnectClaudeCode, None)
            .expect("no link");
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
    assert_eq!(brain.connections_frame(), before);
}

struct EmptyCatalog;

impl Http for EmptyCatalog {
    fn execute(&self, _: Request) -> impl Future<Output = Result<Response, HttpError>> {
        std::future::ready(Ok(Response {
            status: 200,
            request_id: None,
            body: Secret::new(Zeroizing::new(br#"{"data":[],"has_more":false}"#.to_vec())),
        }))
    }
}

/// A brain whose Anthropic key is staged, so `AnthropicKeyStaged` takes it to Ready.
fn brain_with_a_staged_key(scratch: &Scratch) -> Brain {
    let store = FakeStore::default();
    store
        .set(
            "anthropic-key-staged.key-test",
            &Secret::new(Zeroizing::new(b"sk-ant-api-fake-staged-value".to_vec())),
        )
        .expect("fake paste");
    let lock = scratch.db().with_file_name("key-test.lock");
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
        .connection_event(ConnectionEvent::Op(ConnectionOp::AnthropicKeyStaged));
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
    claude.finding = true;
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
    let mut brain = brain_with_a_staged_key(&scratch);
    brain.attach_claude(status_out());
    key_ready(&mut brain).await;
    release(&mut brain);
    assert_eq!(brain.connections_frame(), BOTH_READY);
}

#[tokio::test]
async fn the_keys_frame_carries_claude_codes_view() {
    let scratch = Scratch::new("claude-compose-claude-first");
    let mut brain = brain_with_a_staged_key(&scratch);
    brain.attach_claude(status_out());
    release(&mut brain);
    let frames = key_ready(&mut brain).await;
    let sent: Vec<ControlToShell> = frames.iter().map(|frame| brain.composed(frame)).collect();
    assert_eq!(sent, [BOTH_READY]);
}
