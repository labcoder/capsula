use super::{DUPLEX_BUFFER, Ended, PATIENCE, Phase, RunId, Scratch, duplex, outcome, outcomes};
use crate::coordinator::StoreOp;
use crate::drawing::Write;
use crate::engines::{CallId, Engine, Round, RunRequest, SendGate, ToolName, TurnEvent};
use crate::runtime::Seen;
use crate::store::Store;
use crate::tap::Tap;
use kumochi_scene::SceneState;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Debug)]
struct SceneLoad;

impl Engine for SceneLoad {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, _: SendGate) -> Round {
        assert!(request.request.tools.iter().any(|tool| tool.name == "draw"));
        Box::pin(async move {
            events
                .send(TurnEvent::TextDelta("x".repeat(240_000)))
                .await
                .expect("text");
            let curve = "M0 0 C0 20000 20000 20000 20000 0 ";
            let long = curve.repeat(8);
            let short = format!("{curve}{}", "L0 0 ".repeat(392));
            for name in ["one", "two", "three", "four"] {
                let call = CallId::new(name).expect("call id");
                let args = json!({"records":[
                    {"begin":{"v":1,"scene":name,"surface":"overlay","alt":"","others":"keep"}},
                    {"step":{"id":name,"put":[
                        {"path":{"id":"long","d":long}},
                        {"path":{"id":"short","d":short}}
                    ]}},
                    {"end":{}}
                ]})
                .to_string();
                for event in [
                    TurnEvent::ToolCallStarted {
                        call: call.clone(),
                        name: ToolName::Draw,
                    },
                    TurnEvent::ToolArgsDelta {
                        call: call.clone(),
                        delta: args.clone(),
                    },
                    TurnEvent::ToolCallReady { call, args },
                ] {
                    events.send(event).await.expect("draw input");
                }
            }
            std::future::pending::<()>().await;
        })
    }
}

#[tokio::test]
async fn stop_is_answered_with_a_blocked_writer_and_four_scenes_at_the_path_cap() {
    let scratch = Scratch::new("saturated-scene-admission");
    let mut brain = super::super::draw_stream_tests::brain(&scratch);
    brain.engine = Box::new(SceneLoad);
    brain.coordinator.defer_drawing_preparation();
    let inputs = brain.inputs_tx.clone();
    let mut observed = brain.observe();
    let release = brain.hold_store(|op| {
        matches!(op, StoreOp::Drawing { write: Write::Step { step, .. }, .. }
            if step.id.as_str() == "four")
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        inputs
            .send(super::super::draw_stream_tests::provider_ready())
            .expect("select the provider draw route after the shell opens");
        shell.submit("draw").await;
        let (run, phase) = shell.state().await;
        assert_eq!(phase, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        let len = shell.start_frame().await;
        assert!(
            len > 4 * DUPLEX_BUFFER,
            "the unread frame must block: {len} bytes"
        );
        for expected in ["one", "two", "three", "four"] {
            timeout(PATIENCE, async {
                loop {
                    if let Seen::StoreOpStarted(StoreOp::Drawing {
                        write:
                            Write::Step {
                                run: owner, step, ..
                            },
                        ..
                    }) = observed.recv().await.expect("store observation")
                    {
                        assert_eq!((owner, step.id.as_str()), (run, expected));
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("scene {expected} did not reach the store"));
        }
        shell.stop(run).await;
        assert_eq!(
            timeout(PATIENCE, shell.state())
                .await
                .expect("Stop with the fourth write held"),
            (run, Phase::Interrupted)
        );
        release.send(()).expect("release the fourth admitted write");
        drop(shell);
        run
    };
    let (ended, stopped) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(stopped, RunId(1));
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "stop")]);

    let store = Store::open(&scratch.db()).expect("reopen");
    let stored = store.drawing_scenes().expect("committed drawings");
    assert_eq!(stored.skipped, []);
    let logs = stored
        .scenes
        .iter()
        .map(|scene| (&scene.header.begin, scene.log.as_slice()))
        .collect::<Vec<_>>();
    let (scenes, global) = SceneState::replay_all(&logs).expect("legal aggregate admission");
    assert_eq!(global.usage().scenes, 4);
    assert_eq!(global.usage().segments, 20_000);
    assert_eq!(
        scenes
            .iter()
            .map(|scene| scene.usage().segments)
            .collect::<Vec<_>>(),
        [5_000; 4]
    );
}
