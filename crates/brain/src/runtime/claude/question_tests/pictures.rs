use std::sync::mpsc as std_mpsc;

use super::*;
use crate::context::{InputItem, Jpeg, Role};
use crate::engines::ClaudeStreamFailure;
use crate::engines::claude_code::lane::tests::PICTURE_JPEG;
use crate::screen::ScreenContext;
use crate::test_support::{fixture_has_picture_marker, has_picture_marker};

type TeardownBarrier = (oneshot::Sender<()>, std_mpsc::Receiver<()>);
static PICTURE_TEARDOWN: Mutex<Option<TeardownBarrier>> = Mutex::new(None);

fn hold_picture_teardown(job: &Job) -> io::Result<()> {
    let barrier = PICTURE_TEARDOWN.lock().expect("test barrier").take();
    if let Some((reached, resume)) = barrier {
        reached.send(()).ok();
        resume.recv_timeout(WAIT).map_err(io::Error::other)?;
    }
    job.terminate()
}

fn request_with(image: Jpeg) -> RunRequest {
    let mut request = choosing("haiku", Effort::Low, ConnectionKind::ClaudeCode);
    request.request.input = vec![
        InputItem::Message {
            role: Role::User,
            text: "look".to_owned(),
        },
        InputItem::Image(image),
        InputItem::Message {
            role: Role::Developer,
            text: "captured content".to_owned(),
        },
    ];
    request
}

fn pictured_lease(rig: &mut Rig, played: &Played, picture: PictureGrant) -> Answer {
    let Answer::Grant(lease) = played.lease() else {
        panic!("test grant");
    };
    rig.claude.picture_granted(
        played.key,
        Some(ConnectionKind::ClaudeCode),
        picture.clone(),
    );
    Answer::Grant(lease.with_picture(Some(picture)))
}

async fn receipt(played: &mut Played, decision: fn(LeaseId) -> Input) {
    let lease = LeaseId {
        key: played.key,
        send: 1,
    };
    let actual = timeout(WAIT, played.inputs.recv())
        .await
        .expect("settled in time");
    assert_eq!(actual, Some(decision(lease)));
}

#[tokio::test]
async fn a_pictured_question_only_writes_to_stdin_and_stop_removes_the_folder() {
    let captured = Captured::default();
    let _logs = captured.install();
    let mut script = offering_nothing().held_at_user("keep-open");
    script
        .user
        .insert(0, Value::String("!expect-picture".to_owned()));
    let mut rig = Rig::new("picture-sink", &script).await;
    let request = request_with(ScreenContext::fixture(PICTURE_JPEG).into_jpeg());
    let question = rig.claude.question(&request).expect("admitted");
    let (id, _) = rig.last();
    let folder = rig.base.owned().join(ENGINE_DIR).join(id.hex());
    assert!(!has_picture_marker(
        format!("{question:?}{request:?}").as_bytes(),
        PICTURE_JPEG
    ));
    let mut played = play_with(&question, request);
    let lease = pictured_lease(&mut rig, &played, PictureGrant::open_for_tests());
    rig.drive(played.asked(lease)).await;
    until("validated the actual stdin shape", || {
        rig.base.0.join("picture-shape.1").is_file()
    })
    .await;
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status", "1 user"]);
    for dir in [&folder, &folder.join("work"), &folder.join("tmp")] {
        assert!(dir.is_dir());
        drop(crate::platform::owner_only_dir(dir).expect("owner-only launch directory"));
    }
    let sibling = rig.base.0.join("picture-sibling");
    let scan = |base: &Base| {
        fixture_has_picture_marker(&folder, PICTURE_JPEG)
            || [
                "starts.log",
                "stdin.log",
                "seed.1",
                "picture-shape.1",
                "picture-sibling",
            ]
            .iter()
            .any(|name| fixture_has_picture_marker(&base.0.join(name), PICTURE_JPEG))
    };
    assert!(!scan(&rig.base));
    fs::write(&sibling, &PICTURE_JPEG[3..]).expect("positive control");
    assert!(scan(&rig.base), "the named sibling is in scope");
    fs::remove_file(sibling).expect("remove test copy");
    played.round.abort();
    assert!(
        (&mut played.round)
            .await
            .expect_err("stopped")
            .is_cancelled()
    );
    receipt(&mut played, |lease| Input::SendSettled { lease }).await;
    until("removed after Stop", || !folder.exists()).await;
    assert!(!scan(&rig.base));
    assert!(!has_picture_marker(
        captured.text().as_bytes(),
        PICTURE_JPEG
    ));
    rig.close().await;
}

#[tokio::test]
async fn a_missing_or_closed_grant_or_unsealed_capture_never_seeds() {
    for reason in ["no-grant", "closed", "unsealed"] {
        let mut rig = Rig::new(reason, &offering_nothing()).await;
        let image = if reason == "unsealed" {
            Jpeg::new(PICTURE_JPEG.to_vec()).expect("marker")
        } else {
            ScreenContext::fixture(PICTURE_JPEG).into_jpeg()
        };
        let request = request_with(image);
        let question = rig.claude.question(&request).expect("admitted");
        let (id, _) = rig.last();
        let folder = rig.base.owned().join(ENGINE_DIR).join(id.hex());
        let mut played = play_with(&question, request);
        let lease = if reason == "no-grant" {
            played.lease()
        } else {
            let grant = PictureGrant::open_for_tests();
            let answer = pictured_lease(&mut rig, &played, grant.clone());
            if reason == "closed" {
                grant.close();
            }
            answer
        };
        rig.drive(played.asked(lease)).await;
        let events = rig.drive(played.events()).await;
        let failure = TurnEvent::Finished(Finish::Failed(EngineFailure::Provider));
        if reason == "no-grant" {
            assert_eq!(events, [failure]);
            receipt(&mut played, |lease| Input::SendUnsent { lease }).await;
        } else {
            assert_eq!(events, [TurnEvent::Finished(Finish::ReceiptOrdered)]);
            receipt(&mut played, |lease| Input::SendUnsent { lease }).await;
            let SendKey::Build(key) = played.key else {
                panic!("pictured build")
            };
            assert_eq!(
                played.inputs.try_recv(),
                Ok(Input::Engine {
                    key,
                    event: failure
                })
            );
        }
        assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
        until("removed after refusal", || !folder.exists()).await;
        rig.close().await;
    }
}

#[tokio::test]
async fn closed_picture_grant_does_not_truncate_while_teardown_holds_the_receipt() {
    let mut rig = Rig::with("picture-pump-rebuild", &offering_nothing(), |host| {
        host.terminate = hold_picture_teardown;
    })
    .await;
    let (reached, at_teardown) = oneshot::channel();
    let (resume, held) = std_mpsc::channel();
    *PICTURE_TEARDOWN.lock().expect("test barrier") = Some((reached, held));

    let request = request_with(ScreenContext::fixture(PICTURE_JPEG).into_jpeg());
    let question = rig.claude.question(&request).expect("admitted");
    let key = request.key;
    let (core, mut receipts) = mpsc::unbounded_channel();
    let (engine, mut events) = mpsc::channel(1);
    let asks = Asks::default();
    let pump = engines::start(
        &question,
        request,
        engine,
        SendGate::new(key, Arc::clone(&asks), core.clone()),
    );
    assert!(
        matches!(rig.drive(receipts.recv()).await, Some(Input::SendReady { key: asked, .. }) if asked == key)
    );
    let lease = LeaseId { key, send: 1 };
    let grant = PictureGrant::open_for_tests();
    grant.close();
    engines::answer(
        &asks,
        key,
        Answer::Grant(SendLease::new(lease, core).with_picture(Some(grant))),
    );
    timeout(WAIT, at_teardown)
        .await
        .expect("held teardown")
        .expect("arrived");
    let before_settlement = timeout(Duration::from_millis(300), events.recv()).await;
    let premature_receipt = receipts.try_recv();
    resume.send(()).expect("release teardown");

    assert!(
        before_settlement.is_err(),
        "the production pump must not synthesize Truncated: {before_settlement:?}"
    );
    assert_eq!(premature_receipt, Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(
        timeout(WAIT, receipts.recv()).await.expect("settled"),
        Some(Input::SendUnsent { lease })
    );
    let SendKey::Build(key) = key else {
        panic!("pictured build")
    };
    assert_eq!(
        timeout(WAIT, receipts.recv())
            .await
            .expect("ordered terminal outcome"),
        Some(Input::Engine {
            key,
            event: TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))
        })
    );
    assert_eq!(
        timeout(WAIT, events.recv()).await.expect("pump ended"),
        None
    );
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    drop(pump);
    rig.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered launch, teardown and rebuild"
)]
async fn a_bound_find_keeps_the_round_until_its_picture_settles_unsent() {
    use crate::coordinator::{Effect, StoreOp, built_for, pictured_claude_run};
    use crate::engines::claude_code::supervisor::tests::{Barrier, barrier};

    let script = synthetic_signed_in()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new("picture-bound-find", &script).await;
    let scratch = Scratch::new("picture-bound-find");
    let mut brain = brain_on(&mut rig, &scratch);
    brain.claude.fixture_picture = Some(ModelSlug::new("haiku").expect("alias"));
    let (coordinator, effects) = pictured_claude_run(
        brain.claude.availability(),
        ScreenContext::fixture(PICTURE_JPEG).into_jpeg(),
    );
    *brain.coordinator = coordinator;
    let Some(Effect::StartEngine { request }) = effects.last() else {
        panic!("pictured request was not admitted");
    };
    let SendKey::Build(key) = request.key else {
        panic!("pictured build")
    };
    let choice = request.request.choice.clone().expect("run choice");
    let config = brain
        .config
        .stage(kumochi_protocol::message::SettingsOp::SetDefaultModel(
            choice.clone(),
        ))
        .unwrap();
    brain.config.commit(config);
    brain.apply(effects, None).expect("launch");

    let (pause_write, writing) = barrier();
    let (pause_teardown, teardown) = barrier();
    let held = brain.claude.launches.last().expect("launch");
    {
        let mut hooks = held.control.picture_hooks.lock().expect("hooks");
        hooks.write = Some(pause_write);
        hooks.terminate = Some(pause_teardown);
    }
    let note = timeout(WAIT, brain.claude.notes.1.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(note, Note::Bind(_)),
        "expected a bind, got {note:?}"
    );
    brain.claude_done(Wake::Note(note), None).expect("bound");
    assert!(brain.claude.launches.last().expect("launch").bound);
    let ask = timeout(WAIT, brain.inputs.recv()).await.unwrap().unwrap();
    let grants = brain.handle(ask).expect("grant effects");
    let lease = LeaseId {
        key: SendKey::Build(key),
        send: 1,
    };
    assert!(grants.contains(&Effect::Grant(lease)));
    brain.apply(grants, None).expect("grant");

    let reached = |gate: Barrier| {
        tokio::task::spawn_blocking(move || {
            assert!(gate.reached_within(WAIT), "reached barrier");
            gate
        })
    };
    let writing = reached(writing).await.expect("picture poised");
    let generation = brain.claude.generation;
    brain
        .claude_op(ConnectionOp::FindClaudeCode, None)
        .expect("find");
    assert_eq!(brain.claude.generation, generation + 1);
    let teardown = reached(teardown).await.expect("send sealed");
    assert_eq!(brain.coordinator.current_run(), Some(key.run));
    assert!(
        brain
            .coordinator
            .takes_pictures(ConnectionKind::ClaudeCode, &choice.model)
    );
    assert!(
        brain.coordinator.picture_lost_for_test(key.run),
        "generation loss is sticky before settlement"
    );
    writing.release();
    let early = timeout(Duration::from_millis(300), brain.engine_events.recv()).await;
    let early_receipt = brain.inputs.try_recv();
    teardown.release();
    assert!(
        early.is_err(),
        "no terminal event before settlement: {early:?}"
    );
    assert_eq!(early_receipt, Err(mpsc::error::TryRecvError::Empty));

    let receipt = timeout(WAIT, brain.inputs.recv()).await.unwrap().unwrap();
    assert_eq!(receipt, Input::SendUnsent { lease });
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    let effects = brain.handle(receipt).expect("rebuild");
    let Some(Effect::Store(StoreOp::BuildContext(again))) = effects.last() else {
        panic!("no text-only rebuild");
    };
    assert_eq!(
        again.key,
        crate::coordinator::BuildKey {
            attempt: key.attempt + 1,
            ..key
        }
    );
    let rebuilt = brain
        .handle(built_for(again.key, &choice))
        .expect("rebuilt");
    let Some(Effect::StartEngine { request }) = rebuilt.last() else {
        panic!("text rebuild was not admitted");
    };
    assert_eq!(request.request.choice, Some(choice));
    let text = |role, text: &str| InputItem::Message {
        role,
        text: text.into(),
    };
    assert_eq!(
        request.request.input,
        [
            text(Role::User, "words"),
            text(
                Role::Developer,
                "The screen was not included: this model can't be sent images."
            ),
        ]
    );
    brain.close().await;
}

/// A sealed capture is already in its built request when Claude Code's generation moves after the
/// launch binds and before its grant: the ask is refused, the fake child gets no picture byte, and
/// the run builds again text-only under the same route and choice.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered launch, refusal and rebuild"
)]
async fn a_generation_move_before_the_grant_refuses_the_picture_and_rebuilds_text_only() {
    use crate::coordinator::{Effect, StoreOp, built_for, pictured_claude_run};

    let script = synthetic_signed_in()
        .pre_tools(BOTH_TOOLS)
        .post_tools(BOTH_TOOLS);
    let mut rig = Rig::new("picture-pregrant-find", &script).await;
    let scratch = Scratch::new("picture-pregrant-find");
    let mut brain = brain_on(&mut rig, &scratch);
    brain.claude.fixture_picture = Some(ModelSlug::new("haiku").expect("alias"));
    let (coordinator, effects) = pictured_claude_run(
        brain.claude.availability(),
        ScreenContext::fixture(PICTURE_JPEG).into_jpeg(),
    );
    *brain.coordinator = coordinator;
    let Some(Effect::StartEngine { request }) = effects.last() else {
        panic!("pictured request was not admitted");
    };
    assert!(
        request
            .request
            .input
            .iter()
            .any(|item| matches!(item, InputItem::Image(_))),
        "the sealed capture is in the built request"
    );
    let SendKey::Build(key) = request.key else {
        panic!("pictured build")
    };
    let choice = request.request.choice.clone().expect("run choice");
    let config = brain
        .config
        .stage(kumochi_protocol::message::SettingsOp::SetDefaultModel(
            choice.clone(),
        ))
        .unwrap();
    brain.config.commit(config);
    brain.apply(effects, None).expect("launch");
    let note = timeout(WAIT, brain.claude.notes.1.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(note, Note::Bind(_)), "expected a bind");
    brain.claude_done(Wake::Note(note), None).expect("bound");
    let launch = brain.claude.launches.last().expect("launch").id;
    let folder = rig.base.owned().join(ENGINE_DIR).join(launch.hex());
    let ask = timeout(WAIT, brain.inputs.recv()).await.unwrap().unwrap();
    assert!(matches!(
        ask,
        Input::SendReady {
            key: SendKey::Build(asked),
            ..
        } if asked == key
    ));

    let generation = brain.claude.generation;
    brain
        .claude_op(ConnectionOp::FindClaudeCode, None)
        .expect("find");
    assert_eq!(brain.claude.generation, generation + 1);
    let refused = brain.handle(ask).expect("refusal effects");
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::Grant(_))),
        "no grant after the generation moved"
    );
    assert!(refused.contains(&Effect::Rebuild(SendKey::Build(key))));
    let Some(Effect::Store(StoreOp::BuildContext(again))) = refused.last() else {
        panic!("no text-only rebuild");
    };
    let again = again.key;
    assert_eq!(
        again,
        crate::coordinator::BuildKey {
            attempt: key.attempt + 1,
            ..key
        }
    );
    brain.apply(refused, None).expect("refusal");
    until("refused launch removed", || !folder.exists()).await;
    assert_eq!(rig.stdin(), ["1 initialize", "1 mcp_status"]);
    let scan = || {
        fixture_has_picture_marker(&folder, PICTURE_JPEG)
            || [
                "starts.log",
                "stdin.log",
                "seed.1",
                "picture-shape.1",
                "picture-sibling",
            ]
            .iter()
            .any(|name| fixture_has_picture_marker(&rig.base.0.join(name), PICTURE_JPEG))
    };
    assert!(!scan());
    let sibling = rig.base.0.join("picture-sibling");
    fs::write(&sibling, &PICTURE_JPEG[3..]).expect("positive control");
    assert!(scan(), "the named sibling is in scope");
    fs::remove_file(sibling).expect("remove test copy");

    let rebuilt = brain.handle(built_for(again, &choice)).expect("rebuilt");
    let Some(Effect::StartEngine { request }) = rebuilt.last() else {
        panic!("text rebuild was not admitted");
    };
    assert_eq!(request.key, SendKey::Build(again));
    assert_eq!(request.request.choice, Some(choice));
    let text = |role, text: &str| InputItem::Message {
        role,
        text: text.into(),
    };
    assert_eq!(
        request.request.input,
        [
            text(Role::User, "words"),
            text(
                Role::Developer,
                "The screen was not included: this model can't be sent images."
            ),
        ]
    );
    brain.close().await;
}

#[tokio::test]
async fn pictured_child_loss_or_echo_retires_without_forwarding_a_picture() {
    for (action, size, finish) in [
        ("!exit", PICTURE_JPEG.len(), Finish::Truncated),
        (
            "!echo-input",
            PICTURE_JPEG.len(),
            Finish::Failed(EngineFailure::ClaudeStream(
                ClaudeStreamFailure::PictureEcho,
            )),
        ),
        (
            "!echo-input",
            800_000,
            Finish::Failed(EngineFailure::ClaudeStream(
                ClaudeStreamFailure::LineTooLong,
            )),
        ),
    ] {
        let mut script = offering_nothing();
        script.user.insert(0, Value::String(action.to_owned()));
        let mut rig = Rig::new("picture-output", &script).await;
        let mut jpeg = PICTURE_JPEG.to_vec();
        jpeg.resize(size, 0);
        let request = request_with(ScreenContext::fixture(&jpeg).into_jpeg());
        let question = rig.claude.question(&request).expect("admitted");
        let (id, _) = rig.last();
        let folder = rig.base.owned().join(ENGINE_DIR).join(id.hex());
        let mut played = play_with(&question, request);
        let lease = pictured_lease(&mut rig, &played, PictureGrant::open_for_tests());
        rig.drive(played.asked(lease)).await;
        assert_eq!(
            rig.drive(played.events()).await,
            [TurnEvent::Finished(finish)]
        );
        receipt(&mut played, |lease| Input::SendSettled { lease }).await;
        until("removed after child loss or echo", || !folder.exists()).await;
        rig.close().await;
    }
}

#[tokio::test]
async fn a_pictured_provider_error_removes_the_childs_private_picture() {
    let captured = Captured::default();
    let _logs = captured.install();
    let mut script = offering_nothing();
    script.user.truncate(1);
    script.user.extend([
        json!("!keep-picture"),
        json!("!wait report-provider-error"),
        json!({"type": "result", "subtype": "provider_error", "is_error": true}),
    ]);
    let mut rig = Rig::new("pictured-provider-error", &script).await;
    let request = request_with(ScreenContext::fixture(PICTURE_JPEG).into_jpeg());
    let question = rig.claude.question(&request).expect("admitted");
    let (id, _) = rig.last();
    let folder = rig.base.owned().join(ENGINE_DIR).join(id.hex());
    let picture = folder.join("tmp").join("images").join("picture.jpg");
    let mut played = play_with(&question, request);
    let lease = pictured_lease(&mut rig, &played, PictureGrant::open_for_tests());
    rig.drive(played.asked(lease)).await;
    until(
        "the child kept its picture before reporting the error",
        || fs::read(&picture).is_ok_and(|bytes| bytes == PICTURE_JPEG),
    )
    .await;
    rig.go("report-provider-error");
    let failed = provider_failed(&mut rig).await;
    receipt(&mut played, |lease| Input::SendSettled { lease }).await;
    until("provider error removed the private launch", || {
        !folder.exists()
    })
    .await;
    assert!(!picture.exists(), "provider error left the child's picture");
    rig.wake(Wake::Note(failed));
    assert_eq!(
        rig.drive(played.events()).await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Provider))]
    );
    for artifact in [rig.base.starts().join("\n"), captured.text()] {
        assert!(
            !has_picture_marker(artifact.as_bytes(), PICTURE_JPEG)
                && !artifact.contains("picture.jpg")
        );
    }
    rig.close().await;
}
