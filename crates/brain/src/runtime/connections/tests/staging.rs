use super::*;
use kumochi_protocol::constants::{generation_hex, generation_target};

mod fold;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one fake vault across both authority mutations"
)]
async fn routed_adoption_and_removal_revoke_at_the_worker_handoff() {
    let scratch = Scratch::new("routed-key-authority");
    let (mut account, store, initial_peer) = keyed(&scratch).await;
    let catalog = r#"{"data":[],"has_more":false}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{catalog}",
        catalog.len()
    )
    .into_bytes();
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        responses: [
            response.clone(),
            response.clone(),
            response.clone(),
            response,
        ]
        .into(),
        ..Script::default()
    })));
    account.connections.transport = Some(Transport::offline(peer.clone(), 4, 0));
    account.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut account).await;
    let first = Arc::clone(account.connections.verified.as_ref().expect("verified"));
    assert_eq!(first.revision, 1);
    assert_ne!(first.tag.0, 0);
    account.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut account).await;
    assert!(Arc::ptr_eq(
        &first,
        account
            .connections
            .verified
            .as_ref()
            .expect("verified again")
    ));
    let plan = begin(&mut account, 1, 16).await;
    account.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    let loaded = next(&mut account).await;
    account.handle(loaded);
    let metadata = account.connections.route_input().expect("route configured");
    assert!(matches!(
        &metadata,
        Input::Engines(ConnectionKind::AnthropicKey, Availability::Ready(ready))
            if ready.connection == first.tag && ready.models.is_empty()
    ));
    assert!(!format!("{metadata:?}").contains("sk-ant-"));
    let queued_catalog = account
        .connections
        .checking
        .clone()
        .expect("catalog queued");
    let observing_cut = |authority: Fence, catalog: Option<Fence>| -> Box<dyn Fn() + Send + Sync> {
        Box::new(move || {
            assert!(
                !authority.open(),
                "the old authority is closed at worker handoff"
            );
            assert!(
                catalog.as_ref().is_none_or(|read| !read.open()),
                "the read is closed at worker handoff"
            );
        })
    };
    account.connections.before_key_mutation = Some(observing_cut(
        first.fence.clone(),
        Some(queued_catalog.clone()),
    ));

    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    assert!(!first.fence.open());
    assert!(!queued_catalog.open());
    assert!(matches!(
        account.connections.route_input(),
        Some(Input::Engines(
            ConnectionKind::AnthropicKey,
            Availability::Unavailable(_)
        ))
    ));
    let failed = next(&mut account).await;
    assert!(matches!(
        failed,
        Event::Connection(ConnectionEvent::StageDone { result: Err(_), .. })
    ));
    account.handle(failed);
    settle(&mut account).await;
    let recovered = Arc::clone(
        account
            .connections
            .verified
            .as_ref()
            .expect("old revision re-proven"),
    );
    assert_eq!(recovered.revision, 1);
    assert_ne!(recovered.tag, first.tag);
    assert!(!first.fence.open());
    assert_eq!(peer.0.lock().expect("peer").starts, 3);

    let replacement = "sk-ant-api-synthetic-replacement";
    let plan = begin(
        &mut account,
        2,
        u16::try_from(replacement.len()).expect("bounded"),
    )
    .await;
    plant(&store, plan, replacement);
    account.connections.before_key_mutation = Some(observing_cut(recovered.fence.clone(), None));
    account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id });
    assert!(!recovered.fence.open());
    let adopted = next(&mut account).await;
    assert!(matches!(
        adopted,
        Event::Connection(ConnectionEvent::StageDone {
            result: Ok((2, _)),
            ..
        })
    ));
    account.handle(adopted);
    settle(&mut account).await;
    let new = Arc::clone(account.connections.verified.as_ref().expect("replacement"));
    assert_ne!(new.tag, recovered.tag);
    account.connections.before_key_mutation = Some(observing_cut(new.fence.clone(), None));
    account.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 2,
        request: 3,
    }));
    assert!(!new.fence.open());
    settle(&mut account).await;
    assert_eq!(account.connections.view.anthropic, KeyView::Missing);
    assert!(account.connections.verified.is_none());
    assert_eq!(starts(&initial_peer), 1);
    assert_eq!(peer.0.lock().expect("peer").starts, 4);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[test]
fn the_staging_worker_does_not_print_the_key_length() {
    for len in [127, 128] {
        let work = StageWork::Begin {
            epoch: 1,
            request: 7,
            len,
        };
        assert_eq!(format!("{work:?}"), "StageWork::Begin");
    }
}

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
    assert_eq!(retained_key(&account.connections), Some(text));
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    assert_eq!(starts(&requests), 2);
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
                result: Ok((1, plan))
            }),
            vec![]
        );
        assert_eq!(account.connections.stage_request, Some(3));
        assert_eq!(account.connections.stage_admission, None);
    }
    account.connections.removing = Some(1);
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
    account.connections.removing = None;
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
        [
            view_frame(KeyView::Unconfirmed, Some(1)),
            ControlToShell::AnthropicKeyStageDone {
                plan: plan.id,
                result: Err(KeyStageFailure::Unavailable),
            },
        ]
    );
    assert_eq!(account.connections.stage_admission, None);
    assert!(account.connections.verified.is_none());
    assert_eq!(account.connections.view.anthropic, KeyView::Unconfirmed);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

async fn keyed(scratch: &Scratch) -> (Accounts, FakeStore, Scripted) {
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let owned = store.clone();
    let worker = Worker::new(
        Box::new(move || Vault::with_service(owned.clone(), "key-test", &lock)),
        Arc::new(FixedClock(0)),
    );
    let requests = peer(KEPT_CATALOG);
    let mut account = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        requests.clone(),
        8,
    );
    settle(&mut account).await;
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
        retained_key(&account.connections),
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

fn view_frame(anthropic: KeyView, anthropic_revision: Option<u64>) -> ControlToShell {
    ControlToShell::Connections(ConnectionsView {
        anthropic,
        anthropic_revision,
        claude_code: ClaudeCodeView::NotFound,
    })
}

#[tokio::test]
async fn failed_or_superseded_paste_keeps_the_working_key_and_catalog() {
    let scratch = Scratch::new("stage-runtime-failed");
    let (mut account, store, requests) = keyed(&scratch).await;
    let plan = begin(&mut account, 1, 16).await;
    let generation = account.connections.generation;
    let revision = account.connections.anthropic_revision();
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    assert!(account.connections.verified.is_none());
    assert_eq!(starts(&requests), 1);
    assert_eq!(account.connections.generation, generation + 1);
    let event = next(&mut account).await;
    assert_eq!(
        account.handle(event),
        [
            view_frame(KeyView::Checking, None),
            ControlToShell::AnthropicKeyStageDone {
                plan: plan.id,
                result: Err(KeyStageFailure::Unavailable),
            },
        ]
    );
    settle(&mut account).await;
    ready_old(&account);
    assert_eq!(account.connections.anthropic_revision(), revision);
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
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    assert!(account.connections.verified.is_none());
    let event = next(&mut account).await;
    store.0.lock().expect("fake").refusing = false;
    assert_eq!(
        account.handle(event),
        [
            view_frame(KeyView::Checking, None),
            ControlToShell::AnthropicKeyStageDone {
                plan: current.id,
                result: Err(KeyStageFailure::Unavailable),
            },
        ]
    );
    settle(&mut account).await;
    ready_old(&account);
    assert_eq!(account.connections.anthropic_revision(), revision);
    assert_eq!(starts(&requests), 3);
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
async fn failed_first_paste_updates_the_reconnected_shell_without_an_old_receipt() {
    let scratch = Scratch::new("stage-failed-first");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    let (entered, at_gate) = std::sync::mpsc::channel();
    let (resume, held) = std::sync::mpsc::channel();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut held_store = Some(HeldStore {
        armed: Arc::clone(&armed),
        store: store.clone(),
        entered,
        resume: std::sync::Mutex::new(Some(held)),
    });
    let worker = Worker::new(
        Box::new(move || {
            Vault::with_service(held_store.take().expect("one open"), "key-test", &lock)
        }),
        Arc::new(FixedClock(0)),
    );
    let peer = peer(EMPTY_CATALOG);
    let mut account = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        peer.clone(),
        1,
    );
    account.connections.stage_session(Some(1));
    assert_eq!(
        account.connections.connected(),
        view_frame(KeyView::Missing, None)
    );
    let plan = begin(&mut account, 1, 16).await;
    plant(&store, plan, "sk-ant-api123456");
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        []
    );
    at_gate
        .recv_timeout(Duration::from_secs(5))
        .expect("adoption waiting on the staged chunk");
    store.0.lock().expect("fake").denied_read = Some(generation_target(
        "key-test",
        "anthropic",
        &generation_hex(&plan.id.0),
        0,
    ));
    account.connections.stage_session(None);
    account.connections.stage_session(Some(2));
    assert_eq!(
        account.connections.connected(),
        view_frame(KeyView::Checking, None)
    );
    resume.send(()).expect("release denied read");
    let failed = next(&mut account).await;
    assert!(matches!(
        &failed,
        Event::Connection(ConnectionEvent::StageDone {
            epoch: 1,
            result: Err(KeyStageFailure::Unavailable),
            ..
        })
    ));
    assert_eq!(
        account.handle(failed),
        [view_frame(KeyView::Unconfirmed, None)]
    );
    assert!(!account.connections.loading);
    assert!(matches!(
        account.connections.route_input(),
        Some(Input::Engines(
            ConnectionKind::AnthropicKey,
            Availability::Unavailable(_)
        ))
    ));
    assert_eq!(starts(&peer), 0);
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
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    assert!(account.connections.verified.is_none());
    account.connections.stage_session(None);
    account.connections.stage_session(Some(2));
    let event = next(&mut account).await;
    assert_eq!(account.handle(event), vec![]);
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    settle(&mut account).await;
    assert_eq!(retained_key(&account.connections), Some(text));
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
async fn recovery_local_read_does_not_wait_for_a_catalog_timeout() {
    recovery_with_unavailable_catalog(None).await;
}

#[tokio::test]
async fn recovery_local_read_does_not_wait_for_a_rate_limited_catalog() {
    recovery_with_unavailable_catalog(Some(429)).await;
}

async fn recovery_with_unavailable_catalog(status: Option<u16>) {
    let scratch = Scratch::new("key-recovery-unavailable-catalog");
    let (mut account, store, _) = keyed(&scratch).await;
    let current = store
        .names()
        .into_iter()
        .find(|name| name.starts_with("key-test/gen/anthropic/"))
        .expect("stored key chunk");
    let entered = Arc::new(Notify::new());
    let peer = Scripted(Arc::new(std::sync::Mutex::new(Script {
        entered: Some(Arc::clone(&entered)),
        connect_gate: status.is_none().then(|| Arc::new(Notify::new())),
        responses: status.into_iter().map(|status| wire(status, "")).collect(),
        ..Script::default()
    })));
    account.connections.transport = Some(Transport::offline(peer, 1, 0));
    let snapshot = |anthropic| {
        ControlToShell::Connections(ConnectionsView {
            anthropic,
            anthropic_revision: Some(1),
            claude_code: ClaudeCodeView::NotFound,
        })
    };
    store.0.lock().expect("fake").denied_read = Some(current);
    account.connection_event(ConnectionEvent::Op(ConnectionOp::ReconcileAnthropicKey {
        request: 2,
    }));
    let unreadable = next(&mut account).await;
    assert_eq!(
        account.handle(unreadable),
        [
            snapshot(KeyView::Unconfirmed),
            ControlToShell::AnthropicKeyReconciled {
                request: 2,
                read: None
            },
        ]
    );
    assert!(account.connections.read.is_none());

    store.0.lock().expect("fake").denied_read = None;
    account.connection_event(ConnectionEvent::Op(ConnectionOp::ReconcileAnthropicKey {
        request: 3,
    }));
    let loaded = next(&mut account).await;
    let local = account.handle(loaded);
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("fake catalog entered");
    if status.is_none() {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(15)).await;
    }
    let checked = next(&mut account).await;
    let remote = account.handle(checked);
    if status.is_none() {
        tokio::time::resume();
    }
    assert_eq!(account.connections.view.anthropic, KeyView::Unconfirmed);
    assert_eq!(
        local,
        [
            snapshot(KeyView::Checking),
            ControlToShell::AnthropicKeyReconciled {
                request: 3,
                read: Some(KeyRecoveryRead::Present {
                    revision: 1,
                    rejection: None
                }),
            },
        ],
        "a healthy local retry resolves recovery before the provider does"
    );
    assert_eq!(remote, [snapshot(KeyView::Unconfirmed)]);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn a_recovery_check_reads_after_adoption_and_replies_only_on_its_link() {
    let scratch = Scratch::new("key-reconcile");
    let (mut account, store, _) = keyed(&scratch).await;
    let replacement = "sk-ant-api123456";
    let plan = begin(&mut account, 1, 16).await;
    plant(&store, plan, replacement);
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    let snapshot = |anthropic, anthropic_revision| {
        ControlToShell::Connections(ConnectionsView {
            anthropic,
            anthropic_revision,
            claude_code: ClaudeCodeView::NotFound,
        })
    };
    assert_eq!(
        account.connection_event(ConnectionEvent::Op(ConnectionOp::ReconcileAnthropicKey {
            request: 2
        })),
        [snapshot(KeyView::Checking, None)]
    );
    account.connections.stage_session(None);
    account.connections.stage_session(Some(2));
    let adopted = next(&mut account).await;
    assert!(matches!(
        adopted,
        Event::Connection(ConnectionEvent::StageDone { result: Ok(_), .. })
    ));
    assert_eq!(account.handle(adopted), vec![]);
    let loaded = next(&mut account).await;
    assert_eq!(
        account.handle(loaded),
        [snapshot(KeyView::Checking, Some(2))]
    );
    let checked = next(&mut account).await;
    assert_eq!(account.handle(checked), [snapshot(KeyView::Ready, Some(2))]);
    assert_eq!(retained_key(&account.connections), Some(replacement));

    assert_eq!(
        account.connection_event(ConnectionEvent::Op(ConnectionOp::ReconcileAnthropicKey {
            request: 3
        })),
        [snapshot(KeyView::Checking, None)]
    );
    let loaded = next(&mut account).await;
    assert_eq!(
        account.handle(loaded),
        [
            snapshot(KeyView::Checking, Some(2)),
            ControlToShell::AnthropicKeyReconciled {
                request: 3,
                read: Some(KeyRecoveryRead::Present {
                    revision: 2,
                    rejection: None
                }),
            },
        ]
    );
    let checked = next(&mut account).await;
    assert_eq!(account.handle(checked), [snapshot(KeyView::Ready, Some(2))]);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn removal_confirmed_on_the_next_link_cannot_delete_an_earlier_links_adoption() {
    let scratch = Scratch::new("stage-runtime-stale-removal");
    let store = FakeStore::default();
    let lock = scratch.db().with_file_name("key-test.lock");
    save_key(&store, &lock);
    let (entered, at_gate) = std::sync::mpsc::channel();
    let (resume, held) = std::sync::mpsc::channel();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut held_store = Some(HeldStore {
        armed: Arc::clone(&armed),
        store: store.clone(),
        entered,
        resume: std::sync::Mutex::new(Some(held)),
    });
    let worker = Worker::new(
        Box::new(move || {
            Vault::with_service(held_store.take().expect("one open"), "key-test", &lock)
        }),
        Arc::new(FixedClock(0)),
    );
    let mut account = with_transport(
        Accounts::spawn_unconfigured(worker, test_support::net()).expect("worker"),
        peer(EMPTY_CATALOG),
        8,
    );
    settle(&mut account).await;
    account.connections.stage_session(Some(1));
    let replacement = "sk-ant-api123456";
    let plan = begin(&mut account, 1, 16).await;
    plant(&store, plan, replacement);
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id });
    tokio::task::spawn_blocking(move || {
        at_gate
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("adoption held");
    })
    .await
    .expect("gate joined");

    account.connections.stage_session(None);
    account.connections.stage_session(Some(2));
    let ControlToShell::Connections(ConnectionsView {
        anthropic: KeyView::Checking,
        anthropic_revision: Some(revision),
        ..
    }) = account.connections.connected()
    else {
        panic!("the next link sees adoption in flight and its old revision");
    };
    let remove = ConnectionOp::RemoveAnthropicKey {
        revision,
        request: 1,
    };
    account.connection_event(ConnectionEvent::Op(remove));
    resume.send(()).expect("release adoption");
    let adopted = next(&mut account).await;
    assert!(matches!(
        adopted,
        Event::Connection(ConnectionEvent::StageDone {
            epoch: 1,
            result: Ok(_),
            ..
        })
    ));
    assert_eq!(
        account.handle(adopted),
        vec![],
        "the old link's acknowledgement is lost"
    );
    assert_eq!(account.connections.view.anthropic, KeyView::Checking);
    for retry in [false, true] {
        if retry {
            account.connection_event(ConnectionEvent::Op(remove));
        }
        settle(&mut account).await;
        assert_eq!(account.connections.view.anthropic, KeyView::Ready);
        assert_eq!(
            account.connections.view.anthropic_revision,
            Some(revision + 1)
        );
        assert_eq!(retained_key(&account.connections), Some(replacement));
    }
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
    account.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 3,
        request: 1,
    }));
    assert_eq!(
        account.connection_event(ConnectionEvent::StageDone {
            epoch: 1,
            generation,
            plan: plan.id,
            result: Ok((1, key()))
        }),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Ok(())
        }]
    );
    settle(&mut account).await;
    assert_eq!(account.connections.view.anthropic, KeyView::Missing);
    assert!(account.connections.key.is_none());
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}
