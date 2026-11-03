//! Settings over the pipes: the view's place in a connection, one answer per change in order,
//! and a held write that never holds up Stop.

use std::fs::File;
use std::time::Duration;

use tokio::time::Instant;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, ControlToShell, ErrorKey, Phase, SettingsOp, SettingsView,
    UserName,
};
use kumochi_protocol::tap;
use tokio::io::AsyncRead;

use super::{Ended, Shell, duplex};
use crate::ipc;
use crate::runtime::CLOSE_WAIT;
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
        let next = ipc::read::<ControlToShell>(&mut self.control_in, Pipe::Control);
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
        let next = ipc::read::<ControlToShell>(&mut self.control_in, Pipe::Control);
        match tokio::time::timeout(super::PATIENCE, next).await {
            Ok(Ok(Some(ControlToShell::Error(error)))) => error.key(),
            other => panic!("expected Error, got {other:?}"),
        }
    }
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
    assert!(lines[1].contains("brain -> shell  Scenes {"), "{dumped}");
    assert!(lines[2].contains("shell -> brain  Settings ("), "{dumped}");
    assert!(lines[3].contains("brain -> shell  Settings ("), "{dumped}");
    assert!(lines[4].contains("shell -> brain  Pong"), "{dumped}");
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
    brain.quit(Instant::now() + CLOSE_WAIT).await;
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
    brain.control_offline(rename("Nimbus"), 0, &mut tap);
    brain.control_offline(rename("Cirrus"), 0, &mut tap);
    let saved = async {
        while saved_name(&scratch) != "Cirrus" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        () = brain.idle() => unreachable!("idle never returns"),
        saved = tokio::time::timeout(super::PATIENCE, saved) => {
            saved.expect("both changes are written with no shell connected");
        }
    }
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
    done.recv_timeout(CLOSE_WAIT + Duration::from_secs(5))
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
