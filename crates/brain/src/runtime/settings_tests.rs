//! Settings over the pipes: the view's place in a connection, one answer per change in order,
//! and a held write that never holds up Stop.

use std::fs::File;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::frame::Pipe;
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, ControlToShell, Phase, SettingsOp, SettingsView, UserName,
};
use kumochi_protocol::tap;
use tokio::io::AsyncRead;

use super::{Ended, Shell, duplex};
use crate::ipc;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

fn op(op: SettingsOp) -> ControlToBrain {
    ControlToBrain::Settings(op)
}

fn rename(name: &str) -> ControlToBrain {
    op(SettingsOp::SetAssistantName(
        AssistantName::new(name).expect("valid"),
    ))
}

impl<S: AsyncRead + tokio::io::AsyncWrite> Shell<S> {
    /// The next control frame, which must be a `Settings`.
    async fn view(&mut self) -> SettingsView {
        let next = ipc::read::<ControlToShell>(&mut self.control_in, Pipe::Control);
        match tokio::time::timeout(super::PATIENCE, next).await {
            Ok(Ok(Some(ControlToShell::Settings(view)))) => view,
            other => panic!("expected Settings, got {other:?}"),
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
            shell.view().await.assistant_name.as_str(),
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
        let names: Vec<_> = [shell.view().await, shell.view().await, shell.view().await]
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
        shell.view().await;
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
        release.send(()).expect("the write waits on the gate");
        assert_eq!(shell.view().await.assistant_name.as_str(), "Nimbus");
        assert_eq!(shell.view().await.assistant_name.as_str(), "Cirrus");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(scratch.config().view().assistant_name.as_str(), "Cirrus");
}

#[tokio::test]
async fn a_refused_change_is_answered_with_the_unchanged_view() {
    let scratch = Scratch::new("settings-refused");
    let config = scratch.db().with_file_name(CONFIG_FILE);
    std::fs::create_dir_all(config.parent().expect("a folder")).expect("folder");
    std::fs::write(&config, "not = [toml").expect("broken");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.view().await;
        shell.scenes().await;
        shell.send(&rename("Nimbus")).await;
        assert_eq!(
            shell.view().await.assistant_name.as_str(),
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
        shell.view().await;
        shell.scenes().await;
        shell.send(&rename(NAME)).await;
        assert_eq!(shell.view().await.assistant_name.as_str(), NAME);
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
