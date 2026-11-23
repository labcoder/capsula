use super::*;
#[cfg(windows)]
use kumochi_protocol::message::ControlToBrain;
use kumochi_protocol::message::KeyRejection;
#[cfg(windows)]
use tokio::io::AsyncWriteExt as _;

#[tokio::test]
async fn remove_during_begin_never_forwards_the_discarded_plan_or_leaves_staged_names() {
    let scratch = Scratch::new("stage-remove-requested");
    let (mut account, store, _) = keyed(&scratch).await;
    assert_eq!(
        account.stage_op(
            1,
            ConnectionOp::BeginAnthropicKeyStage {
                request: 41,
                len: 16
            }
        ),
        vec![]
    );
    account.connection_event(ConnectionEvent::Op(ConnectionOp::RemoveAnthropicKey {
        revision: 1,
        request: 1,
    }));
    let planned = next(&mut account).await;
    let Event::Connection(ConnectionEvent::StagePlan {
        request: 41,
        result: Ok((1, KeyStagePlan { len: 16, id })),
        ..
    }) = &planned
    else {
        panic!("expected a blocked plan for the original revision");
    };
    let plan = *id;
    let removed = next(&mut account).await;
    assert!(matches!(
        removed,
        Event::Connection(ConnectionEvent::Loaded {
            result: (None, Ok(None)),
            ..
        })
    ));
    assert_eq!(
        store
            .names()
            .into_iter()
            .filter(|name| name.starts_with("key-test/gen/anthropic/"))
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );
    assert_eq!(
        account.handle(planned),
        [ControlToShell::AnthropicKeyStage {
            request: 41,
            result: Err(KeyStageFailure::Unavailable),
        }],
        "Remove must refuse the current Begin instead of forwarding its discarded plan"
    );
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan }),
        [ControlToShell::AnthropicKeyStageDone {
            plan,
            result: Err(KeyStageFailure::Changed),
        }]
    );
    account.handle(removed);
    assert_eq!(account.connections.stage_request, None);
    assert_eq!(account.connections.stage_admission, None);
    assert_eq!(account.connections.view.anthropic, KeyView::Missing);
    assert!(account.connections.key.is_none());
    assert_eq!(
        store
            .names()
            .into_iter()
            .filter(|name| name.starts_with("key-test/gen/anthropic/"))
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

async fn adopt_and_check_cleanup(account: &mut Accounts, store: &FakeStore, plan: KeyStagePlan) {
    assert_eq!(
        account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
        vec![]
    );
    let event = next(account).await;
    assert_eq!(
        account.handle(event),
        vec![ControlToShell::AnthropicKeyStageDone {
            plan: plan.id,
            result: Ok(())
        }]
    );
    settle(account).await;
    assert_eq!(retained_key(&account.connections), Some("sk-ant-api123456"));
    assert_eq!(account.connections.view.anthropic, KeyView::Ready);
    assert!(
        store
            .names()
            .iter()
            .all(|name| !name.contains(&generation_hex(&plan.id.0)))
    );
}

#[tokio::test]
async fn check_after_begin_reply_keeps_admission_and_deletes_staged_names() {
    let scratch = Scratch::new("stage-check-issued");
    let (mut account, store, _) = keyed(&scratch).await;
    let plan = begin(&mut account, 1, 16).await;
    plant(&store, plan, "sk-ant-api123456");
    account.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    settle(&mut account).await;
    adopt_and_check_cleanup(&mut account, &store, plan).await;
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn check_during_begin_still_answers_the_request_and_adopts_the_paste() {
    let scratch = Scratch::new("stage-check-requested");
    let (mut account, store, _) = keyed(&scratch).await;
    assert_eq!(
        account.stage_op(
            1,
            ConnectionOp::BeginAnthropicKeyStage {
                request: 1,
                len: 16
            }
        ),
        vec![]
    );
    account.connection_event(ConnectionEvent::Op(ConnectionOp::CheckAnthropicKey));
    let event = next(&mut account).await;
    let frames = account.handle(event);
    assert_eq!(frames.len(), 1, "Check must not discard a Begin reply");
    let [
        ControlToShell::AnthropicKeyStage {
            request: 1,
            result: Ok(plan),
        },
    ] = frames.as_slice()
    else {
        panic!("Check must not discard the Begin reply")
    };
    assert_eq!(plan.len, 16);
    plant(&store, *plan, "sk-ant-api123456");
    settle(&mut account).await;
    adopt_and_check_cleanup(&mut account, &store, *plan).await;
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[tokio::test]
async fn runtime_distinguishes_invalid_lengths_and_rejected_present_bytes() {
    let scratch = Scratch::new("stage-typed-errors");
    let (mut account, store, requests) = keyed(&scratch).await;
    for len in [0, 15, 2049, u16::MAX] {
        assert_eq!(
            account.stage_op(1, ConnectionOp::BeginAnthropicKeyStage { request: 7, len }),
            vec![]
        );
        let event = next(&mut account).await;
        assert_eq!(
            account.handle(event),
            vec![ControlToShell::AnthropicKeyStage {
                request: 7,
                result: Err(KeyStageFailure::InvalidLength)
            }]
        );
        ready_old(&account);
    }
    let text = "sk-ant-oat-synthetic-token";
    let plan = begin(&mut account, 8, u16::try_from(text.len()).expect("length")).await;
    plant(&store, plan, text);
    account.stage_op(1, ConnectionOp::AnthropicKeyStaged { plan: plan.id });
    let event = next(&mut account).await;
    assert_eq!(
        account.handle(event),
        [
            view_frame(KeyView::Checking, None),
            ControlToShell::AnthropicKeyStageDone {
                plan: plan.id,
                result: Err(KeyStageFailure::Rejected(KeyRejection::SubscriptionToken)),
            },
        ]
    );
    settle(&mut account).await;
    ready_old(&account);
    assert_eq!(starts(&requests), 2);
    account
        .close(tokio::time::Instant::now(), tokio::time::Instant::now())
        .await;
}

#[cfg(windows)]
async fn stage_frame(control: &mut tokio::io::DuplexStream) -> ControlToShell {
    timeout(Duration::from_secs(5), async {
        loop {
            match crate::ipc::read::<ControlToShell>(
                control,
                kumochi_protocol::frame::Pipe::Control,
            )
            .await
            .expect("frame")
            .expect("open")
            {
                reply @ (ControlToShell::AnthropicKeyStage { .. }
                | ControlToShell::AnthropicKeyStageDone { .. }) => return reply,
                ControlToShell::Ping => control
                    .write_all(
                        &kumochi_protocol::frame::encode(
                            &ControlToBrain::Pong,
                            kumochi_protocol::frame::Pipe::Control,
                        )
                        .expect("pong"),
                    )
                    .await
                    .expect("sent"),
                _ => {}
            }
        }
    })
    .await
    .expect("paste reply")
}

/// Requires native trust for Anthropic staging admission; the fake vault only supplies storage.
#[cfg(windows)]
#[tokio::test]
async fn serve_binds_staging_and_clears_it_before_the_next_link() {
    use kumochi_protocol::frame::{self, Pipe};
    let scratch = Scratch::new("stage-real-session");
    let mut brain = test_support::brain(&scratch, "");
    let mut tap = crate::tap::Tap::from_env();
    let (control, mut client) = tokio::io::duplex(65536);
    let (data, client_data) = tokio::io::duplex(65536);
    let session = async {
        client
            .write_all(
                &frame::encode(
                    &ControlToBrain::Connection(ConnectionOp::BeginAnthropicKeyStage {
                        request: 9,
                        len: 16,
                    }),
                    Pipe::Control,
                )
                .expect("begin"),
            )
            .await
            .expect("sent");
        let reply = stage_frame(&mut client).await;
        assert!(matches!(
            reply,
            ControlToShell::AnthropicKeyStage {
                request: 9,
                result: Ok(KeyStagePlan { len: 16, .. })
            }
        ));
        let ControlToShell::AnthropicKeyStage {
            request: 9,
            result: Ok(plan),
        } = reply
        else {
            panic!("serve must bind staging admission")
        };
        assert_eq!(plan.len, 16);
        drop((client, client_data));
        plan
    };
    let (ended, plan) = timeout(Duration::from_secs(5), async {
        tokio::join!(brain.serve(control, data, &mut tap), session)
    })
    .await
    .expect("first session ends");
    assert_eq!(ended, crate::runtime::Ended::Lost);
    assert_eq!(brain.account.connections.stage_link, None);
    assert_eq!(brain.account.connections.stage_request, None);
    assert_eq!(brain.account.connections.stage_admission, None);
    let (control, mut client) = tokio::io::duplex(65536);
    let (data, client_data) = tokio::io::duplex(65536);
    let replay = async {
        client
            .write_all(
                &frame::encode(
                    &ControlToBrain::Connection(ConnectionOp::AnthropicKeyStaged { plan: plan.id }),
                    Pipe::Control,
                )
                .expect("staged"),
            )
            .await
            .expect("sent");
        assert_eq!(
            stage_frame(&mut client).await,
            ControlToShell::AnthropicKeyStageDone {
                plan: plan.id,
                result: Err(KeyStageFailure::Changed)
            }
        );
        drop((client, client_data));
    };
    let (ended, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(brain.serve(control, data, &mut tap), replay)
    })
    .await
    .expect("second session ends");
    assert_eq!(ended, crate::runtime::Ended::Lost);
    brain.close().await;
}
