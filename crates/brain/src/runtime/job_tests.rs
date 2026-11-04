//! The title job over the pipes: a task's first `Done` run sends its first ask through the
//! runtime's engine under the job's own lease, the model's title reaches the store, and the shell
//! is nudged. Neither the title nor the ask reaches the logs.

use kumochi_protocol::message::{ControlToShell, Phase};
use tokio::sync::mpsc;

use super::*;
use crate::coordinator::SendKey;
use crate::engines::{Answer, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::test_support::{Captured, SENTINEL};

/// Answers a run's round with "answer" and a job's with [`SENTINEL`], each under a grant.
#[derive(Debug)]
struct Titles;

impl Engine for Titles {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let text = match request.key {
            SendKey::Build(_) => "answer",
            SendKey::Job(_) => SENTINEL,
        };
        Box::pin(async move {
            let Answer::Grant(_lease) = gate.ask().await else {
                return;
            };
            if events
                .send(TurnEvent::TextDelta(text.to_owned()))
                .await
                .is_ok()
            {
                events
                    .send(TurnEvent::Finished(Finish::Completed))
                    .await
                    .ok();
            }
        })
    }
}

#[tokio::test]
async fn a_tasks_first_done_run_takes_the_models_title_and_nudges_the_shell() {
    let scratch = Scratch::new("title-job");
    let captured = Captured::default();
    let _logs = captured.install();
    let mut brain = patient_brain_with(&scratch, Box::new(Titles));
    brain.offline(Input::SignedIn { generation: 0 });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("zqxfirstask plan a trip").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        shell.read_to_end(run).await;
        assert_eq!(shell.control().await, Some(ControlToShell::ThreadsChanged));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen");
    let titles: Vec<(String, String)> = store
        .conn_for_tests()
        .prepare("SELECT title, title_source FROM threads WHERE kind = 'thread'")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(titles, [(SENTINEL.to_owned(), "model".to_owned())]);
    let logs = captured.text();
    assert!(logs.contains("a title job ended"), "{logs}");
    assert!(!logs.contains(SENTINEL), "{logs}");
    assert!(!logs.contains("zqxfirstask"), "{logs}");
}
