//! `cargo xtask live claude-signin [--disconnect]`: the early Claude Code sign-in trigger. It waits
//! for the dev brain to find Claude Code, then asks for Claude Code's own sign-in, which runs in a
//! console the owner answers, or with `--disconnect` for its sign-out and cleanup. Only the view's
//! state words are printed: never a version, a path, a frame, a client error or anything Claude
//! Code printed.
//!
//! It never sends `FindClaudeCode`: the brain runs discovery as it starts, and a Find that lands
//! while that runs queues a second round, during which the brain drops a Connect. It never sends
//! `CancelClaudeSignIn` either: at the deadline `Quitting` ends the sign-in with the brain's lane.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail, ensure};
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionOp, ControlToBrain, ControlToShell, PolicyFailed, Unsupported,
};

use super::signin::{VIEW, Wire, harness};
use crate::slice2::client::Frame;

/// The harness's whole budget for discovery [guess]. It isn't derived: the lane bounds each
/// call's root run at 10 s, but its teardown and the folder checks come on top.
pub(super) const FIND: Duration = Duration::from_secs(30);
/// How long the sign-in may take to settle: the lane ends `claude`'s login at 5 minutes, then
/// reads the status under 10 s. One deadline, however many views arrive.
const SIGNIN: Duration = Duration::from_secs(330);
/// The harness's whole budget for Disconnect [guess]: the logout, the deletion and discovery
/// again. The lane bounds each call's root run at 10 s; teardown and the deletion come on top.
const CLEANUP: Duration = Duration::from_secs(45);

/// The copied harness: starts the brain with no model budget, signs Claude Code in or out, and
/// quits it.
pub(super) fn inner(ask: Ask) -> Result<()> {
    harness(|wire, out| drive(wire, out, ask))
}

/// What the owner asked of Claude Code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ask {
    SignIn,
    /// `--disconnect`.
    Disconnect,
}

fn drive(wire: &mut impl Wire, out: &mut impl Write, ask: Ask) -> Result<()> {
    let first = answer(wire, "first Connections view")?;
    let found = leave(
        wire,
        first,
        |view| matches!(view, ClaudeCodeView::NotFound | ClaudeCodeView::Finding),
        FIND,
        "finding Claude Code",
    )?
    .unwrap_or(ClaudeCodeView::NotFound);
    writeln!(out, "claude code: {}", state(found))?;
    match ask {
        Ask::SignIn => sign_in(wire, out, found),
        Ask::Disconnect => sign_out(wire, out, found),
    }
}

/// Ok only when Claude Code ends ready.
fn sign_in(wire: &mut impl Wire, out: &mut impl Write, found: ClaudeCodeView) -> Result<()> {
    match found {
        ClaudeCodeView::Ready { .. } => {
            writeln!(out, "already signed in; nothing to do")?;
            return Ok(());
        }
        ClaudeCodeView::SignedOut { .. } => {}
        other => bail!("not signing in while claude code is {}", state(other)),
    }
    send(wire, ConnectionOp::ConnectClaudeCode)?;
    let mut now = ack(wire, found, "ConnectClaudeCode")?;
    if now == ClaudeCodeView::SigningIn {
        writeln!(
            out,
            "signing in: finish in Claude Code's console; waiting at most {SIGNIN:?}"
        )?;
        now = leave(
            wire,
            now,
            |view| view == ClaudeCodeView::SigningIn,
            SIGNIN,
            "signing in",
        )?
        .ok_or_else(|| anyhow!("the sign-in didn't settle in {SIGNIN:?}; Quitting ends it"))?;
    }
    writeln!(out, "claude code: {}", state(now))?;
    ensure!(
        matches!(now, ClaudeCodeView::Ready { .. }),
        "sign-in ended with claude code {}",
        state(now)
    );
    Ok(())
}

/// Ok only when the cleanup finished and left nothing signed in.
fn sign_out(wire: &mut impl Wire, out: &mut impl Write, found: ClaudeCodeView) -> Result<()> {
    send(wire, ConnectionOp::DisconnectClaudeCode)?;
    // Only `CleanupPending` answers it: a late discovery view can land first. Found pending, a
    // failed cleanup the op retries, it's already answered.
    let now = reach(
        wire,
        found,
        ClaudeCodeView::CleanupPending,
        "DisconnectClaudeCode",
    )?;
    writeln!(
        out,
        "disconnecting: waiting at most {CLEANUP:?} for the cleanup"
    )?;
    // A failed deletion stays `CleanupPending` too, so only the deadline tells it apart.
    let end = leave(
        wire,
        now,
        |view| view == ClaudeCodeView::CleanupPending,
        CLEANUP,
        "cleaning up",
    )?
    .ok_or_else(|| {
        anyhow!("the cleanup didn't finish in {CLEANUP:?}; the brain retries it as it starts")
    })?;
    writeln!(out, "claude code: {}", state(end))?;
    ensure!(
        matches!(
            end,
            ClaudeCodeView::SignedOut { .. } | ClaudeCodeView::NotFound
        ),
        "disconnect ended with claude code {}",
        state(end)
    );
    Ok(())
}

enum Next {
    View(ClaudeCodeView),
    Closed,
    TimedOut,
}

/// Claude Code's half of the next Connections view; every other frame is skipped unread.
fn next(wire: &mut impl Wire, limit: Duration) -> Next {
    let picked = wire.until(limit, |frame| match frame {
        Frame::Control(ControlToShell::Connections(view)) => Some(Next::View(view.claude_code)),
        Frame::Closed => Some(Next::Closed),
        _ => None,
    });
    // As in `signin`: Closed is picked first, so an error means the deadline, and its text, which
    // holds a Debug tail of frames, is dropped.
    picked.unwrap_or(Next::TimedOut)
}

/// The view the brain owes within [`VIEW`].
fn answer(wire: &mut impl Wire, what: &str) -> Result<ClaudeCodeView> {
    match next(wire, VIEW) {
        Next::View(view) => Ok(view),
        Next::Closed => bail!("the brain closed the pipe before the {what}"),
        Next::TimedOut => bail!("no {what} in {VIEW:?}"),
    }
}

/// The op's answer: the first view within [`VIEW`] that isn't `before`, one deadline for all.
/// Snapshots the brain sends for other reasons, such as the Anthropic key's checks, may still
/// show `before` after the op was sent.
fn ack(wire: &mut impl Wire, before: ClaudeCodeView, op: &str) -> Result<ClaudeCodeView> {
    leave(
        wire,
        before,
        |view| view == before,
        VIEW,
        "waiting for an answer",
    )?
    .ok_or_else(|| anyhow!("no answer to {op} in {VIEW:?}"))
}

/// Reads views from `view` on until one is `want`, within [`VIEW`]; `view` itself may be it.
fn reach(
    wire: &mut impl Wire,
    mut view: ClaudeCodeView,
    want: ClaudeCodeView,
    op: &str,
) -> Result<ClaudeCodeView> {
    let deadline = Instant::now() + VIEW;
    while view != want {
        view = match next(wire, deadline.saturating_duration_since(Instant::now())) {
            Next::View(view) => view,
            Next::Closed => bail!("the brain closed the pipe while waiting for an answer"),
            Next::TimedOut => bail!(
                "no answer to {op} in {VIEW:?}; claude code is {}",
                state(view)
            ),
        };
    }
    Ok(view)
}

/// Reads views while `stay` matches, for at most `limit`; `None` past it.
fn leave(
    wire: &mut impl Wire,
    mut view: ClaudeCodeView,
    stay: impl Fn(ClaudeCodeView) -> bool,
    limit: Duration,
    what: &str,
) -> Result<Option<ClaudeCodeView>> {
    let deadline = Instant::now() + limit;
    while stay(view) {
        view = match next(wire, deadline.saturating_duration_since(Instant::now())) {
            Next::View(view) => view,
            Next::Closed => bail!("the brain closed the pipe while {what}"),
            Next::TimedOut => return Ok(None),
        };
    }
    Ok(Some(view))
}

fn send(wire: &mut impl Wire, op: ConnectionOp) -> Result<()> {
    wire.send(&ControlToBrain::Connection(op))
        .map_err(|_pipe| anyhow!("couldn't send {op:?} to the brain"))
}

/// The view's state in words, from its enums only: a version is never formatted.
pub(super) fn state(view: ClaudeCodeView) -> String {
    match view {
        ClaudeCodeView::TurnedOff => "turned off".to_owned(),
        ClaudeCodeView::Finding => "finding".to_owned(),
        ClaudeCodeView::NotFound => "not found".to_owned(),
        ClaudeCodeView::Unsupported(why) => {
            let why = match why {
                Unsupported::Shim => "a shim",
                Unsupported::SignInStore => "sign-in store",
                Unsupported::Unreviewed(_) => "unreviewed version",
            };
            format!("unsupported: {why}")
        }
        ClaudeCodeView::SignedOut { version: _ } => "signed out".to_owned(),
        ClaudeCodeView::SigningIn => "signing in".to_owned(),
        ClaudeCodeView::Ready {
            version: _,
            analytics_on,
        } => if analytics_on {
            "ready, analytics on"
        } else {
            "ready"
        }
        .to_owned(),
        // Its checks carry no value, only which one failed.
        ClaudeCodeView::AuditFailed(check) => format!("audit failed: {check:?}"),
        ClaudeCodeView::PolicyFailed(policy) => format!(
            "policy failed: {}",
            match policy {
                PolicyFailed::ManagedPolicy => "managed policy",
                PolicyFailed::TurnCap => "turn cap",
            }
        ),
        ClaudeCodeView::CleanupPending => "cleanup pending".to_owned(),
        ClaudeCodeView::InUse => "in use".to_owned(),
        ClaudeCodeView::Unavailable => "unavailable".to_owned(),
        ClaudeCodeView::Busy => "busy".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::thread;
    use std::time::Duration;

    use anyhow::{Result, anyhow, bail};
    use kumochi_protocol::message::{
        AccountEmail, AccountView, AuditFailed, ClaudeCodeView, ClaudeVersion, ConnectionOp,
        ConnectionsView, ControlToBrain, ControlToShell, DataToShell, KeyView, PlanState,
        PolicyFailed, RunId, Unsupported,
    };

    use super::{Ask, CLEANUP, FIND, SIGNIN, VIEW, Wire, drive, state};
    use crate::live::signin::session;
    use crate::slice2::client::Frame;

    /// Every planted value starts with this, so one search finds any of them.
    const SENTINEL: &str = "kumochisentinel";
    const EMAIL: &str = "kumochisentinel-email@example.invalid";
    const TOKEN: &str = "kumochisentinel-token-7f3a";
    const PATH: &str = r"C:\Users\kumochisentinel\AppData\Local\Kumochi Dev\claude";
    /// A version no state may print.
    const VERSION: ClaudeVersion = ClaudeVersion {
        major: 4919,
        minor: 4919,
        patch: 4919,
    };

    /// A fixture brain at the client's boundary, as in `signin`'s tests.
    #[derive(Default)]
    struct Fixture {
        frames: VecDeque<Result<Frame>>,
        sent: Vec<ControlToBrain>,
        refuse: Option<ControlToBrain>,
        limits: Vec<Duration>,
    }

    impl Wire for &mut Fixture {
        fn send(&mut self, message: &ControlToBrain) -> Result<()> {
            if self.refuse.as_ref() == Some(message) {
                bail!("WriteFile failed at {PATH} holding {TOKEN}");
            }
            self.sent.push(message.clone());
            Ok(())
        }

        fn until<T>(
            &mut self,
            limit: Duration,
            mut pick: impl FnMut(&Frame) -> Option<T>,
        ) -> Result<T> {
            self.limits.push(limit);
            while let Some(frame) = self.frames.pop_front() {
                if let Some(picked) = pick(&frame?) {
                    thread::sleep(Duration::from_millis(2));
                    return Ok(picked);
                }
            }
            bail!("nothing expected arrived in {limit:?}; last: {TOKEN} {PATH}")
        }
    }

    /// Frames besides Claude Code's view: the account's, with a planted email, and text as
    /// Claude Code might print it, with a planted path and token.
    fn unrelated() -> Vec<Result<Frame>> {
        vec![
            Ok(Frame::Control(ControlToShell::Account(
                AccountView::SignedIn {
                    email: Some(AccountEmail::new(EMAIL).expect("a valid email")),
                    plan: PlanState::Enabled,
                    recovery: None,
                },
            ))),
            Ok(Frame::Data(
                DataToShell::AssistantText {
                    run: RunId(3),
                    delta: format!("Login successful. Credentials saved to {PATH} {TOKEN}").into(),
                },
                0,
            )),
        ]
    }

    enum Step {
        View(ClaudeCodeView),
        /// A snapshot the brain sent for the key's sake: Claude Code's half and the key's.
        Snapshot(ClaudeCodeView, KeyView),
        Closed,
        /// The client's deadline, with the tail it would print.
        Late,
    }

    fn connections(claude_code: ClaudeCodeView, anthropic: KeyView) -> Frame {
        Frame::Control(ControlToShell::Connections(ConnectionsView {
            anthropic,
            anthropic_revision: None,
            claude_code,
        }))
    }

    fn fixture(steps: Vec<Step>) -> Fixture {
        let mut frames = VecDeque::new();
        for step in steps {
            frames.extend(unrelated());
            frames.push_back(match step {
                Step::View(claude_code) => Ok(connections(claude_code, KeyView::Missing)),
                Step::Snapshot(claude_code, anthropic) => Ok(connections(claude_code, anthropic)),
                Step::Closed => Ok(Frame::Closed),
                Step::Late => Err(anyhow!("nothing expected arrived; last: {TOKEN} {PATH}")),
            });
        }
        Fixture {
            frames,
            ..Fixture::default()
        }
    }

    /// Runs the production session over the fixture: stdout, the error's text, and whether
    /// Quitting was sent. Nothing planted, and no version, may appear in stdout or the error.
    fn run(fixture: &mut Fixture, ask: Ask) -> (String, Option<String>, bool) {
        let mut out = Vec::new();
        let (result, quit_sent) = session(Ok(&mut *fixture), &mut out, |wire, out| {
            drive(wire, out, ask)
        });
        let out = String::from_utf8(out).expect("utf-8");
        let printed = result.as_ref().err().map(|error| format!("{error:?}"));
        let error = result.err().map(|error| format!("{error:#}"));
        for text in [Some(&out), printed.as_ref(), error.as_ref()]
            .into_iter()
            .flatten()
        {
            assert!(!text.contains(SENTINEL), "a planted value leaked: {text}");
            assert!(!text.contains("4919"), "a version leaked: {text}");
        }
        (out, error, quit_sent)
    }

    const CONNECT: ControlToBrain = ControlToBrain::Connection(ConnectionOp::ConnectClaudeCode);
    const DISCONNECT: ControlToBrain =
        ControlToBrain::Connection(ConnectionOp::DisconnectClaudeCode);
    const QUIT: ControlToBrain = ControlToBrain::Quitting;
    const WAITING: &str = "signing in: finish in Claude Code's console; waiting at most 330s\n";
    const CLEANING: &str = "disconnecting: waiting at most 45s for the cleanup\n";

    fn signed_out() -> ClaudeCodeView {
        ClaudeCodeView::SignedOut { version: VERSION }
    }

    fn ready() -> ClaudeCodeView {
        ClaudeCodeView::Ready {
            version: VERSION,
            analytics_on: false,
        }
    }

    #[test]
    fn a_sign_in_waits_for_discovery_and_prints_only_states() {
        let mut brain = fixture(vec![
            Step::View(ClaudeCodeView::NotFound),
            Step::View(ClaudeCodeView::NotFound),
            Step::View(signed_out()),
            Step::View(ClaudeCodeView::SigningIn),
            Step::View(ClaudeCodeView::SigningIn),
            Step::View(ready()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            out,
            format!("claude code: signed out\n{WAITING}claude code: ready\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
        let [first, finding, found, answer, settle, again] = brain.limits[..] else {
            panic!("six waits: {:?}", brain.limits);
        };
        assert_eq!(first, VIEW);
        assert!(answer <= VIEW, "{answer:?}");
        assert!(
            finding <= FIND && found < finding,
            "{finding:?} then {found:?}"
        );
        assert!(
            settle <= SIGNIN && again < settle,
            "{settle:?} then {again:?}"
        );
    }

    #[test]
    fn claude_code_signed_in_is_left_alone() {
        let mut brain = fixture(vec![Step::View(ready())]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            out,
            "claude code: ready\nalready signed in; nothing to do\n"
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn the_authoritative_finding_state_settles_before_connect() {
        let mut brain = fixture(vec![
            Step::View(ClaudeCodeView::Finding),
            Step::View(ClaudeCodeView::Finding),
            Step::View(signed_out()),
            Step::View(ClaudeCodeView::SigningIn),
            Step::View(ready()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            out,
            format!("claude code: signed out\n{WAITING}claude code: ready\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn placeholder_then_finding_still_waits_for_settled_discovery() {
        let mut brain = fixture(vec![
            Step::View(ClaudeCodeView::NotFound),
            Step::View(ClaudeCodeView::Finding),
            Step::View(signed_out()),
            Step::View(ClaudeCodeView::SigningIn),
            Step::View(ready()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            out,
            format!("claude code: signed out\n{WAITING}claude code: ready\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn an_unavailable_sign_in_stops_at_once_without_an_op() {
        let cases = [
            (
                ClaudeCodeView::Unsupported(Unsupported::SignInStore),
                "unsupported: sign-in store",
            ),
            (
                ClaudeCodeView::Unsupported(Unsupported::Shim),
                "unsupported: a shim",
            ),
            (ClaudeCodeView::TurnedOff, "turned off"),
            (ClaudeCodeView::Unavailable, "unavailable"),
            (ClaudeCodeView::InUse, "in use"),
            (ClaudeCodeView::CleanupPending, "cleanup pending"),
            (
                ClaudeCodeView::AuditFailed(AuditFailed::TokenSource),
                "audit failed: TokenSource",
            ),
            (
                ClaudeCodeView::PolicyFailed(PolicyFailed::ManagedPolicy),
                "policy failed: managed policy",
            ),
        ];
        for (found, words) in cases {
            // Today's main ships `Undecided`: discovery ends at the first case.
            let mut brain = fixture(vec![
                Step::View(ClaudeCodeView::NotFound),
                Step::View(found),
            ]);
            let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
            assert_eq!(out, format!("claude code: {words}\n"));
            assert_eq!(
                error.as_deref(),
                Some(format!("not signing in while claude code is {words}").as_str())
            );
            assert_eq!(brain.sent, [QUIT]);
            assert!(quit_sent);
            assert_eq!(brain.limits.len(), 2, "no wait after the view: {words}");
        }
    }

    #[test]
    fn not_found_past_the_discovery_deadline_is_the_answer() {
        let mut brain = fixture(vec![Step::View(ClaudeCodeView::NotFound), Step::Late]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(out, "claude code: not found\n");
        assert_eq!(
            error.as_deref(),
            Some("not signing in while claude code is not found")
        );
        assert_eq!(brain.sent, [QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_sign_in_that_ends_anywhere_but_ready_fails() {
        for (end, words) in [
            (signed_out(), "signed out"),
            (
                ClaudeCodeView::AuditFailed(AuditFailed::Deadline),
                "audit failed: Deadline",
            ),
            (ClaudeCodeView::Unavailable, "unavailable"),
        ] {
            let mut brain = fixture(vec![
                Step::View(signed_out()),
                Step::View(ClaudeCodeView::SigningIn),
                Step::View(end),
            ]);
            let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
            assert_eq!(
                out,
                format!("claude code: signed out\n{WAITING}claude code: {words}\n")
            );
            assert_eq!(
                error.as_deref(),
                Some(format!("sign-in ended with claude code {words}").as_str())
            );
            assert_eq!(brain.sent, [CONNECT, QUIT]);
            assert!(quit_sent);
        }
    }

    #[test]
    fn a_connect_with_no_answer_fails_at_one_deadline() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::Snapshot(signed_out(), KeyView::Checking),
            Step::Snapshot(signed_out(), KeyView::Ready),
            Step::Late,
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(out, "claude code: signed out\n");
        assert_eq!(
            error.as_deref(),
            Some("no answer to ConnectClaudeCode in 10s")
        );
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
        let [first, one, two, three] = brain.limits[..] else {
            panic!("four waits: {:?}", brain.limits);
        };
        assert_eq!(first, VIEW);
        assert!(
            one <= VIEW && two < one && three < two,
            "{one:?}, {two:?}, {three:?}"
        );
    }

    #[test]
    fn a_snapshot_still_showing_signed_out_isnt_the_connects_answer() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::Snapshot(signed_out(), KeyView::Checking),
            Step::View(ClaudeCodeView::SigningIn),
            Step::View(ready()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            out,
            format!("claude code: signed out\n{WAITING}claude code: ready\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_snapshot_still_showing_ready_isnt_the_disconnects_answer() {
        let mut brain = fixture(vec![
            Step::View(ready()),
            Step::Snapshot(ready(), KeyView::Checking),
            Step::View(ClaudeCodeView::CleanupPending),
            Step::View(signed_out()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::Disconnect);
        assert_eq!(
            out,
            format!("claude code: ready\n{CLEANING}claude code: signed out\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn the_deadline_ends_the_sign_in_by_quitting_without_a_cancel() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::View(ClaudeCodeView::SigningIn),
            Step::Late,
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(out, format!("claude code: signed out\n{WAITING}"));
        assert_eq!(
            error.as_deref(),
            Some("the sign-in didn't settle in 330s; Quitting ends it")
        );
        assert_eq!(brain.sent, [CONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_disconnect_waits_through_the_cleanup() {
        let mut brain = fixture(vec![
            Step::View(ready()),
            Step::View(ClaudeCodeView::CleanupPending),
            Step::View(ClaudeCodeView::CleanupPending),
            Step::View(signed_out()),
        ]);
        let (out, error, quit_sent) = run(&mut brain, Ask::Disconnect);
        assert_eq!(
            out,
            format!("claude code: ready\n{CLEANING}claude code: signed out\n")
        );
        assert_eq!(error, None);
        assert_eq!(brain.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);
        let [first, answer, cleanup, again] = brain.limits[..] else {
            panic!("four waits: {:?}", brain.limits);
        };
        assert_eq!(first, VIEW);
        assert!(answer <= VIEW, "{answer:?}");
        assert!(
            cleanup <= CLEANUP && again < cleanup,
            "{cleanup:?} then {again:?}"
        );
    }

    #[test]
    fn a_disconnect_the_brain_doesnt_take_fails() {
        let off = ClaudeCodeView::Unsupported(Unsupported::SignInStore);
        let mut ignored = fixture(vec![
            Step::View(off),
            Step::Snapshot(off, KeyView::Checking),
            Step::Late,
        ]);
        let (out, error, quit_sent) = run(&mut ignored, Ask::Disconnect);
        assert_eq!(out, "claude code: unsupported: sign-in store\n");
        assert_eq!(
            error.as_deref(),
            Some(
                "no answer to DisconnectClaudeCode in 10s; claude code is unsupported: sign-in store"
            )
        );
        assert_eq!(ignored.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);

        let mut refused = fixture(vec![
            Step::View(ready()),
            Step::View(ClaudeCodeView::Unavailable),
        ]);
        let (_, error, _) = run(&mut refused, Ask::Disconnect);
        assert_eq!(
            error.as_deref(),
            Some("no answer to DisconnectClaudeCode in 10s; claude code is unavailable")
        );
        assert_eq!(refused.sent, [DISCONNECT, QUIT]);
    }

    #[test]
    fn a_late_discovery_view_isnt_the_disconnects_answer() {
        for (late, words) in [(signed_out(), "signed out"), (ready(), "ready")] {
            let mut brain = fixture(vec![
                Step::View(ClaudeCodeView::NotFound),
                Step::Late,
                Step::View(late),
                Step::View(ClaudeCodeView::CleanupPending),
                Step::View(signed_out()),
            ]);
            let (out, error, quit_sent) = run(&mut brain, Ask::Disconnect);
            assert_eq!(
                out,
                format!("claude code: not found\n{CLEANING}claude code: signed out\n"),
                "{words}"
            );
            assert_eq!(error, None, "{words}");
            assert_eq!(brain.sent, [DISCONNECT, QUIT]);
            assert!(quit_sent);
        }
    }

    #[test]
    fn a_pending_cleanup_is_retried_and_waited_for() {
        let mut done = fixture(vec![
            Step::View(ClaudeCodeView::CleanupPending),
            Step::View(signed_out()),
        ]);
        let (out, error, quit_sent) = run(&mut done, Ask::Disconnect);
        assert_eq!(
            out,
            format!("claude code: cleanup pending\n{CLEANING}claude code: signed out\n")
        );
        assert_eq!(error, None);
        assert_eq!(done.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);

        let mut stuck = fixture(vec![
            Step::View(ClaudeCodeView::CleanupPending),
            Step::Snapshot(ClaudeCodeView::CleanupPending, KeyView::Checking),
            Step::Late,
        ]);
        let (out, error, quit_sent) = run(&mut stuck, Ask::Disconnect);
        assert_eq!(out, format!("claude code: cleanup pending\n{CLEANING}"));
        assert_eq!(
            error.as_deref(),
            Some("the cleanup didn't finish in 45s; the brain retries it as it starts")
        );
        assert_eq!(stuck.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_cleanup_that_fails_or_never_finishes_fails() {
        let mut failed = fixture(vec![
            Step::View(ready()),
            Step::View(ClaudeCodeView::CleanupPending),
            Step::View(ClaudeCodeView::InUse),
        ]);
        let (out, error, _) = run(&mut failed, Ask::Disconnect);
        assert_eq!(
            out,
            format!("claude code: ready\n{CLEANING}claude code: in use\n")
        );
        assert_eq!(
            error.as_deref(),
            Some("disconnect ended with claude code in use")
        );

        let mut stuck = fixture(vec![
            Step::View(ready()),
            Step::View(ClaudeCodeView::CleanupPending),
            Step::Late,
        ]);
        let (out, error, quit_sent) = run(&mut stuck, Ask::Disconnect);
        assert_eq!(out, format!("claude code: ready\n{CLEANING}"));
        assert_eq!(
            error.as_deref(),
            Some("the cleanup didn't finish in 45s; the brain retries it as it starts")
        );
        assert_eq!(stuck.sent, [DISCONNECT, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_closed_pipe_a_missing_view_or_a_failed_send_fails_in_plain_words() {
        let mut closed = fixture(vec![
            Step::View(signed_out()),
            Step::View(ClaudeCodeView::SigningIn),
            Step::Closed,
        ]);
        let (_, error, quit_sent) = run(&mut closed, Ask::SignIn);
        assert_eq!(
            error.as_deref(),
            Some("the brain closed the pipe while signing in")
        );
        assert!(quit_sent);

        let mut finding = fixture(vec![Step::View(ClaudeCodeView::NotFound), Step::Closed]);
        let (out, error, _) = run(&mut finding, Ask::SignIn);
        assert_eq!(out, "");
        assert_eq!(
            error.as_deref(),
            Some("the brain closed the pipe while finding Claude Code")
        );

        let mut silent = fixture(vec![Step::Late]);
        let (out, error, _) = run(&mut silent, Ask::Disconnect);
        assert_eq!(out, "");
        assert_eq!(error.as_deref(), Some("no first Connections view in 10s"));

        let mut refused = fixture(vec![Step::View(signed_out())]);
        refused.refuse = Some(CONNECT);
        let (_, error, quit_sent) = run(&mut refused, Ask::SignIn);
        assert_eq!(
            error.as_deref(),
            Some("couldn't send ConnectClaudeCode to the brain")
        );
        assert!(quit_sent);
    }

    #[test]
    fn a_failed_quitting_ends_the_brain() {
        let mut brain = fixture(vec![Step::View(ready())]);
        brain.refuse = Some(QUIT);
        let (_, error, quit_sent) = run(&mut brain, Ask::SignIn);
        assert_eq!(
            error.as_deref(),
            Some("couldn't send Quitting; the brain was ended")
        );
        assert!(!quit_sent);
    }

    #[test]
    fn no_state_prints_a_version() {
        for (view, words) in [
            (
                ClaudeCodeView::Unsupported(Unsupported::Unreviewed(VERSION)),
                "unsupported: unreviewed version",
            ),
            (signed_out(), "signed out"),
            (
                ClaudeCodeView::Ready {
                    version: VERSION,
                    analytics_on: true,
                },
                "ready, analytics on",
            ),
            (
                ClaudeCodeView::PolicyFailed(PolicyFailed::TurnCap),
                "policy failed: turn cap",
            ),
        ] {
            assert_eq!(state(view), words);
        }
    }
}
