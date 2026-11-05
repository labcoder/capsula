//! The title job over the pipes: a task's first `Done` run sends its first ask through the
//! runtime's engine under the job's own lease, the model's title reaches the store, and the shell
//! is nudged. Neither the title nor the ask reaches the logs.

use kumochi_protocol::message::{ControlToShell, Phase, RunId};
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
    assert!(logs.contains("a job ended"), "{logs}");
    assert!(!logs.contains(SENTINEL), "{logs}");
    assert!(!logs.contains("zqxfirstask"), "{logs}");
}

#[tokio::test]
async fn a_long_tasks_done_run_titles_it_then_stores_a_summary_of_its_oldest_runs() {
    let scratch = Scratch::new("compact-job");
    let task = {
        let mut store = Store::open(&scratch.db()).expect("store");
        store.recover().expect("recover");
        let (high_thread, high_run) = store.high_water().expect("ids");
        let task = kumochi_protocol::message::ThreadId(high_thread.0 + 1);
        for n in 1..=9 {
            let run = RunId(high_run.0 + n);
            let title = (n == 1).then_some("zqxoldask");
            store
                .begin_turn(task, title, run, &format!("zqxoldask {n}"))
                .expect("turn");
            store
                .end_run(task, run, &[], crate::store::RunEnd::Done)
                .expect("ended");
        }
        task
    };
    let captured = Captured::default();
    let _logs = captured.install();
    let mut brain = patient_brain_with(&scratch, Box::new(Titles));
    brain.compact_at = 1;
    brain.offline(Input::SignedIn { generation: 0 });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let db = scratch.db();
    let client = async {
        let turn = kumochi_protocol::message::DataToBrain::TextSubmit {
            turn: 1,
            screen: kumochi_protocol::message::ScreenChoice::None,
            thread: Some(task),
            text: "one more".into(),
        };
        shell
            .data_out
            .write_all(&frame::encode(&turn, Pipe::Data).expect("encodes"))
            .await
            .expect("sent");
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        shell.read_to_end(run).await;
        assert_eq!(shell.control().await, Some(ControlToShell::ThreadsChanged));
        let reader = rusqlite::Connection::open(&db).expect("reader");
        let found = timeout(PATIENCE, async {
            loop {
                let found: Option<(i64, i64, String)> =
                    rusqlite::OptionalExtension::optional(reader.query_row(
                        "SELECT first_event, last_event, text FROM summaries",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    ))
                    .expect("read");
                if let Some(found) = found {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a summary is stored in time");
        drop(shell);
        found
    };
    let (ended, found) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let store = Store::open(&scratch.db()).expect("reopen");
    let oldest: (i64, i64) = store
        .conn_for_tests()
        .query_row(
            "SELECT min(id), max(id) FROM events WHERE run IN
             (SELECT run FROM events WHERE thread = ?1 ORDER BY id LIMIT 1)",
            [i64::try_from(task.0).expect("small")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("oldest run");
    assert_eq!(found.2, SENTINEL);
    let second_end: i64 = store
        .conn_for_tests()
        .query_row(
            "SELECT max(id) FROM events WHERE run =
             (SELECT run FROM events WHERE thread = ?1 GROUP BY run ORDER BY min(id) LIMIT 1 OFFSET 1)",
            [i64::try_from(task.0).expect("small")],
            |row| row.get(0),
        )
        .expect("second run");
    assert_eq!(found.0, oldest.0, "it covers from the oldest event");
    assert!(second_end > oldest.1);
    assert_eq!(
        found.1, second_end,
        "through the second run, the last one outside the window"
    );
    let logs = captured.text();
    assert!(!logs.contains(SENTINEL), "{logs}");
    assert!(!logs.contains("zqxoldask"), "{logs}");
}
