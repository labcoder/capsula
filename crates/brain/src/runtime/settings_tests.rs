//! Settings over the pipes: the view's place in a connection, one answer per change in order,
//! and a held write that never holds up Stop.

use std::fs::File;
use std::os::windows::fs::OpenOptionsExt as _;

use tokio::time::Instant;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, ControlToShell, ErrorKey, Phase, RunId, SettingsOp,
    SettingsView, ShortcutChord, ShortcutView, ThreadId, UserName,
};
use kumochi_protocol::tap;
use kumochi_protocol::voice::VoicePart;
use tokio::io::AsyncRead;

use super::{Ended, Shell, duplex};
use crate::context::{Origin, RunSnapshot};
use crate::coordinator::{BuildContext, BuildKey};
use crate::ipc;
use crate::platform::set_dacl;
use crate::platform::sherpa::tests::{set_junction, stage_root};
use crate::runtime::CLOSE_WAIT;
use crate::runtime::Seen;
use crate::runtime::settings::Gate;
use crate::tap::Tap;
use crate::test_support::{self, Captured, SENTINEL, Scratch};

#[derive(PartialEq, Eq)]
enum Answer {
    Yes,
    No,
}

fn op(op: SettingsOp) -> ControlToBrain {
    ControlToBrain::Settings(op)
}

fn rename(name: &str) -> ControlToBrain {
    op(SettingsOp::SetAssistantName(
        AssistantName::new(name).expect("valid"),
    ))
}

impl<S: AsyncRead + tokio::io::AsyncWrite> Shell<S> {
    /// The next control frame, which must be a `Settings` that answers, or doesn't answer, a
    /// change.
    async fn settings(&mut self, answer: Answer) -> SettingsView {
        let next = self.unaccounted();
        let expected = answer == Answer::Yes;
        match tokio::time::timeout(super::PATIENCE, next).await {
            Ok(Ok(Some(ControlToShell::Settings { view, answer }))) if answer == expected => view,
            other => panic!("expected Settings answering {expected}, got {other:?}"),
        }
    }

    /// The answer to this connection's oldest unanswered change.
    async fn answer(&mut self) -> SettingsView {
        self.settings(Answer::Yes).await
    }

    /// The settings sent on their own: a connection's first frame, or a change another
    /// connection sent, once saved.
    async fn settled(&mut self) -> SettingsView {
        self.settings(Answer::No).await
    }

    /// The next control frame, which must be an `Error`.
    async fn error(&mut self) -> ErrorKey {
        let next = self.unaccounted();
        match tokio::time::timeout(super::PATIENCE, next).await {
            Ok(Ok(Some(ControlToShell::Error(error)))) => error.key(),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

/// The next settings, screen gate or scenes frame, as a label.
async fn gate_or_settings<S: AsyncRead + tokio::io::AsyncWrite>(shell: &mut Shell<S>) -> String {
    loop {
        let next = tokio::time::timeout(super::PATIENCE, shell.next_control());
        match next
            .await
            .expect("in time")
            .expect("readable")
            .expect("open")
        {
            ControlToShell::Settings { answer, .. } => return format!("Settings answer={answer}"),
            ControlToShell::ScreenHolds(state) => return format!("{state:?}"),
            ControlToShell::Scenes { .. } => return "Scenes".to_owned(),
            _ => {}
        }
    }
}

#[tokio::test]
async fn the_screen_gate_is_sent_at_connect_and_after_a_change_that_moves_it() {
    let scratch = Scratch::new("settings-screen-gate");
    let mut brain = test_support::brain(&scratch, "");
    let rig = crate::supervise::fixture::start::<crate::screen::Holds>();
    brain.attach_screen(std::sync::Arc::new(crate::screen::HoldClient::new(
        rig.handle.clone(),
    )));
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        let mut labels = Vec::new();
        for _ in 0..3 {
            labels.push(gate_or_settings(&mut shell).await);
        }
        assert_eq!(labels, ["Settings answer=false", "Off(NotReady)", "Scenes"]);
        // The gate speaks for the screen's picture, whatever the older window choice says.
        shell.send(&op(SettingsOp::SetShowScreen(true))).await;
        assert_eq!(gate_or_settings(&mut shell).await, "Settings answer=true");
        shell.send(&op(SettingsOp::SetScreenHolds(false))).await;
        assert_eq!(
            gate_or_settings(&mut shell).await,
            "Settings answer=true",
            "storing the choice it already read didn't resend the gate"
        );
        shell.send(&op(SettingsOp::SetShowScreen(false))).await;
        assert_eq!(
            gate_or_settings(&mut shell).await,
            "Off(TurnedOff)",
            "the gate didn't close as Off was taken, before its save"
        );
        assert_eq!(gate_or_settings(&mut shell).await, "Settings answer=true");
        shell.send(&op(SettingsOp::SetScreenHolds(true))).await;
        assert_eq!(gate_or_settings(&mut shell).await, "Settings answer=true");
        shell.send(&op(SettingsOp::SetShowScreen(true))).await;
        assert_eq!(
            gate_or_settings(&mut shell).await,
            "Settings answer=true",
            "the window choice coming back didn't move the gate"
        );
        assert_eq!(gate_or_settings(&mut shell).await, "Off(NotReady)");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    drop(rig);
}

#[tokio::test]
async fn the_view_comes_before_scenes_and_each_change_is_answered_in_order() {
    let scratch = Scratch::new("settings-order");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            shell.settled().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        assert!(shell.scenes().await.is_empty());
        shell.send(&rename("Nimbus")).await;
        shell
            .send(&op(SettingsOp::SetUserName(Some(
                UserName::new("Oscar").expect("valid"),
            ))))
            .await;
        shell.send(&op(SettingsOp::FinishOnboarding)).await;
        let names: Vec<_> = [
            shell.answer().await,
            shell.answer().await,
            shell.answer().await,
        ]
        .into_iter()
        .map(|view| {
            (
                view.assistant_name.as_str().to_owned(),
                view.user_name.map(|name| name.as_str().to_owned()),
                view.onboarded,
            )
        })
        .collect();
        assert_eq!(
            names,
            [
                ("Nimbus".to_owned(), None, false),
                ("Nimbus".to_owned(), Some("Oscar".to_owned()), false),
                ("Nimbus".to_owned(), Some("Oscar".to_owned()), true),
            ]
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let saved = scratch.config();
    assert_eq!(saved.view().assistant_name.as_str(), "Nimbus");
    assert!(saved.view().onboarded);
}

#[tokio::test]
async fn shortcut_save_and_reset_are_answered_in_order_after_persistence() {
    let scratch = Scratch::new("settings-shortcut");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let chord = ShortcutChord::new(1, Some(12)).expect("left Ctrl and F12");
    let client = async {
        assert_eq!(shell.settled().await.shortcut, ShortcutView::Default);
        assert!(shell.scenes().await.is_empty());
        shell.send(&op(SettingsOp::SetShortcut(Some(chord)))).await;
        assert_eq!(shell.answer().await.shortcut, ShortcutView::Custom(chord));
        assert_eq!(
            scratch.config().view().shortcut,
            ShortcutView::Custom(chord)
        );

        shell.send(&op(SettingsOp::SetShortcut(None))).await;
        assert_eq!(shell.answer().await.shortcut, ShortcutView::Default);
        assert_eq!(scratch.config().view().shortcut, ShortcutView::Default);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_held_write_never_holds_up_stop_and_the_changes_behind_it_wait() {
    let scratch = Scratch::new("settings-held");
    let mut brain = test_support::brain(&scratch, "0 text a\n5000 text never");
    let release = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        shell.send(&rename("Cirrus")).await;
        shell.submit("go").await;
        let (run, thinking) = shell.state().await;
        assert_eq!(thinking, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        shell.stop(run).await;
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        assert!(
            shell.views.is_empty(),
            "no change answered while the write is held"
        );
        release
            .send(Gate::Write)
            .expect("the write waits on the gate");
        assert_eq!(shell.answer().await.assistant_name.as_str(), "Nimbus");
        assert_eq!(shell.answer().await.assistant_name.as_str(), "Cirrus");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(scratch.config().view().assistant_name.as_str(), "Cirrus");
}

#[tokio::test]
async fn voice_installer_ops_are_refused_in_order_with_the_changes_around_them() {
    let scratch = Scratch::new("settings-voice-dormant");
    let mut brain = test_support::brain(&scratch, "0 text a\n5000 text never");
    let release = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        shell
            .send(&op(SettingsOp::InstallVoice(VoicePart::SpokenReplies)))
            .await;
        shell.send(&op(SettingsOp::SetTalkBack(true))).await;
        shell
            .send(&op(SettingsOp::RemoveVoice(VoicePart::SpeechInput)))
            .await;
        shell.send(&op(SettingsOp::CancelVoiceInstall)).await;
        // The run's answer means the brain has read every change above.
        shell.submit("go").await;
        let (run, thinking) = shell.state().await;
        assert_eq!(thinking, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        shell.stop(run).await;
        assert_eq!(shell.state().await, (run, Phase::Interrupted));
        assert!(
            shell.views.is_empty(),
            "nothing answered past the held write"
        );
        release
            .send(Gate::Write)
            .expect("the write waits on the gate");
        assert_eq!(shell.answer().await.assistant_name.as_str(), "Nimbus");
        assert_eq!(shell.error().await, ErrorKey::VoiceUnavailable);
        let refused = shell.answer().await;
        assert_eq!(refused.assistant_name.as_str(), "Nimbus");
        assert!(!refused.talk_back);
        assert!(shell.answer().await.talk_back);
        for _ in 0..2 {
            assert_eq!(shell.error().await, ErrorKey::VoiceUnavailable);
            assert!(shell.answer().await.talk_back);
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert!(scratch.config().view().talk_back);
    let text = std::fs::read_to_string(scratch.db().with_file_name(CONFIG_FILE)).expect("readable");
    let saved: toml::Table = text.parse().expect("parses");
    assert!(
        !saved["voice"]
            .as_table()
            .expect("a table")
            .contains_key("installed")
    );
}

#[tokio::test]
async fn a_refused_change_is_answered_with_its_error_then_the_unchanged_view() {
    let scratch = Scratch::new("settings-refused");
    let config = scratch.db().with_file_name(CONFIG_FILE);
    std::fs::create_dir_all(config.parent().expect("a folder")).expect("folder");
    std::fs::write(&config, "not = [toml").expect("broken");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        assert_eq!(shell.error().await, ErrorKey::ConfigUnreadable);
        assert_eq!(
            shell.answer().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn names_never_reach_the_tap_in_either_direction() {
    const NAME: &str = "Zqxsettingsname";
    let scratch = Scratch::new("settings-tap");
    let mut brain = test_support::brain(&scratch, "");
    let recording = scratch.db().with_file_name("tap.bin");
    let mut tap = Tap::to_file(File::create(&recording).expect("tap file"));
    let ((control, data), mut shell) = duplex();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename(NAME)).await;
        assert_eq!(shell.answer().await.assistant_name.as_str(), NAME);
        shell.send(&ControlToBrain::Pong).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    drop(tap);
    let recorded = std::fs::read(&recording).expect("tap read");
    assert!(
        !recorded.windows(NAME.len()).any(|w| w == NAME.as_bytes()),
        "the name was recorded"
    );
    let mut dumped = Vec::new();
    tap::dump(&mut recorded.as_slice(), &mut dumped).expect("dumps");
    let dumped = String::from_utf8(dumped).expect("utf-8");
    assert!(!dumped.contains(NAME), "{dumped}");
    let lines: Vec<_> = dumped.lines().collect();
    assert!(lines[0].contains("brain -> shell  Settings ("), "{dumped}");
    assert!(lines[1].contains("brain -> shell  Account ("), "{dumped}");
    assert!(lines[2].contains("brain -> shell  Connections(ConnectionsView { anthropic: Missing, anthropic_revision: None, claude_code: NotFound })"), "{dumped}");
    assert!(
        lines[3].contains("brain -> shell  ScreenHolds ("),
        "{dumped}"
    );
    assert!(lines[4].contains("brain -> shell  Scenes {"), "{dumped}");
    assert!(lines[5].contains("shell -> brain  Settings ("), "{dumped}");
    assert!(lines[6].contains("brain -> shell  Settings ("), "{dumped}");
    assert!(lines[7].contains("shell -> brain  Pong"), "{dumped}");
}

fn saved_name(scratch: &Scratch) -> String {
    scratch.config().view().assistant_name.as_str().to_owned()
}

#[tokio::test]
async fn a_change_drained_at_quit_is_written() {
    let scratch = Scratch::new("settings-quit-drained");
    let mut brain = test_support::brain(&scratch, "");
    let mut tap = Tap::from_env();
    brain.control_offline(rename("Nimbus"), 0, &mut tap);
    brain.quit(Instant::now() + super::PATIENCE).await;
    assert_eq!(saved_name(&scratch), "Nimbus");
}

#[tokio::test]
async fn quit_writes_a_held_change_and_the_one_queued_behind_it() {
    let scratch = Scratch::new("settings-quit-queued");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        shell.send(&rename("Cirrus")).await;
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    let Ended::Quit { deadline } = ended else {
        panic!("expected Quit, got {ended:?}");
    };
    release
        .send(Gate::Write)
        .expect("the write waits on the gate");
    brain.quit(deadline).await;
    assert_eq!(saved_name(&scratch), "Cirrus");
}

#[tokio::test]
async fn changes_from_a_gone_shell_are_written_while_none_is_connected() {
    let scratch = Scratch::new("settings-idle");
    let mut brain = test_support::brain(&scratch, "");
    let mut tap = Tap::from_env();
    let mut seen = brain.observe();
    brain.control_offline(rename("Nimbus"), 0, &mut tap);
    brain.control_offline(rename("Cirrus"), 0, &mut tap);
    let committed = async {
        let mut commits = 0;
        while commits < 2 {
            if seen.recv().await.expect("the brain reports") == Seen::SettingsCommitted {
                commits += 1;
            }
        }
    };
    tokio::select! {
        () = brain.idle() => unreachable!("idle never returns"),
        committed = tokio::time::timeout(super::PATIENCE, committed) => {
            committed.expect("both changes are written with no shell connected");
        }
    }
    assert_eq!(saved_name(&scratch), "Cirrus");
}

#[tokio::test]
async fn a_user_name_drained_after_a_disconnect_never_reaches_the_tap() {
    const USER: &str = "Zqxdraineduser";
    let scratch = Scratch::new("settings-tap-drained");
    let mut brain = test_support::brain(&scratch, "");
    let recording = scratch.db().with_file_name("tap.bin");
    let mut tap = Tap::to_file(File::create(&recording).expect("tap file"));
    let change = op(SettingsOp::SetUserName(Some(
        UserName::new(USER).expect("valid"),
    )));
    let len = frame::encode(&change, Pipe::Control)
        .expect("encodes")
        .len()
        - frame::HEADER_LEN;
    brain.control_offline(change, len, &mut tap);
    brain.quit(Instant::now() + super::PATIENCE).await;
    drop(tap);
    assert_eq!(
        scratch.config().view().user_name,
        Some(UserName::new(USER).expect("valid"))
    );
    let recorded = std::fs::read(&recording).expect("tap read");
    assert!(
        !recorded.windows(USER.len()).any(|w| w == USER.as_bytes()),
        "the name was recorded"
    );
    let mut dumped = Vec::new();
    tap::dump(&mut recorded.as_slice(), &mut dumped).expect("dumps");
    assert_eq!(
        String::from_utf8(dumped).expect("utf-8"),
        format!("    0 Control shell -> brain  Settings ({len} bytes)\n1 frames\n")
    );
}

#[tokio::test]
async fn a_later_connection_hears_the_settled_settings_and_never_an_old_answer() {
    let scratch = Scratch::new("settings-reconnect");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let first = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), first);
    assert_eq!(ended, Ended::Lost);
    let ((control, data), mut shell) = duplex();
    let second = async {
        assert_eq!(
            shell.settled().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        shell.scenes().await;
        shell.send(&rename("Cirrus")).await;
        release
            .send(Gate::Write)
            .expect("the write waits on the gate");
        // The first shell's change is saved and heard as settled settings, not as an answer.
        assert_eq!(shell.settled().await.assistant_name.as_str(), "Nimbus");
        assert_eq!(shell.answer().await.assistant_name.as_str(), "Cirrus");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), second);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(saved_name(&scratch), "Cirrus");
}

#[tokio::test]
async fn a_failed_write_is_answered_unchanged_and_the_next_change_still_saves() {
    let scratch = Scratch::new("settings-write-failed");
    let mut brain = test_support::brain(&scratch, "0 text a\n5000 text never");
    let release = brain.hold_config_write();
    // A folder where the temp file goes makes the write fail before the replace.
    let blocker = scratch.db().with_file_name(format!("{CONFIG_FILE}.tmp"));
    std::fs::create_dir_all(&blocker).expect("blocker");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        shell.submit("go").await;
        let (run, thinking) = shell.state().await;
        assert_eq!(thinking, Phase::Thinking);
        // The watchdog keeps pinging and hearing pongs while the write is held.
        let mut pings = 0;
        while pings < 2 {
            let next = ipc::read::<ControlToShell>(&mut shell.control_in, Pipe::Control);
            match tokio::time::timeout(super::PATIENCE, next).await {
                Ok(Ok(Some(ControlToShell::Ping))) => {
                    pings += 1;
                    shell.send(&ControlToBrain::Pong).await;
                }
                Ok(Ok(Some(ControlToShell::RunState {
                    phase: Phase::Answering,
                    ..
                }))) => {}
                other => panic!("expected pings, got {other:?}"),
            }
        }
        shell.stop(run).await;
        while shell.state().await != (run, Phase::Interrupted) {}
        release
            .send(Gate::Write)
            .expect("the write waits on the gate");
        assert_eq!(shell.error().await, ErrorKey::SettingsRefused);
        assert_eq!(
            shell.answer().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        std::fs::remove_dir(&blocker).expect("blocker removed");
        shell.send(&rename("Cirrus")).await;
        assert_eq!(shell.answer().await.assistant_name.as_str(), "Cirrus");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(saved_name(&scratch), "Cirrus");
}

#[tokio::test]
async fn a_panicked_write_logs_only_its_kind() {
    let logs = Captured::default();
    let _guard = logs.install();
    let scratch = Scratch::new("settings-write-panicked");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        release
            .send(Gate::Panic)
            .expect("the write waits on the gate");
        assert_eq!(shell.error().await, ErrorKey::SettingsRefused);
        assert_eq!(
            shell.answer().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let text = logs.text();
    assert!(!text.contains(SENTINEL), "{text}");
    assert!(text.contains("the settings write didn't finish"), "{text}");
    assert!(text.contains("panicked=true"), "{text}");
}

#[test]
fn a_held_write_never_holds_up_shutdown() {
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let scratch = Scratch::new("settings-shutdown");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let release = runtime.block_on(async {
            let mut brain = test_support::brain(&scratch, "");
            let release = brain.hold_config_write();
            let mut tap = Tap::from_env();
            brain.control_offline(rename("Nimbus"), 0, &mut tap);
            brain.quit(Instant::now() + CLOSE_WAIT).await;
            release
        });
        crate::shut_down(runtime);
        done_tx.send(()).expect("the test waits");
        // The write stays held until the test process exits.
        std::mem::forget(release);
    });
    done.recv_timeout(CLOSE_WAIT + super::PATIENCE)
        .expect("shutting down didn't wait for the held write");
}

#[tokio::test]
async fn an_earlier_connections_refused_change_sends_the_next_one_nothing() {
    let scratch = Scratch::new("settings-old-refusal");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    // A folder where the temp file goes makes the held write fail once released.
    let blocker = scratch.db().with_file_name(format!("{CONFIG_FILE}.tmp"));
    std::fs::create_dir_all(&blocker).expect("blocker");
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let first = async {
        shell.settled().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), first);
    assert_eq!(ended, Ended::Lost);
    let ((control, data), mut shell) = duplex();
    let second = async {
        shell.settled().await;
        shell.scenes().await;
        release
            .send(Gate::Write)
            .expect("the write waits on the gate");
        // This connection's change waits behind the earlier one and fails the same way, so its
        // Error is the next frame only if the earlier refusal sent nothing.
        shell.send(&rename("Cirrus")).await;
        assert_eq!(shell.error().await, ErrorKey::SettingsRefused);
        assert_eq!(
            shell.answer().await.assistant_name.as_str(),
            DEFAULT_ASSISTANT_NAME
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), second);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

/// Attempt `attempt` of run `run`'s first round in task 2, before or after its snapshot.
fn build(run: u64, attempt: u64, snapshot: Option<RunSnapshot>) -> BuildContext {
    BuildContext {
        control: crate::recall::control::Control::new(
            std::time::Instant::now() + crate::coordinator::WALL_CLOCK,
        ),
        key: BuildKey {
            run: RunId(run),
            round: 1,
            attempt,
        },
        thread: ThreadId(2),
        origin: Origin::Task(ThreadId(2)),
        first: snapshot.is_none(),
        arrived: crate::coordinator::Arrived::default(),
        snapshot,
        drawings: crate::drawing::Digest::default(),
        picture_models: Vec::new(),
        key_results: Vec::new(),
    }
}

#[tokio::test]
async fn a_rename_reaches_the_next_run_and_never_a_running_ones_rebuild() {
    let scratch = Scratch::new("rename-snapshot");
    let mut brain = test_support::brain(&scratch, "");
    let mut first = build(1, 1, None);
    brain.seed(&mut first);
    let started = first.snapshot.expect("the first build is seeded");
    assert_eq!(started.assistant.as_str(), DEFAULT_ASSISTANT_NAME);
    let renamed = brain
        .config
        .stage(SettingsOp::SetAssistantName(
            AssistantName::new("Cirrus").expect("valid"),
        ))
        .expect("staged");
    brain.config.commit(renamed);
    let mut rebuild = build(1, 2, Some(started.clone()));
    brain.seed(&mut rebuild);
    assert_eq!(
        rebuild.snapshot,
        Some(started),
        "the running run keeps its names"
    );
    let mut next = build(2, 3, None);
    brain.seed(&mut next);
    assert_eq!(
        next.snapshot.expect("seeded").assistant.as_str(),
        "Cirrus",
        "the next run takes the rename"
    );
}

#[tokio::test]
async fn a_junction_on_the_way_to_a_part_deletes_nothing_outside_it() {
    let scratch = Scratch::new("voice-removal-junction");
    let root = stage_root("voice-removal-junction");
    let path = seeded(&scratch, "[\"speech-input\"]");
    let outside = root.join("outside");
    let sentinel = outside.join(VoicePart::SpeechInput.dir()).join("keep.txt");
    std::fs::create_dir_all(sentinel.parent().expect("a parent")).expect("the outside folder");
    std::fs::write(&sentinel, [1]).expect("the sentinel");
    let models = root.join("models");
    std::fs::create_dir(&models).expect("models folder");
    set_junction(&models, &outside).expect("models redirected outside");
    let mut brain = test_support::brain(&scratch, "");
    brain.finish_removals(&models);
    brain.drain_settings().await;
    assert!(sentinel.is_file(), "nothing behind the junction is deleted");
    assert_eq!(
        removing_on_disk(&path),
        toml::Value::Array(vec!["speech-input".into()])
    );
    assert_eq!(brain.config.removing(), [VoicePart::SpeechInput]);
}

#[tokio::test]
async fn a_models_folder_others_can_change_deletes_nothing() {
    let scratch = Scratch::new("voice-removal-unsafe");
    let root = stage_root("voice-removal-unsafe");
    let path = seeded(&scratch, "[\"speech-input\"]");
    let models = root.join("models");
    let model = models.join(VoicePart::SpeechInput.dir()).join("model.onnx");
    std::fs::create_dir_all(model.parent().expect("a parent")).expect("the part's folder");
    std::fs::write(&model, [1]).expect("a model file");
    set_dacl(&models, |user| {
        format!("D:P(A;OICI;FA;;;{user})(A;;FA;;;WD)")
    });
    let mut brain = test_support::brain(&scratch, "");
    brain.finish_removals(&models);
    brain.drain_settings().await;
    assert!(
        model.is_file(),
        "a folder everyone may change isn't trusted"
    );
    assert_eq!(
        removing_on_disk(&path),
        toml::Value::Array(vec!["speech-input".into()])
    );
    assert_eq!(brain.config.removing(), [VoicePart::SpeechInput]);
}

#[tokio::test]
async fn a_missing_models_folder_counts_as_deleted() {
    let scratch = Scratch::new("voice-removal-no-models");
    let root = stage_root("voice-removal-no-models");
    let path = seeded(&scratch, "[\"speech-input\"]");
    let mut brain = test_support::brain(&scratch, "");
    brain.finish_removals(&root.join("models"));
    brain.drain_settings().await;
    assert_eq!(removing_on_disk(&path), toml::Value::Array(Vec::new()));
    assert_eq!(brain.config.removing(), []);
}

/// Start-up recovery's config writes, read back from the file.
fn removing_on_disk(path: &std::path::Path) -> toml::Value {
    let saved: toml::Table = std::fs::read_to_string(path)
        .expect("readable")
        .parse()
        .expect("parses");
    saved["voice"]["removing"].clone()
}

fn seeded(scratch: &Scratch, removing: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let path = scratch.dir().join(CONFIG_FILE);
    std::fs::write(
        &path,
        format!("[voice]\ninstalled = [\"speech-input\"]\nremoving = {removing}\n"),
    )
    .expect("seeded");
    path
}

#[tokio::test]
async fn start_up_deletes_each_removed_part_then_finishes_its_removal() {
    let scratch = Scratch::new("voice-removals");
    let root = stage_root("voice-removals");
    let path = seeded(
        &scratch,
        "[\"speech-input\", \"later-part\", \"spoken-replies\"]",
    );
    let models = root.join("models");
    let spoken = models.join(VoicePart::SpokenReplies.dir());
    std::fs::create_dir_all(spoken.join("voices")).expect("a part left mid-delete");
    std::fs::write(spoken.join("voices").join("juniper.bin"), [1, 2, 3]).expect("a model file");
    let mut brain = test_support::brain(&scratch, "");
    brain.finish_removals(&models);
    brain.drain_settings().await;
    assert!(!spoken.exists(), "the folder is deleted");
    assert_eq!(
        removing_on_disk(&path),
        toml::Value::Array(vec!["later-part".into()]),
        "a folder already gone counts as deleted, and an unknown name stays"
    );
    assert_eq!(brain.config.removing(), []);
    assert!(models.exists(), "only the parts' folders go");
}

#[tokio::test]
async fn a_part_that_cant_be_deleted_stays_listed_for_the_next_start() {
    let scratch = Scratch::new("voice-removal-stuck");
    let root = stage_root("voice-removal-stuck");
    let path = seeded(&scratch, "[\"speech-input\"]");
    let models = root.join("models");
    let part = models.join(VoicePart::SpeechInput.dir());
    std::fs::create_dir_all(&part).expect("the part's folder");
    let stuck = part.join("model.onnx");
    std::fs::write(&stuck, [1]).expect("a model file");
    // Opened without sharing, so nothing else can open it to delete it.
    let _open = std::fs::File::options()
        .read(true)
        .share_mode(0)
        .open(&stuck)
        .expect("held open");
    let mut brain = test_support::brain(&scratch, "");
    brain.finish_removals(&models);
    brain.drain_settings().await;
    assert!(stuck.is_file());
    assert_eq!(
        removing_on_disk(&path),
        toml::Value::Array(vec!["speech-input".into()])
    );
    assert_eq!(brain.config.removing(), [VoicePart::SpeechInput]);
}
