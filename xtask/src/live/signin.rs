//! `cargo xtask live signin`: the early sign-in trigger. It asks the dev brain to sign in, the
//! brain opens the browser, and only the account's state words are printed: never the email, and
//! never a frame, a client error or its tail, so names, titles and tokens have no way out.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail, ensure};
use kumochi_protocol::message::{
    AccountOp, AccountRecovery, AccountView, Cleanup, ControlToBrain, ControlToShell, PlanState,
};

use super::{Brain, with_cleanup};
use crate::slice2::client::{Client, Frame};

/// How long the brain gets to send a view it owes: the first, or an op's answer.
pub(super) const VIEW: Duration = Duration::from_secs(10);
/// How long the sign-in may take to settle: the brain ends its attempt at 5 minutes, plus room for
/// the exchange's 15 s deadlines and the save. One deadline, however many views arrive.
const SIGNIN: Duration = Duration::from_secs(330);

/// The pipe as the driver sees it: the real client, or a test's frames.
pub(super) trait Wire {
    fn send(&mut self, message: &ControlToBrain) -> Result<()>;
    fn until<T>(&mut self, limit: Duration, pick: impl FnMut(&Frame) -> Option<T>) -> Result<T>;
}

impl Wire for Client {
    fn send(&mut self, message: &ControlToBrain) -> Result<()> {
        self.control(message).map(drop)
    }

    fn until<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&Frame) -> Option<T>,
    ) -> Result<T> {
        Client::until(self, limit, |seen| pick(&seen.frame))
    }
}

/// The copied harness: starts the brain with no model budget, signs in, and quits it.
pub(super) fn inner() -> Result<()> {
    harness(drive)
}

/// Starts the brain with no model budget, runs `drive` over its pipe, and quits it.
pub(super) fn harness(
    drive: impl FnOnce(&mut Client, &mut io::StdoutLock<'static>) -> Result<()>,
) -> Result<()> {
    let brain = Brain::start(0, 0)?;
    let (driven, quit_sent) = session(Client::connect(), &mut io::stdout().lock(), drive);
    let cleanup = if quit_sent {
        brain.quit()
    } else {
        brain.stop()
    };
    with_cleanup(driven, cleanup)
}

/// Runs `drive` over `connected`, then sends `Quitting`; says whether it was sent.
pub(super) fn session<W: Wire, O: Write>(
    connected: Result<W>,
    out: &mut O,
    drive: impl FnOnce(&mut W, &mut O) -> Result<()>,
) -> (Result<()>, bool) {
    let Ok(mut wire) = connected else {
        return (Err(anyhow!("the brain didn't accept the client")), false);
    };
    let driven = drive(&mut wire, out);
    if wire.send(&ControlToBrain::Quitting).is_ok() {
        return (driven, true);
    }
    let quit = Err(anyhow!("couldn't send Quitting; the brain was ended"));
    (with_cleanup(driven, quit), false)
}

/// Ok only when the account ends signed in with nothing to recover.
fn drive(wire: &mut impl Wire, out: &mut impl Write) -> Result<()> {
    let first = answer(wire, "first account view")?;
    writeln!(out, "account: {}", state(&first))?;
    if signed_in(&first) {
        writeln!(out, "already signed in; nothing to do")?;
        return Ok(());
    }
    ensure!(
        matches!(
            first,
            AccountView::SignedOut { .. }
                | AccountView::SignedIn {
                    recovery: Some(AccountRecovery::SignInAgain),
                    ..
                }
        ),
        "not signing in while the account is {}",
        state(&first)
    );
    send(wire, AccountOp::SignIn)?;
    let mut now = answer(wire, "answer to SignIn")?;
    if now == AccountView::SigningIn {
        writeln!(
            out,
            "signing in: finish in the browser; waiting at most {SIGNIN:?}"
        )?;
        let deadline = Instant::now() + SIGNIN;
        while now == AccountView::SigningIn {
            now = match next(wire, deadline.saturating_duration_since(Instant::now())) {
                Next::View(view) => view,
                Next::Closed => bail!("the brain closed the pipe while signing in"),
                Next::TimedOut => {
                    send(wire, AccountOp::CancelSignIn)?;
                    let after = answer(wire, "answer to CancelSignIn")?;
                    writeln!(out, "account: {}", state(&after))?;
                    bail!("sign-in didn't settle in {SIGNIN:?}; it was cancelled");
                }
            };
        }
    }
    writeln!(out, "account: {}", state(&now))?;
    ensure!(
        signed_in(&now),
        "sign-in ended with the account {}",
        state(&now)
    );
    Ok(())
}

pub(super) fn inspect(wire: &mut impl Wire, out: &mut impl Write) -> Result<()> {
    let view = answer(wire, "first account view")?;
    writeln!(out, "account: {}", state(&view))?;
    Ok(())
}

fn signed_in(view: &AccountView) -> bool {
    matches!(view, AccountView::SignedIn { recovery: None, .. })
}

enum Next {
    View(AccountView),
    Closed,
    TimedOut,
}

/// The next account view; every other frame is skipped unread.
fn next(wire: &mut impl Wire, limit: Duration) -> Next {
    let picked = wire.until(limit, |frame| match frame {
        Frame::Control(ControlToShell::Account(view)) => Some(Next::View(view.clone())),
        Frame::Closed => Some(Next::Closed),
        _ => None,
    });
    // The client also fails when both pipes ended or a pipe ended unpicked, but Closed is picked
    // first, so its errors mean the deadline here. Their text holds a Debug tail of frames.
    picked.unwrap_or(Next::TimedOut)
}

/// The view the brain owes within [`VIEW`].
fn answer(wire: &mut impl Wire, what: &str) -> Result<AccountView> {
    match next(wire, VIEW) {
        Next::View(view) => Ok(view),
        Next::Closed => bail!("the brain closed the pipe before the {what}"),
        Next::TimedOut => bail!("no {what} in {VIEW:?}"),
    }
}

fn send(wire: &mut impl Wire, op: AccountOp) -> Result<()> {
    wire.send(&ControlToBrain::AccountOp(op))
        .map_err(|_pipe| anyhow!("couldn't send {op:?} to the brain"))
}

/// The view's state in words, from its enums only: the email is never formatted.
fn state(view: &AccountView) -> String {
    let unconfirmed = |flag: bool| if flag { ", revocation unconfirmed" } else { "" };
    match view {
        AccountView::SignedOut {
            revocation_unconfirmed,
        } => format!("signed out{}", unconfirmed(*revocation_unconfirmed)),
        AccountView::SigningIn => "signing in".to_owned(),
        AccountView::SigningOut {
            cleanup,
            revocation_unconfirmed,
        } => {
            let cleanup = match cleanup {
                Cleanup::Running => "running",
                Cleanup::Refused => "refused",
                Cleanup::NotDurable => "not durable",
            };
            format!(
                "signing out, cleanup {cleanup}{}",
                unconfirmed(*revocation_unconfirmed)
            )
        }
        AccountView::Busy => "busy: another Windows session holds the account".to_owned(),
        AccountView::SignedIn {
            email: _,
            plan,
            recovery,
        } => {
            let plan = match plan {
                PlanState::Enabled => "enabled",
                PlanState::Declined => "declined",
                PlanState::Unavailable => "unavailable",
            };
            let recovery = match recovery {
                None => "",
                Some(AccountRecovery::Retry) => ", needs retry",
                Some(AccountRecovery::SignInAgain) => ", needs sign-in again",
            };
            format!("signed in, plan {plan}{recovery}")
        }
    }
}

#[cfg(test)]
fn assert_one_deadline(mut waits: impl Iterator<Item = (Instant, Duration)>, cap: Duration) {
    let (first, remaining) = waits.next().expect("the fixture reached the phase");
    assert!(
        (cap - Duration::from_secs(1)..=cap).contains(&remaining),
        "initial allowance was {remaining:?}, expected {cap:?}"
    );
    let deadline = first + remaining;
    let tolerance = Duration::from_millis(10);
    for (at, remaining) in waits {
        let later = at + remaining;
        assert!(
            later <= deadline + tolerance,
            "the phase deadline was extended"
        );
        assert!(
            later + tolerance >= deadline,
            "the phase deadline was shortened"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow, bail};
    use kumochi_protocol::message::{
        AccountEmail, AccountOp, AccountRecovery, AccountView, AssistantName, Cleanup,
        ControlToBrain, ControlToShell, DataToShell, MaxFrameRate, PlanState, RunId, SettingsView,
        ThreadId, ThreadKind, ThreadRow, UserName,
    };

    use super::{Wire, assert_one_deadline, drive, inspect, session, state};
    use crate::slice2::client::Frame;

    /// Every planted value starts with this, so one search finds any of them.
    const SENTINEL: &str = "kumochisentinel";
    const EMAIL: &str = "kumochisentinel-email@example.invalid";
    const TOKEN: &str = "kumochisentinel-token-7f3a";
    const NAME: &str = "kumochisentinel-name";
    const TITLE: &str = "kumochisentinel-title";

    /// A fixture brain at the client's boundary: frames as the client's readers would hand them
    /// over, failures as the client reports them, and the messages sent to it.
    #[derive(Default)]
    struct Fixture {
        frames: VecDeque<Result<Frame>>,
        sent: Vec<ControlToBrain>,
        /// A message whose send fails.
        refuse: Option<ControlToBrain>,
        limits: Vec<Duration>,
    }

    impl Wire for &mut Fixture {
        fn send(&mut self, message: &ControlToBrain) -> Result<()> {
            if self.refuse.as_ref() == Some(message) {
                bail!("WriteFile failed while holding {TOKEN}");
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
                    // Time passes between views, as it does on the pipe.
                    thread::sleep(Duration::from_millis(2));
                    return Ok(picked);
                }
            }
            bail!("nothing expected arrived in {limit:?}; last: {TOKEN}")
        }
    }

    /// Frames the brain sends besides the account, each carrying a planted name or title.
    fn unrelated() -> Vec<Result<Frame>> {
        let settings = SettingsView {
            assistant_name: AssistantName::new(NAME).expect("a valid name"),
            user_name: Some(UserName::new(NAME).expect("a valid name")),
            onboarded: true,
            onboarding_from: kumochi_protocol::message::OnboardingStep::Welcome,
            main_thread: ThreadId(1),
            max_frame_rate: MaxFrameRate::default(),
            default_model: None,
            dock_position: kumochi_protocol::message::DockPosition::TopCenter,
            show_in_taskbar: false,
            talk_back: false,
            stt_unload: kumochi_protocol::message::SttUnload::Kept,
            hold_to_talk: kumochi_protocol::message::DEFAULT_HOLD_TO_TALK,
            screen_holds: kumochi_protocol::message::DEFAULT_SCREEN_HOLDS,
            show_screen: kumochi_protocol::message::DEFAULT_SHOW_SCREEN,
        };
        let row = ThreadRow {
            id: ThreadId(2),
            title: TITLE.to_owned(),
            updated_ms: 0,
            kind: ThreadKind::Thread,
            choice: None,
        };
        vec![
            Ok(Frame::Control(ControlToShell::Settings {
                view: settings,
                answer: false,
            })),
            Ok(Frame::Data(
                DataToShell::ThreadList {
                    request: 1,
                    threads: vec![row],
                    next: None,
                },
                0,
            )),
            Ok(Frame::Data(
                DataToShell::AssistantText {
                    run: RunId(3),
                    delta: format!("{NAME} {TITLE}").into(),
                },
                0,
            )),
        ]
    }

    enum Step {
        View(AccountView),
        Closed,
        /// The client's deadline, with the tail it would print.
        Late,
    }

    fn signed_in(recovery: Option<AccountRecovery>) -> AccountView {
        AccountView::SignedIn {
            email: Some(AccountEmail::new(EMAIL).expect("a valid email")),
            plan: PlanState::Enabled,
            recovery,
        }
    }

    fn signed_out() -> AccountView {
        AccountView::SignedOut {
            revocation_unconfirmed: false,
        }
    }

    fn fixture(steps: Vec<Step>) -> Fixture {
        let mut frames = VecDeque::new();
        for step in steps {
            frames.extend(unrelated());
            frames.push_back(match step {
                Step::View(view) => Ok(Frame::Control(ControlToShell::Account(view))),
                Step::Closed => Ok(Frame::Closed),
                Step::Late => Err(anyhow!("nothing expected arrived; last: {TOKEN} {EMAIL}")),
            });
        }
        Fixture {
            frames,
            ..Fixture::default()
        }
    }

    /// Runs the production session over the fixture: stdout, the error's text, and whether
    /// Quitting was sent. Nothing planted may appear in stdout or in the error, as `main` prints
    /// it or otherwise.
    fn run(fixture: &mut Fixture) -> (String, Option<String>, bool) {
        let mut out = Vec::new();
        let (result, quit_sent) = session(Ok(&mut *fixture), &mut out, drive);
        let out = String::from_utf8(out).expect("utf-8");
        let printed = result.as_ref().err().map(|error| format!("{error:?}"));
        let error = result.err().map(|error| format!("{error:#}"));
        for text in [Some(&out), printed.as_ref(), error.as_ref()]
            .into_iter()
            .flatten()
        {
            assert!(!text.contains(SENTINEL), "a planted value leaked");
        }
        (out, error, quit_sent)
    }

    fn ops(fixture: &Fixture) -> Vec<ControlToBrain> {
        fixture.sent.clone()
    }

    const SIGN_IN: ControlToBrain = ControlToBrain::AccountOp(AccountOp::SignIn);
    const CANCEL: ControlToBrain = ControlToBrain::AccountOp(AccountOp::CancelSignIn);
    const QUIT: ControlToBrain = ControlToBrain::Quitting;
    const WAITING: &str = "signing in: finish in the browser; waiting at most 330s\n";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum WaitFor {
        Initial,
        Answer,
        SignIn,
    }

    #[derive(Default)]
    struct ObservedFixture {
        frames: VecDeque<Result<Frame>>,
        sent: Vec<ControlToBrain>,
        signing_in: bool,
        sent_before_busy: Option<ControlToBrain>,
        waits: Vec<(WaitFor, Instant, Duration)>,
    }

    impl Wire for &mut ObservedFixture {
        fn send(&mut self, message: &ControlToBrain) -> Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn until<T>(
            &mut self,
            limit: Duration,
            mut pick: impl FnMut(&Frame) -> Option<T>,
        ) -> Result<T> {
            let phase = if self.signing_in {
                WaitFor::SignIn
            } else if self.sent.last() == Some(&SIGN_IN) {
                WaitFor::Answer
            } else {
                WaitFor::Initial
            };
            self.waits.push((phase, Instant::now(), limit));
            while let Some(frame) = self.frames.pop_front() {
                let frame = frame?;
                if let Some(picked) = pick(&frame) {
                    if self.signing_in {
                        thread::sleep(Duration::from_millis(25));
                    }
                    self.signing_in = matches!(
                        frame,
                        Frame::Control(ControlToShell::Account(AccountView::SigningIn))
                    );
                    if matches!(
                        frame,
                        Frame::Control(ControlToShell::Account(AccountView::Busy))
                    ) {
                        self.sent_before_busy = self.sent.last().cloned();
                    }
                    return Ok(picked);
                }
            }
            bail!("nothing expected arrived in {limit:?}; last: {TOKEN}")
        }
    }

    fn observed(steps: Vec<Step>) -> ObservedFixture {
        ObservedFixture {
            frames: fixture(steps).frames,
            ..ObservedFixture::default()
        }
    }

    fn run_observed(fixture: &mut ObservedFixture) -> (String, Option<String>, bool) {
        let mut out = Vec::new();
        let (result, quit_sent) = session(Ok(&mut *fixture), &mut out, drive);
        let out = String::from_utf8(out).expect("utf-8");
        let printed = result.as_ref().err().map(|error| format!("{error:?}"));
        let error = result.err().map(|error| format!("{error:#}"));
        for text in [Some(&out), printed.as_ref(), error.as_ref()]
            .into_iter()
            .flatten()
        {
            assert!(!text.contains(SENTINEL), "a planted value leaked");
        }
        (out, error, quit_sent)
    }

    #[test]
    fn a_sign_in_prints_only_states_and_keeps_one_deadline() {
        let declined = AccountView::SignedIn {
            email: None,
            plan: PlanState::Declined,
            recovery: None,
        };
        for (end, words) in [
            (signed_in(None), "plan enabled"),
            (declined, "plan declined"),
        ] {
            let mut brain = observed(vec![
                Step::View(signed_out()),
                Step::View(AccountView::SigningIn),
                Step::View(AccountView::SigningIn),
                Step::View(end),
            ]);
            let (out, error, quit_sent) = run_observed(&mut brain);
            assert_eq!(
                out,
                format!("account: signed out\n{WAITING}account: signed in, {words}\n")
            );
            assert_eq!(error, None);
            assert_eq!(brain.sent, [SIGN_IN, QUIT]);
            assert!(quit_sent);
            for (phase, seconds) in [
                (WaitFor::Initial, 10),
                (WaitFor::Answer, 10),
                (WaitFor::SignIn, 330),
            ] {
                assert_one_deadline(
                    brain
                        .waits
                        .iter()
                        .filter_map(|&(seen, at, limit)| (seen == phase).then_some((at, limit))),
                    Duration::from_secs(seconds),
                );
            }
        }
    }

    #[test]
    fn an_account_signed_in_is_left_alone() {
        let mut brain = fixture(vec![Step::View(signed_in(None))]);
        let (out, error, quit_sent) = run(&mut brain);
        assert_eq!(
            out,
            "account: signed in, plan enabled\nalready signed in; nothing to do\n"
        );
        assert_eq!(error, None);
        assert_eq!(ops(&brain), [QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn inspecting_the_account_never_starts_signin_or_prints_account_content() {
        for (view, expected) in [
            (signed_out(), "account: signed out\n"),
            (signed_in(None), "account: signed in, plan enabled\n"),
        ] {
            let mut brain = fixture(vec![Step::View(view)]);
            let mut out = Vec::new();
            let (result, quit_sent) = session(Ok(&mut brain), &mut out, inspect);
            assert_eq!(result.err().map(|error| format!("{error:#}")), None);
            assert_eq!(String::from_utf8(out).expect("UTF-8"), expected);
            assert_eq!(ops(&brain), [QUIT]);
            assert!(quit_sent);
        }
    }

    #[test]
    fn a_reauthentication_that_still_needs_sign_in_fails() {
        let again = || signed_in(Some(AccountRecovery::SignInAgain));
        let mut brain = fixture(vec![
            Step::View(again()),
            Step::View(AccountView::SigningIn),
            Step::View(again()),
        ]);
        let (out, error, quit_sent) = run(&mut brain);
        let words = "signed in, plan enabled, needs sign-in again";
        assert_eq!(
            out,
            format!("account: {words}\n{WAITING}account: {words}\n")
        );
        assert_eq!(
            error.as_deref(),
            Some(format!("sign-in ended with the account {words}").as_str())
        );
        assert_eq!(ops(&brain), [SIGN_IN, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn a_sign_in_that_settles_needing_retry_or_signed_out_fails() {
        for (end, words) in [
            (
                signed_in(Some(AccountRecovery::Retry)),
                "signed in, plan enabled, needs retry",
            ),
            (signed_out(), "signed out"),
        ] {
            let mut brain = fixture(vec![
                Step::View(signed_out()),
                Step::View(AccountView::SigningIn),
                Step::View(end),
            ]);
            let (out, error, quit_sent) = run(&mut brain);
            assert_eq!(
                out,
                format!("account: signed out\n{WAITING}account: {words}\n")
            );
            assert_eq!(
                error.as_deref(),
                Some(format!("sign-in ended with the account {words}").as_str())
            );
            assert_eq!(ops(&brain), [SIGN_IN, QUIT]);
            assert!(quit_sent);
        }
    }

    #[test]
    fn a_state_that_cant_sign_in_is_refused_without_an_op() {
        for (first, words) in [
            (
                AccountView::Busy,
                "busy: another Windows session holds the account",
            ),
            (
                AccountView::SigningOut {
                    cleanup: Cleanup::Refused,
                    revocation_unconfirmed: false,
                },
                "signing out, cleanup refused",
            ),
            (
                signed_in(Some(AccountRecovery::Retry)),
                "signed in, plan enabled, needs retry",
            ),
        ] {
            let mut brain = fixture(vec![Step::View(first)]);
            let (out, error, quit_sent) = run(&mut brain);
            assert_eq!(out, format!("account: {words}\n"));
            assert_eq!(
                error.as_deref(),
                Some(format!("not signing in while the account is {words}").as_str())
            );
            assert_eq!(ops(&brain), [QUIT]);
            assert!(quit_sent);
        }
    }

    #[test]
    fn a_refused_sign_in_reports_the_answer() {
        let mut brain = observed(vec![
            Step::View(signed_out()),
            Step::View(AccountView::Busy),
        ]);
        let (out, error, quit_sent) = run_observed(&mut brain);
        assert!(out.contains("signed out") && out.contains("busy"), "{out}");
        let error = error.expect("sign-in failed");
        assert!(
            error.contains("busy") && error.contains("another Windows session"),
            "{error}"
        );
        assert_eq!(brain.sent_before_busy, Some(SIGN_IN));
        assert_eq!(brain.sent, [SIGN_IN, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn the_deadline_cancels_the_sign_in() {
        let diagnostics = |detail: &str| {
            let mut brain = fixture(vec![
                Step::View(signed_out()),
                Step::View(AccountView::SigningIn),
                Step::Late,
                Step::View(signed_out()),
            ]);
            *brain
                .frames
                .iter_mut()
                .find(|frame| frame.is_err())
                .expect("deadline error") =
                Err(anyhow!("nothing expected arrived; last: {detail}"));
            brain.frames.push_front(Ok(Frame::Data(
                DataToShell::AssistantText {
                    run: RunId(3),
                    delta: detail.into(),
                },
                0,
            )));
            let mut out = Vec::new();
            let (result, quit_sent) = session(Ok(&mut brain), &mut out, drive);
            let error = result.expect_err("sign-in times out");
            assert_eq!(ops(&brain), [SIGN_IN, CANCEL, QUIT]);
            assert!(quit_sent);
            (out, format!("{error:#}"), format!("{error:?}"))
        };
        let original = format!("{TOKEN} {EMAIL}");
        let [shown, other] = [
            original.as_str(),
            "abcalternate-key-1290 abcother@fakemail.testsite",
        ]
        .map(diagnostics);
        assert!(
            shown == other,
            "account material affected sign-in diagnostics"
        );
        assert!(
            shown.0 == format!("account: signed out\n{WAITING}account: signed out\n").as_bytes()
                && shown.1 == "sign-in didn't settle in 330s; it was cancelled"
                && !shown.2.contains(SENTINEL),
            "sign-in deadline diagnostics are not sanitized"
        );
    }

    #[test]
    fn a_cancel_with_no_answer_says_so() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::View(AccountView::SigningIn),
            Step::Late,
            Step::Late,
        ]);
        let (out, error, _) = run(&mut brain);
        assert_eq!(out, format!("account: signed out\n{WAITING}"));
        assert_eq!(error.as_deref(), Some("no answer to CancelSignIn in 10s"));
    }

    #[test]
    fn a_closed_pipe_or_a_missing_view_fails_in_plain_words() {
        let mut closed = fixture(vec![
            Step::View(signed_out()),
            Step::View(AccountView::SigningIn),
            Step::Closed,
        ]);
        let (out, error, quit_sent) = run(&mut closed);
        assert_eq!(out, format!("account: signed out\n{WAITING}"));
        assert_eq!(
            error.as_deref(),
            Some("the brain closed the pipe while signing in")
        );
        assert!(quit_sent, "the fixture still takes Quitting");

        let mut early = fixture(vec![Step::Closed]);
        let (out, error, _) = run(&mut early);
        assert_eq!(out, "");
        assert_eq!(
            error.as_deref(),
            Some("the brain closed the pipe before the first account view")
        );

        let mut silent = fixture(vec![Step::Late]);
        let (out, error, _) = run(&mut silent);
        assert_eq!(out, "");
        assert_eq!(error.as_deref(), Some("no first account view in 10s"));
    }

    #[test]
    fn a_failed_send_or_connect_hides_the_pipe_error() {
        let mut brain = fixture(vec![Step::View(signed_out())]);
        brain.refuse = Some(SIGN_IN);
        let (out, error, quit_sent) = run(&mut brain);
        assert_eq!(out, "account: signed out\n");
        assert_eq!(error.as_deref(), Some("couldn't send SignIn to the brain"));
        assert!(quit_sent);

        let mut out = Vec::new();
        let (result, quit_sent) = session(
            Err::<&mut Fixture, _>(anyhow!("no capability: {TOKEN}")),
            &mut out,
            drive,
        );
        assert!(out.is_empty());
        assert!(!quit_sent);
        assert_eq!(
            format!("{:#}", result.expect_err("fails")),
            "the brain didn't accept the client"
        );
    }

    #[test]
    fn a_failed_quitting_after_a_sign_in_ends_the_brain() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::View(AccountView::SigningIn),
            Step::View(signed_in(None)),
        ]);
        brain.refuse = Some(QUIT);
        let (out, error, quit_sent) = run(&mut brain);
        assert_eq!(
            out,
            format!("account: signed out\n{WAITING}account: signed in, plan enabled\n")
        );
        assert_eq!(
            error.as_deref(),
            Some("couldn't send Quitting; the brain was ended")
        );
        assert!(!quit_sent, "the brain is stopped, not waited for");
    }

    #[test]
    fn a_failed_cancel_is_reported_and_the_brain_still_quits() {
        let mut brain = fixture(vec![
            Step::View(signed_out()),
            Step::View(AccountView::SigningIn),
            Step::Late,
        ]);
        brain.refuse = Some(CANCEL);
        let (out, error, quit_sent) = run(&mut brain);
        assert_eq!(out, format!("account: signed out\n{WAITING}"));
        assert_eq!(
            error.as_deref(),
            Some("couldn't send CancelSignIn to the brain")
        );
        assert_eq!(ops(&brain), [SIGN_IN, QUIT]);
        assert!(quit_sent);
    }

    #[test]
    fn every_view_renders_from_its_enums_only() {
        let cases = [
            (
                AccountView::SignedOut {
                    revocation_unconfirmed: true,
                },
                "signed out, revocation unconfirmed",
            ),
            (AccountView::SigningIn, "signing in"),
            (
                AccountView::SigningOut {
                    cleanup: Cleanup::Running,
                    revocation_unconfirmed: false,
                },
                "signing out, cleanup running",
            ),
            (
                AccountView::SigningOut {
                    cleanup: Cleanup::NotDurable,
                    revocation_unconfirmed: true,
                },
                "signing out, cleanup not durable, revocation unconfirmed",
            ),
            (
                AccountView::SignedIn {
                    email: Some(AccountEmail::new(EMAIL).expect("a valid email")),
                    plan: PlanState::Unavailable,
                    recovery: None,
                },
                "signed in, plan unavailable",
            ),
        ];
        for (view, words) in cases {
            assert_eq!(state(&view), words);
        }
    }
}
