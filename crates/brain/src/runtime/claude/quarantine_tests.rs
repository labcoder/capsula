//! The quarantine store (Q1–Q6): a version whose audit failed stays refused across Disconnect
//! and restarts, until a different version is found. On the lane's fake, in a scratch base.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kumochi_protocol::constants::CLAUDE_CODE_QUARANTINE;

use super::tests::READY;
use super::*;
use crate::engines::claude_code::install::FindError;
use crate::engines::claude_code::lane::Program;
use crate::engines::claude_code::lane::tests::{Base, V, host_finding, soon};
use crate::engines::claude_code::supervisor::tests::question;
use crate::platform::FAKE_CLAUDE;

/// The unreviewed version the fake's `other` mode prints.
const OTHER: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 307,
};

/// A connection on the fake, whose mode the test changes between finds.
struct Found {
    claude: Claude,
    mode: Arc<Mutex<String>>,
}

impl Found {
    fn start(base: &Base, mode: &str) -> Self {
        let mode = Arc::new(Mutex::new(mode.to_owned()));
        let find_mode = Arc::clone(&mode);
        let (host, _switches) = host_finding(
            base,
            Box::new(move || {
                let mode = find_mode
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                Ok(Program {
                    exe: std::env::current_exe().map_err(|_| FindError::NotFound)?,
                    prefix: vec![FAKE_CLAUDE.into(), mode.into()],
                })
            }),
        );
        Self {
            claude: Claude::start(Lane::start(host).expect("lane"), SIGN_IN),
            mode,
        }
    }

    async fn settle(&mut self) -> ClaudeCodeView {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.claude.out.is_some() {
                let done = self.claude.next().await;
                self.claude.done(done);
            }
        })
        .await
        .expect("discovery settles");
        self.claude.view()
    }

    async fn find(&mut self, mode: &str) -> ClaudeCodeView {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode.to_owned();
        self.claude.op(ConnectionOp::FindClaudeCode);
        self.settle().await
    }

    fn refused(&mut self) -> bool {
        matches!(
            self.claude.launch(question(Duration::from_secs(20))),
            Err(LaunchRefused::NotReady)
        )
    }

    async fn close(mut self) {
        assert!(self.claude.close(soon()).await);
    }
}

fn marker(base: &Base, version: &str) -> PathBuf {
    base.0.join(format!("{CLAUDE_CODE_QUARANTINE}{version}"))
}

/// Every name in the base with the quarantine prefix.
fn markers(base: &Base) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&base.0)
        .expect("the base")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(CLAUDE_CODE_QUARANTINE))
        .collect();
    names.sort();
    names
}

/// Q1 and Q2: a failure writes the version's marker beside Kumochi's folder, so Disconnect's
/// deletion leaves it and the version stays refused.
#[tokio::test]
async fn an_audit_failure_is_marked_and_survives_disconnect() {
    let base = Base::new("quarantine-disconnect");
    let mut found = Found::start(&base, "in");
    assert_eq!(found.settle().await, READY);
    found.claude.audit_failed(V, AuditFailed::Tools);
    assert_eq!(
        found.claude.view(),
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    assert!(found.refused(), "refused before the marker is written");
    assert_eq!(
        found.settle().await,
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    assert_eq!(markers(&base), ["claude-code.quarantine-2.1.306"]);
    assert!(
        fs::metadata(marker(&base, "2.1.306"))
            .expect("marked")
            .is_file()
    );
    assert!(found.refused());

    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folder");
    let left = config.join("left.json");
    fs::write(&left, b"{}").expect("a file in it");
    found.claude.op(ConnectionOp::DisconnectClaudeCode);
    assert_eq!(
        found.settle().await,
        ClaudeCodeView::AuditFailed(AuditFailed::Tools)
    );
    assert!(!left.exists(), "the Disconnect deleted Kumochi's folder");
    assert_eq!(markers(&base), ["claude-code.quarantine-2.1.306"]);
    assert!(found.refused());
    found.close().await;
}

/// Q3 and Q4: a restarted brain on the same version is refused by the marker alone; a different
/// version clears it, and the reviewed one is Ready again.
#[tokio::test]
async fn a_restart_keeps_it_and_a_different_version_clears_it() {
    let base = Base::new("quarantine-restart");
    let mut found = Found::start(&base, "in");
    found.settle().await;
    found.claude.audit_failed(V, AuditFailed::KeySource);
    found.settle().await;
    found.close().await;

    let mut found = Found::start(&base, "in");
    assert_eq!(
        found.settle().await,
        ClaudeCodeView::AuditFailed(AuditFailed::Version)
    );
    assert!(found.refused());
    assert_eq!(
        found.find("in").await,
        ClaudeCodeView::AuditFailed(AuditFailed::Version),
        "a find of the same version clears nothing"
    );
    assert_eq!(markers(&base), ["claude-code.quarantine-2.1.306"]);

    assert_eq!(
        found.find("other").await,
        ClaudeCodeView::Unsupported(Unsupported::Unreviewed(OTHER))
    );
    assert_eq!(markers(&base), Vec::<String>::new());
    assert_eq!(found.find("in").await, READY);
    found.close().await;
}

/// Q4 in one brain: once the marker is written, a different version clears the refusal held in
/// memory too.
#[tokio::test]
async fn a_different_version_clears_the_refusal_held_in_memory() {
    let base = Base::new("quarantine-memory");
    let mut found = Found::start(&base, "in");
    found.settle().await;
    found.claude.audit_failed(V, AuditFailed::Agents);
    found.settle().await;
    assert_eq!(
        found.find("other").await,
        ClaudeCodeView::Unsupported(Unsupported::Unreviewed(OTHER))
    );
    assert_eq!(found.find("in").await, READY);
    assert!(!found.refused(), "a launch is admitted again");
    found.close().await;
}

/// Q5: a folder squatting on the marker's name: the write fails, the refusal held in memory
/// stays for every version, and once the squatter goes the next find writes the marker.
#[tokio::test]
async fn a_failed_marker_write_is_never_taken_as_durable() {
    let base = Base::new("quarantine-squat");
    let mut found = Found::start(&base, "in");
    found.settle().await;
    fs::create_dir(marker(&base, "2.1.306")).expect("a squatter");
    found.claude.audit_failed(V, AuditFailed::Plugins);
    let failed = ClaudeCodeView::AuditFailed(AuditFailed::Plugins);
    assert_eq!(found.settle().await, failed);
    assert_eq!(found.find("other").await, failed, "a different version too");
    assert!(found.refused());

    fs::remove_dir(marker(&base, "2.1.306")).expect("the squatter goes");
    assert_eq!(found.find("in").await, failed);
    assert!(
        fs::metadata(marker(&base, "2.1.306"))
            .expect("marked")
            .is_file()
    );
    assert_eq!(
        found.find("other").await,
        ClaudeCodeView::Unsupported(Unsupported::Unreviewed(OTHER))
    );
    assert_eq!(markers(&base), Vec::<String>::new());
    found.close().await;
}

/// Q6: a name with the prefix that isn't a marker for a version refuses every version, clears
/// nothing, and shows none of its bytes.
#[tokio::test]
async fn an_unreadable_marker_refuses_every_version() {
    for (name, planted) in [
        ("quarantine-unparsable", "claude-code.quarantine-x.y"),
        (
            "quarantine-long",
            "claude-code.quarantine-2.1.306.0000000000000000000000000000",
        ),
        ("quarantine-padded", "claude-code.quarantine-02.1.306"),
    ] {
        let base = Base::new(name);
        fs::write(base.0.join(planted), b"").expect("planted");
        fs::write(marker(&base, "1.0.0"), b"").expect("another version's marker");
        let mut found = Found::start(&base, "in");
        let view = found.settle().await;
        assert_eq!(
            view,
            ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
            "{planted}"
        );
        let shown = format!("{view:?}");
        assert!(!shown.contains("x.y") && !shown.contains("306"), "{shown}");
        assert!(found.refused());
        assert_eq!(
            found.find("other").await,
            ClaudeCodeView::AuditFailed(AuditFailed::Malformed)
        );
        assert_eq!(markers(&base).len(), 2, "nothing is cleared: {planted}");
        found.close().await;
    }
}
