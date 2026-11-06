//! The Connections frames the shell actually gets, through each place that sends one: Claude
//! Code's results, the account's results, and a new connection's first frames.

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionOp, ConnectionsView, ControlToBrain, ControlToShell, KeyView,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::time::timeout;

use super::{Ended, PATIENCE, Shell, duplex};
use crate::engines::claude_code::lane::Lane;
use crate::engines::claude_code::lane::tests::{Base, host, soon};
use crate::runtime::claude::tests::{READY, brain_with_a_saved_key};
use crate::runtime::claude::{Claude, SignInStore};
use crate::tap::Tap;
use crate::test_support::Scratch;

const BOTH: ConnectionsView = ConnectionsView {
    anthropic: KeyView::Missing,
    claude_code: READY,
};

/// The next Connections frame, answering pings on the way; `None` at the connection's
/// `Scenes`, which follows its first frames.
async fn connections<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>) -> Option<ConnectionsView> {
    loop {
        match timeout(PATIENCE, shell.next_control())
            .await
            .expect("in time")
            .expect("readable")
            .expect("open")
        {
            ControlToShell::Connections(view) => return Some(view),
            ControlToShell::Scenes { .. } => return None,
            ControlToShell::Ping => {
                let pong = frame::encode(&ControlToBrain::Pong, Pipe::Control).expect("encodes");
                shell.control_out.write_all(&pong).await.expect("sent");
            }
            _ => {}
        }
    }
}

/// Every Connections frame up to the first that `done` accepts.
async fn until<S: AsyncRead + AsyncWrite>(
    shell: &mut Shell<S>,
    done: impl Fn(&ConnectionsView) -> bool,
) -> Vec<ConnectionsView> {
    let mut seen = Vec::new();
    loop {
        if let Some(view) = connections(shell).await {
            seen.push(view);
            if done(&view) {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn every_connections_frame_sent_carries_both_current_halves() {
    let scratch = Scratch::new("claude-publish");
    let mut brain = brain_with_a_saved_key(&scratch);
    let base = Base::new("publish");
    let (host, _switches) = host(&base, "in");
    let lane = Lane::start(host).expect("lane");
    brain.attach_claude(Claude::new(
        Some(lane),
        SignInStore::FileWithDacl,
        ClaudeCodeView::NotFound,
    ));
    let mut tap = Tap::from_env();

    let ((control, data), mut shell) = duplex();
    let client = async {
        // The staged key is adopted as the shell connects.
        until(&mut shell, |view| view.anthropic == KeyView::Ready).await;
        shell
            .send(&ControlToBrain::Connection(ConnectionOp::FindClaudeCode))
            .await;
        let found = until(&mut shell, |view| view.claude_code == READY).await;
        assert!(
            found.iter().all(|view| view.anthropic == KeyView::Ready),
            "Claude Code's frames keep the key's half: {found:?}"
        );
        shell
            .send(&ControlToBrain::Connection(
                ConnectionOp::RemoveAnthropicKey,
            ))
            .await;
        let removed = until(&mut shell, |view| view.anthropic == KeyView::Missing).await;
        assert!(
            removed.iter().all(|view| view.claude_code == READY),
            "the account's frames keep Claude Code's half: {removed:?}"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);

    let ((control, data), mut shell) = duplex();
    let client = async {
        let mut opened = Vec::new();
        while let Some(view) = connections(&mut shell).await {
            opened.push(view);
        }
        assert!(!opened.is_empty(), "a new connection gets the view");
        assert!(
            opened.iter().all(|view| *view == BOTH),
            "a new connection's frames: {opened:?}"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert!(brain.claude.close(soon()).await);
    brain.close().await;
}
