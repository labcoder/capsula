use kumochi_protocol::message::{
    ConnectionKind, Effort, GestureId, ModelChoice, ModelSlug, SettingsOp,
};

use super::*;
use crate::context::{Origin, Screen};
use crate::coordinator::{Effect, HoldKey};
use crate::engines::{RunRequest, SendGate};
use crate::screen::ScreenContext;

const WAIT: Duration = Duration::from_secs(10);
const PIPE_BYTES: usize = 600 * 1024;

async fn pictured_request(brain: &mut Brain) -> RunRequest {
    let setting = brain
        .config
        .stage(SettingsOp::SetDefaultModel(ModelChoice {
            connection: ConnectionKind::AnthropicKey,
            model: ModelSlug::new(MODEL).expect("fixture model"),
            effort: Effort::Low,
        }))
        .expect("fixture default");
    brain.config.commit(setting);
    timeout(WAIT, async {
        while brain.account.connections.anthropic() != KeyView::Ready {
            let event = brain.account.next().await;
            brain.account_input(event, None).expect("account event");
        }
    })
    .await
    .expect("verified catalog");
    let hold = HoldKey {
        epoch: 0,
        gesture: GestureId(1),
    };
    let read = brain
        .coordinator
        .prepare_hold(
            hold,
            Some(ThreadId(1)),
            1,
            brain.snapshot(Origin::Kumo),
            Arc::default(),
        )
        .expect("prepare held turn");
    brain
        .apply(vec![Effect::Store(read)], None)
        .expect("read hold");
    let Some(Input::HoldPrepared {
        result: Ok(hold), ..
    }) = timeout(WAIT, brain.inputs.recv())
        .await
        .expect("hold snapshot")
    else {
        panic!("no held context")
    };
    let mut jpeg = vec![b'A'; 2 * PIPE_BYTES];
    jpeg[..3].copy_from_slice(b"\xff\xd8\xff");
    jpeg.extend_from_slice(b"\xff\xd9");
    let screen = ScreenContext::fixture(&jpeg);
    let geometry = screen.geometry();
    let effects = brain
        .coordinator
        .submit_hold(
            hold,
            "look at this".into(),
            Some(Screen::Picture(screen.into_jpeg(), None, geometry)),
        )
        .expect("submit pictured hold");
    brain.apply(effects, None).expect("held turn effects");
    timeout(WAIT, async {
        loop {
            let input = brain.inputs.recv().await.expect("brain input");
            let mut effects = brain.handle(input).expect("held turn input");
            if matches!(effects.last(), Some(Effect::StartEngine { .. })) {
                let Some(Effect::StartEngine { request }) = effects.pop() else {
                    unreachable!()
                };
                brain.apply(effects, None).expect("start effects");
                break request;
            }
            brain.apply(effects, None).expect("build effects");
        }
    })
    .await
    .expect("pictured build")
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered upload, Esc, and peer shutdown"
)]
async fn escape_retires_the_key_before_a_partial_picture_upload_can_write_again() {
    let scratch = Scratch::new("offline-key-escape-picture");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("offline-key-runtime.lock");
    Vault::with_service(store.clone(), "offline-key-runtime", &lock)
        .expect("fake vault")
        .save_anthropic_key(KEY.as_bytes())
        .expect("synthetic key");
    let worker = Worker::new(
        Box::new(move || Vault::with_service(store.clone(), "offline-key-runtime", &lock)),
        Arc::new(FixedClock(0)),
    );
    let catalog = json!({
        "data": [{"type": "model", "id": MODEL, "display_name": "Synthetic model",
                  "capabilities": {"image_input": {"supported": true}}}],
        "has_more": false
    });
    let script = Scripted(Arc::new(Mutex::new(Script {
        responses: [http("application/json", &catalog.to_string())].into(),
        ..Script::default()
    })));
    let mut accounts = Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker");
    accounts
        .connections
        .use_test_transport(Transport::offline(script.clone(), 1, 1));
    let mut brain = test_support::brain_with(&scratch, "", accounts);
    let request = pictured_request(&mut brain).await;
    let run = request.run;
    let gate = SendGate::new(
        request.key,
        Arc::clone(&brain.asks),
        brain.inputs_tx.clone(),
    );
    let engine = brain.owned_engine(&request).expect("owned Key engine");
    let (events, _received) = mpsc::channel(16);
    let mut round = engine.start(request, events, gate);
    let (io, mut peer) = tokio::io::duplex(PIPE_BYTES);
    script.0.lock().expect("script").stream = Some(io);
    let mut prefix = vec![0; PIPE_BYTES];
    timeout(WAIT, async {
        let read = peer.read_exact(&mut prefix);
        tokio::pin!(read);
        loop {
            tokio::select! {
                biased;
                got = &mut read => { got.expect("partial POST"); break; }
                () = &mut round => panic!("round ended before the partial write"),
                input = brain.inputs.recv() => brain.offline(input.expect("send ask")),
            }
        }
    })
    .await
    .expect("pictured upload reached the peer");
    assert!(prefix.starts_with(b"POST /v1/messages "));
    let jpeg_type = br#""media_type":"image/jpeg""#;
    assert!(
        prefix
            .windows(jpeg_type.len())
            .any(|bytes| bytes == jpeg_type)
    );
    assert!(
        prefix[PIPE_BYTES - 32..]
            .iter()
            .all(|byte| b"QUFB".contains(byte))
    );

    let (out, mut controls) = mpsc::channel(64);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let stop = brain
        .control_frame(
            ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            },
            0,
            &mut link,
            &mut None,
        )
        .expect("connected Esc")
        .expect("Stop input");
    let effects = brain.handle(stop).expect("Stop effects");
    brain.apply(effects, Some(&mut link)).expect("apply Stop");
    let interrupted = std::iter::from_fn(|| controls.try_recv().ok()).any(|wire| {
        matches!(
            frame::decode::<ControlToShell>(&wire[frame::HEADER_LEN..]).expect("control frame"),
            ControlToShell::RunState { run: ended, phase: Phase::Interrupted, .. } if ended == run
        )
    });

    let mut next = [0];
    let after = timeout(WAIT, async {
        tokio::select! {
            read = peer.read(&mut next) => read,
            () = &mut round => peer.read(&mut next).await,
        }
    })
    .await
    .expect("the peer closed or received another byte")
    .expect("peer read");
    drop(round);
    brain.close().await;
    assert!(interrupted);
    assert_eq!(
        after, 0,
        "Esc must fence the next picture write before the task aborts"
    );
}
