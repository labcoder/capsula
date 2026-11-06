use super::*;
use kumochi_protocol::constants::{generation_hex, generation_target};

#[tokio::test]
async fn a_duplicate_notification_never_queues_or_rechecks_a_committed_key() {
    let scratch = Scratch::new("stage-runtime-once");
    let (mut account, store, requests) = keyed(&scratch).await;
    let text = "sk-ant-api-new-runtime-key";
    let plan = begin(&mut account, 1, u16::try_from(text.len()).expect("length")).await;
    plant(&store, plan, text);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    let event = next(&mut account).await;
    assert_eq!(
        account.handle(event),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Ok(())
        }]
    );
    settle(&mut account).await;
    assert_eq!(
        account
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some(text)
    );
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
    let (sender, queued) = std::sync::mpsc::channel();
    account.jobs = Some(sender);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn stale_begin_replies_and_failed_queue_cannot_admit_a_paste() {
    let scratch = Scratch::new("stage-runtime-admission");
    let (mut account, _, _) = keyed(&scratch).await;
    let plan = begin(&mut account, 1, 16).await;
    let (sender, queued) = std::sync::mpsc::channel();
    account.jobs = Some(sender);
    account.connections.stage_request = Some(3);
    account.connections.stage_admission = None;
    for (epoch, request) in [(1, 2), (2, 3)] {
        assert_eq!(
            account.connection_event(ConnectionEvent::StagePlan {
                epoch,
                request,
                result: Ok(plan)
            }),
            vec![]
        );
        assert_eq!(account.connections.stage_request, Some(3));
        assert_eq!(account.connections.stage_admission, None);
    }
    account.connections.removing = true;
    assert_eq!(
        account.stage_op(
            1,
            ConnectionOp::BeginAnthropicKeyStage {
                request: 4,
                len: 16
            }
        ),
        vec![ControlToShell::AnthropicKeyStage {
            request: 4,
            result: Err(KeyStageFailure::Unavailable)
        }]
    );
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    account.connections.removing = false;
    drop(queued);
    assert_eq!(
        account.stage_op(
            1,
            ConnectionOp::BeginAnthropicKeyStage {
                request: 5,
                len: 16
            }
        ),
        vec![ControlToShell::AnthropicKeyStage {
            request: 5,
            result: Err(KeyStageFailure::Unavailable)
        }]
    );
    account.connections.stage_admission = Some(plan.id);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Unavailable)
        }]
    );
    assert_eq!(account.connections.stage_admission, None);
    ready_old(&account);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

async fn keyed(scratch: &Scratch) -> (Accounts, FakeStore, Arc<std::sync::atomic::AtomicUsize>) {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let owned = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(owned.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let mut account = Accounts::spawn(
        worker,
        Net {
            http: Arc::new(move || Ok(SharedHttp::new(CountedCatalog(Arc::clone(&counted))))),
            ..test_support::net()
        },
    )
    .expect("worker");
    settle(&mut account).await;
    account.connections.models = vec![models::CatalogModel {
        info: kumochi_protocol::message::ModelInfo {
            slug: kumochi_protocol::message::ModelSlug::new("kept-model").expect("slug"),
            display_name: kumochi_protocol::message::ModelName::new("Kept model").expect("name"),
        },
        capabilities: models::Capabilities::default(),
    }];
    account.connections.stage_session(Some(1));
    (account, store, requests)
}

async fn next(account: &mut Accounts) -> Event {
    timeout(Duration::from_secs(5), account.next())
        .await
        .expect("worker completion")
}

async fn begin(account: &mut Accounts, request: u64, len: u16) -> KeyStagePlan {
    assert_eq!(
        account.stage_op(1, ConnectionOp::BeginAnthropicKeyStage { request, len }),
        vec![]
    );
    let event = next(account).await;
    let frames = account.handle(event);
    let [
        ControlToShell::AnthropicKeyStage {
            request: got,
            result: Ok(plan),
        },
    ] = frames.as_slice()
    else {
        panic!("plan reply required")
    };
    assert_eq!(*got, request);
    assert_eq!(plan.len, len);
    *plan
}

fn plant(store: &FakeStore, plan: KeyStagePlan, text: &str) {
    let layout = plan.layout().expect("plan layout");
    let mut start = 0;
    for index in 0..layout.chunks() {
        let end = start + layout.chunk_len(index);
        store
            .set(
                &generation_target("key-test", "anthropic", &generation_hex(&plan.id.0), index),
                &Secret::new(Zeroizing::new(text.as_bytes()[start..end].to_vec())),
            )
            .expect("staged");
        start = end;
    }
}

fn ready_old(account: &Accounts) {
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    assert_eq!(
        account
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some("sk-ant-api-fake-saved-value")
    );
    assert_eq!(account.connections.models.len(), 1);
    assert_eq!(
        account.connections.models[0].info.slug.as_str(),
        "kept-model"
    );
    assert_eq!(
        account.connections.models[0].info.display_name.as_str(),
        "Kept model"
    );
    assert!(!account.connections.loading);
    assert!(account.connections.read.is_none());
}

#[tokio::test]
async fn failed_or_superseded_paste_keeps_the_working_key_and_catalog() {
    let scratch = Scratch::new("stage-runtime-failed");
    let (mut account, store, requests) = keyed(&scratch).await;
    let plan = begin(&mut account, 1, 16).await;
    let generation = account.connections.generation;
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    ready_old(&account);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(account.connections.generation, generation);
    let event = next(&mut account).await;
    assert_eq!(
        account.handle(event),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Unavailable)
        }]
    );
    ready_old(&account);
    let first = begin(&mut account, 2, 16).await;
    assert_eq!(
        account.stage_op(
            1,
            ConnectionOp::BeginAnthropicKeyStage {
                request: 3,
                len: 16
            }
        ),
        vec![]
    );
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: first.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: first.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    ready_old(&account);
    let event = next(&mut account).await;
    let frames = account.handle(event);
    let [
        ControlToShell::AnthropicKeyStage {
            request: 3,
            result: Ok(current),
        },
    ] = frames.as_slice()
    else {
        panic!("the replacement plan must succeed")
    };
    assert_eq!(current.len, 16);
    plant(&store, *current, "sk-ant-api123456");
    store.0.lock().expect("fake").refusing = true;
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: current.id }),
        vec![]
    );
    ready_old(&account);
    let event = next(&mut account).await;
    assert_eq!(
        account.handle(event),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: current.id,
            result: Err(KeyStageFailure::Unavailable)
        }]
    );
    store.0.lock().expect("fake").refusing = false;
    ready_old(&account);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        store
            .names()
            .iter()
            .all(|name| !name.contains("anthropic-key-staged"))
    );
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn committed_paste_survives_disconnect_but_its_reply_does_not_cross_links() {
    let scratch = Scratch::new("stage-runtime-disconnect");
    let (mut account, store, _) = keyed(&scratch).await;
    let text = "sk-ant-api-new-runtime-key";
    let plan = begin(&mut account, 1, u16::try_from(text.len()).expect("length")).await;
    plant(&store, plan, text);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    ready_old(&account);
    account.connections.stage_session(None);
    account.connections.stage_session(Some(2));
    let event = next(&mut account).await;
    assert_eq!(account.handle(event), vec![]);
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    settle(&mut account).await;
    assert_eq!(
        account
            .connections
            .key
            .as_ref()
            .map(|key| key.expose().as_str()),
        Some(text)
    );
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    assert_eq!(
        account.stage_op(2, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn fifo_adopt_then_begin_uses_the_new_head_and_remove_fences_late_results() {
    let scratch = Scratch::new("stage-runtime-fifo");
    let (mut account, store, _) = keyed(&scratch).await;
    let text = "sk-ant-api-new-runtime-key";
    let plan = begin(&mut account, 1, u16::try_from(text.len()).expect("len")).await;
    plant(&store, plan, text);
    account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id });
    account.stage_op(
        1,
        ConnectionOp::BeginAnthropicKeyStage {
            request: 2,
            len: 16,
        },
    );
    let event = next(&mut account).await;
    assert!(matches!(
        event,
        Event::Connection(ConnectionEvent::StageDone { .. })
    ));
    let planned = next(&mut account).await;
    assert!(matches!(
        planned,
        Event::Connection(ConnectionEvent::StagePlan { .. })
    ));
    assert_eq!(
        account.handle(event),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Ok(())
        }]
    );
    let frames = account.handle(planned);
    let [
        ControlToShell::AnthropicKeyStage {
            request: 2,
            result: Ok(second),
        },
    ] = frames.as_slice()
    else {
        panic!("the following Begin must succeed")
    };
    plant(&store, *second, "sk-ant-api123456");
    account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: second.id });
    let adopted = timeout(Duration::from_secs(5), async {
        loop {
            let event = account.next().await;
            if matches!(event, Event::Connection(ConnectionEvent::StageDone { .. })) {
                break account.handle(event);
            }
            account.handle(event);
        }
    })
    .await
    .expect("second paste completes");
    assert_eq!(
        adopted,
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: second.id,
            result: Ok(())
        }]
    );
    settle(&mut account).await;
    let generation = account.connections.generation;
    account.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey));
    assert_eq!(
        account.connection_event(ConnectionEvent::StageDone {
            epoch: 1,
            generation,
            plan: plan.id,
            result: Ok(key())
        }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Err(KeyStageFailure::Changed)
        }]
    );
    settle(&mut account).await;
    assert_eq!(account.connections.view.anthropic, KeyView::Missing);
    assert!(account.connections.key.is_none());
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}
