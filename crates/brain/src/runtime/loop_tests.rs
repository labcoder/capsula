//! The tool loop across the brain: a round's call is answered and stored, and the next round is
//! built from the store, with the call, its result and nothing else of round 1 in its input.

use kumochi_protocol::message::{DataToShell, Phase};
use tokio::sync::mpsc;

use super::{duplex, patient_brain_with};
use crate::context::InputItem;
use crate::coordinator::{SendKey, UNKNOWN_TOOL};
use crate::engines::{
    CallId, Engine, Finish, Opaque, Round, RunRequest, SendGate, ToolName, TurnEvent,
};
use crate::runtime::Ended;
use crate::tap::Tap;
use crate::test_support::Scratch;

const CALL: &str = r#"{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"}"#;
const ANSWER: &str = r#"{"type":"message","role":"assistant","content":"done"}"#;

/// Round 1 calls a tool Kumochi doesn't have; round 2 answers. Each start reports its input.
#[derive(Debug)]
struct Rounds {
    seen: mpsc::UnboundedSender<Vec<InputItem>>,
}

fn item(json: &str) -> Opaque {
    Opaque::new(json.as_bytes().to_vec()).expect("small")
}

impl Engine for Rounds {
    fn start(
        &self,
        request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        let SendKey::Build(key) = request.key else {
            panic!("only runs start here");
        };
        self.seen.send(request.request.input).ok();
        let call = CallId::new("c1").expect("short");
        let script = if key.round == 1 {
            vec![
                TurnEvent::ToolCallStarted {
                    call: call.clone(),
                    name: ToolName::Unknown,
                },
                TurnEvent::ToolCallReady {
                    call,
                    args: "{}".to_owned(),
                },
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(CALL),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        } else {
            vec![
                TurnEvent::TextDelta("done".to_owned()),
                TurnEvent::OutputItem {
                    index: 0,
                    item: item(ANSWER),
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        };
        Box::pin(async move {
            for event in script {
                if events.send(event).await.is_err() {
                    return;
                }
            }
        })
    }
}

#[tokio::test]
async fn a_called_round_is_stored_and_the_next_round_replays_it() {
    let scratch = Scratch::new("loop");
    let (seen, mut inputs) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(&scratch, Box::new(Rounds { seen }));
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        assert_eq!(
            shell.data().await,
            DataToShell::AssistantText {
                run,
                delta: "done".into()
            }
        );
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;

    let first = inputs.recv().await.expect("round 1");
    let second = inputs.recv().await.expect("round 2");
    assert_eq!(first.len(), 1, "round 1 sends only the turn");
    assert_eq!(second[..1], first[..], "the turn leads round 2 too");
    let [InputItem::Native(native), result] = &second[1..] else {
        panic!("round 2 replays the call and its result, not {second:?}");
    };
    let json = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).expect("json");
    assert_eq!(
        json(native.bytes()),
        json(CALL.as_bytes()),
        "the same JSON value"
    );
    assert_eq!(
        *result,
        InputItem::FunctionOutput {
            call: CallId::new("c1").expect("short"),
            output: UNKNOWN_TOOL.to_owned(),
        }
    );
    assert_eq!(inputs.recv().await, None, "no third round");
    let store = crate::store::Store::open(&scratch.db()).expect("reopen");
    let kinds: Vec<String> = store
        .conn_for_tests()
        .prepare("SELECT kind FROM events WHERE run IS NOT NULL ORDER BY id")
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("read");
    assert_eq!(
        kinds,
        [
            "user",
            "output_item",
            "tool_result",
            "assistant_text",
            "output_item"
        ]
    );
}
