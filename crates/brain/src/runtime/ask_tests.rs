//! An ask about a clicked part through the real request path: the shell's `AskAbout` reaches the
//! engine as a turn in the scene's own thread, on that thread's model, with its history.

use std::sync::Arc;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ConnectionKind, ControlToShell, DataToBrain, Effort, ModelChoice, ModelSlug, Phase, RunId,
    SceneId, ScreenChoice, ThreadId,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::{duplex, patient_brain_with};
use crate::context::{InputItem, Role};
use crate::drawing::{Reply, Target, Write};
use crate::engines::{Engine, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::runtime::Ended;
use crate::store::{RunEnd, Store};
use crate::tap::Tap;
use crate::test_support::{Captured, Scratch};

/// Each start reports its thread, model and input, then answers at once.
#[derive(Debug)]
struct Asked {
    seen: mpsc::UnboundedSender<(ThreadId, Option<ModelChoice>, Vec<InputItem>)>,
}

impl Engine for Asked {
    fn start(
        &self,
        request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        self.seen
            .send((
                request.thread,
                request.request.choice,
                request.request.input,
            ))
            .ok();
        Box::pin(async move {
            for event in [
                TurnEvent::TextDelta("ok".to_owned()),
                TurnEvent::Finished(Finish::Completed),
            ] {
                if events.send(event).await.is_err() {
                    return;
                }
            }
        })
    }
}

const T: ThreadId = ThreadId(2);
const U: ThreadId = ThreadId(3);

/// 400 scalars, most of them four UTF-8 bytes, led by an ASCII marker.
fn capped(marker: &str) -> String {
    let mut text = marker.to_owned();
    text.extend(std::iter::repeat_n('\u{1d538}', 400 - marker.len()));
    text
}

/// Thread 2 drew a car whose solid "a" has `label` and `info` and whose scene has `alt`, after a
/// turn of its own, then took `choice`; thread 3 ran a turn of its own after it.
fn seed(scratch: &Scratch, [label, info, alt]: [&str; 3], choice: &ModelChoice) -> SceneId {
    let mut store = Store::open(&scratch.db()).expect("store");
    store.recover().expect("recover");
    store.main_thread().expect("main");
    store
        .begin_turn(T, Some("task"), RunId(1), "T-HISTORY earlier")
        .expect("turn");
    let header = serde_json::from_value(serde_json::json!({"begin":{"v":1,"scene":"car","surface":"overlay","alt":alt},"layout_version":2})).expect("header");
    let step = serde_json::from_value(serde_json::json!({"id":"s1","put":[{"view3d":{"id":"v"}},
        {"solid":{"in":"v","id":"a","shape":"box","size":[2,2,2],"label":label,"info":info}}]}))
    .expect("typed step");
    let Reply::Step(scene) = store
        .drawing_write(&Write::Step {
            thread: T,
            run: RunId(1),
            clear: Vec::new(),
            target: Target::New(header),
            revision: 1,
            layout_version: 2,
            step: Arc::new(step),
            protected: false,
        })
        .expect("step")
    else {
        panic!("step reply")
    };
    store
        .end_run(T, RunId(1), &[], RunEnd::Done)
        .expect("outcome");
    assert!(store.set_model(T, choice).expect("model"));
    store
        .begin_turn(U, Some("other"), RunId(2), "U-SENTINEL")
        .expect("other turn");
    store
        .end_run(U, RunId(2), &[], RunEnd::Done)
        .expect("other outcome");
    scene
}

/// `stamp` reads as `YYYY-MM-DD HH:MM`.
pub(super) fn assert_minute(stamp: &str) {
    assert_eq!(stamp.len(), 16, "YYYY-MM-DD HH:MM");
    assert!(
        stamp.char_indices().all(|(at, c)| match at {
            4 | 7 => c == '-',
            10 => c == ' ',
            13 => c == ':',
            _ => c.is_ascii_digit(),
        }),
        "YYYY-MM-DD HH:MM"
    );
}

/// The current turn carries its minute and the complete drawing header.
fn assert_turn_context(context: &str, alt: &str) {
    let (stamp, drawings) = context
        .split_once("\nDrawings on screen: ")
        .expect("drawing context follows time");
    assert_minute(stamp);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(drawings).expect("drawing headers"),
        serde_json::json!([{"begin":{"v":1,"scene":"car","surface":"overlay","size":null,"title":null,"alt":alt,"others":"clear"},"objects":[["v","view3d"],["a","solid"]]}])
    );
}

#[tokio::test]
async fn an_ask_reaches_the_engine_in_the_scenes_thread_on_its_model_with_its_history() {
    let (label, info, alt) = (capped("LABELSENT"), capped("INFOSENT"), capped("ALTSENT"));
    let scratch = Scratch::new("ask-request");
    let choice = ModelChoice {
        model: ModelSlug::new("thread-model").expect("valid"),
        effort: Effort::High,
        connection: ConnectionKind::ChatGpt,
    };
    let scene = seed(&scratch, [&label, &info, &alt], &choice);
    let logs = Captured::default();
    let _guard = logs.install();
    let (seen, mut requests) = mpsc::unbounded_channel();
    let mut brain = patient_brain_with(&scratch, Box::new(Asked { seen }));
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let selected = DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: Some(U),
            text: "selected".into(),
        };
        let asked = DataToBrain::AskAbout {
            turn: 2,
            scene,
            element: kumochi_scene::Handle::new(1, 1).expect("in range"),
        };
        let mut focus = Vec::new();
        for message in [selected, asked] {
            let frame = frame::encode(&message, Pipe::Data).expect("encodes");
            shell.data_out.write_all(&frame).await.expect("sent");
            loop {
                match shell.control().await.expect("open") {
                    ControlToShell::Focus { run, thread } => focus.push((run, thread)),
                    ControlToShell::RunState {
                        phase: Phase::Done, ..
                    } => break,
                    _ => {}
                }
            }
        }
        drop(shell);
        focus
    };
    let (ended, focus) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;

    let [(run, thread)] = focus[..] else {
        panic!("one focus, for the ask: {focus:?}");
    };
    assert_eq!(thread, T);
    let (first, _, _) = requests.recv().await.expect("the selected thread's run");
    assert_eq!(first, U);
    let (thread, model, input) = requests.recv().await.expect("the ask's run");
    assert_eq!((thread, model), (T, Some(choice)));
    let literal = format!(
        "Tell me about this part of the drawing: \u{201c}{label}\u{201d}.\n\
         Its description: \u{201c}{info}\u{201d}.\n\
         The drawing shows: \u{201c}{alt}\u{201d}."
    );
    let store = Store::open(&scratch.db()).expect("reopen");
    let row: String = store
        .conn_for_tests()
        .query_row(
            "SELECT text FROM events WHERE run = ?1 AND kind = 'user'",
            [i64::try_from(run.0).expect("small")],
            |row| row.get(0),
        )
        .expect("the ask's row");
    assert_eq!(row, literal);
    let users: Vec<&str> = input
        .iter()
        .filter_map(|item| match item {
            InputItem::Message {
                role: Role::User,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        users[0].starts_with("T-HISTORY earlier"),
        "T's own history leads"
    );
    let stamp = users
        .last()
        .and_then(|last| last.strip_prefix(&format!("{literal}\n\nLocal time: ")))
        .expect("the ask is the last user message");
    assert_turn_context(stamp, &alt);
    assert!(!format!("{input:?}").contains("U-SENTINEL"));
    assert!(input.iter().all(|item| match item {
        InputItem::Message { text, .. } => !text.contains("U-SENTINEL"),
        _ => true,
    }));
    let text = logs.text();
    for marker in ["LABELSENT", "INFOSENT", "ALTSENT"] {
        assert!(!text.contains(marker), "{marker}");
    }
}
