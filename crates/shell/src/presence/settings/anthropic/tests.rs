use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, Render, TestAppContext, VisualTestContext,
    WindowOptions,
};
use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionsView, Incarnation, KeyRecoveryRead, KeyRemovalFailure, KeyStageId,
    KeyStagePlan,
};

use super::super::SettingsTab;
use super::*;
use crate::link::{Batch, LinkEvent, LinkState, Session};
use crate::presence::test_fixture;

const SYNTHETIC: &str = "sk-ant-api-only-for-the-test-clipboard";

const REPLACE_WARNING: &str = "Pasting replaces any saved key before Anthropic checks the new one.";

struct Row(Entity<SettingsView>);

impl Render for Row {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.0
            .update(cx, |view, cx| view.anthropic(cx).into_any_element())
    }
}

fn accounts(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<SettingsView>) {
    let (window, view) = super::super::general::tests::open(cx);
    view.update(cx, |view, cx| {
        view.tab = SettingsTab::Accounts;
        view.model.update(cx, |state, cx| {
            state.key_paste.receive(KeyView::Missing, None);
            cx.notify();
        });
    });
    (window, view)
}

fn isolated_row(
    cx: &mut TestAppContext,
) -> (
    AnyWindowHandle,
    Entity<Presence>,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let (_, view) = accounts(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let (window, _) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Row(view)))
            .expect("test window")
    });
    (window, owner, controls)
}

fn click(visual: &mut VisualTestContext, selector: &'static str) {
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = visual.debug_bounds(selector).expect("visible control");
    visual.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    visual.run_until_parked();
}

#[test]
fn clipboard_takes_text_without_using_other_entries_or_metadata() {
    let item = || ClipboardItem::new_string_with_metadata(SYNTHETIC.into(), "not key text".into());
    assert_eq!(
        clipboard_text(Some(item())).as_deref().map(String::as_str),
        Some(SYNTHETIC)
    );
    let paths = || ClipboardEntry::ExternalPaths(gpui_kit::ExternalPaths::default());
    let mut mixed = item();
    mixed.entries.insert(0, paths());
    assert_eq!(
        clipboard_text(Some(mixed)).as_deref().map(String::as_str),
        Some(SYNTHETIC)
    );
    assert!(
        clipboard_text(Some(ClipboardItem {
            entries: vec![paths()],
        }))
        .is_none()
    );
    assert!(
        clipboard_text(Some(ClipboardItem {
            entries: Vec::new()
        }))
        .is_none()
    );
}

#[gpui::test]
fn production_accounts_renders_the_key_row(cx: &mut TestAppContext) {
    let (window, _) = accounts(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds("anthropic-paste").is_some());
}

#[gpui::test]
fn the_row_pastes_only_from_the_test_clipboard_and_never_writes_it(cx: &mut TestAppContext) {
    let (window, _owner, controls) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    click(&mut visual, "anthropic-paste");
    assert!(visual.debug_bounds("anthropic-remove").is_none());
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    let item = ClipboardItem::new_string(SYNTHETIC.into());
    visual.write_to_clipboard(item.clone());
    click(&mut visual, "anthropic-paste");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::BeginAnthropicKeyStage {
                request: 2,
                len: u16::try_from(SYNTHETIC.len()).expect("bounded"),
            }
        ))
    );
    assert_eq!(visual.read_from_clipboard(), Some(item));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn a_reconnecting_shell_warns_before_paste_from_a_cached_missing_view(cx: &mut TestAppContext) {
    let (window, owner, _) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    key_event(
        &mut visual,
        &owner,
        LinkEvent::State(LinkState::Reconnecting),
    );
    key_event(
        &mut visual,
        &owner,
        LinkEvent::Connected(Session::detached(8, Incarnation(1))),
    );
    key_event(
        &mut visual,
        &owner,
        LinkEvent::Connections {
            generation: 8,
            view: ConnectionsView {
                anthropic: KeyView::Missing,
                anthropic_revision: None,
                claude_code: ClaudeCodeView::NotFound,
            },
        },
    );
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(owner.read(cx).settings.read(cx).key_paste.can_paste());
    });
    assert!(
        visual
            .debug_bounds("anthropic-status:No key saved")
            .is_some()
    );
    assert!(visual.debug_bounds(REPLACE_WARNING).is_some());
    assert!(visual.debug_bounds("Key saved").is_none());
}

#[gpui::test]
fn a_fresh_unconfirmed_view_warns_that_paste_can_replace_a_key(cx: &mut TestAppContext) {
    let (window, owner, _) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds(REPLACE_WARNING).is_some());
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                state.key_paste = KeyPaste::default();
                state.key_paste.receive(KeyView::Unconfirmed, None);
                assert!(!state.key_paste.saved());
                assert!(state.key_paste.can_paste());
                cx.notify();
            });
        });
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds(REPLACE_WARNING).is_some());
}

fn expire_staged_save(key: &mut KeyPaste, old: KeyView) {
    *key = KeyPaste::default();
    key.receive(old, (old == KeyView::Ready).then_some(1));
    let now = Instant::now();
    let at = key
        .begin(Some(Zeroizing::new(SYNTHETIC.into())), 7, now, |_| true)
        .expect("begin");
    let id = KeyStageId([4; 16]);
    let write = key
        .planned(
            7,
            &ControlToShell::AnthropicKeyStage {
                request: at.request,
                result: Ok(KeyStagePlan {
                    id,
                    len: u16::try_from(SYNTHETIC.len()).expect("bounded"),
                }),
            },
            now,
        )
        .expect("stage plan")
        .simulate_write(now);
    key.written(&write, now, |op| {
        assert_eq!(op, ConnectionOp::AnthropicKeyStaged { plan: id });
        true
    });
    assert!(key.expire(at, at.deadline));
}

fn receive_key(
    visual: &mut VisualTestContext,
    owner: &Entity<Presence>,
    view: KeyView,
    revision: Option<u64>,
) {
    key_event(
        visual,
        owner,
        LinkEvent::Connections {
            generation: 7,
            view: ConnectionsView {
                anthropic: view,
                anthropic_revision: revision,
                claude_code: ClaudeCodeView::NotFound,
            },
        },
    );
}

fn key_event(visual: &mut VisualTestContext, owner: &Entity<Presence>, event: LinkEvent) {
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner
                .link_events(Batch::Plain(vec![event]), cx)
                .expect("link event");
        });
    });
}

const UNKNOWN_SAVE: &str =
    "anthropic-status:Couldn't confirm the save. Check the key before pasting again.";

fn assert_unknown_save(visual: &mut VisualTestContext, owner: &Entity<Presence>) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(!owner.read(cx).settings.read(cx).key_paste.can_paste());
    });
    assert!(
        visual.debug_bounds(UNKNOWN_SAVE).is_some(),
        "an old snapshot is not the Check result"
    );
    for stale in [
        "anthropic-status:No key saved",
        "anthropic-status:Checking key",
        "anthropic-status:Anthropic accepted the key",
        "Key saved",
        "anthropic-remove",
        REPLACE_WARNING,
    ] {
        assert!(visual.debug_bounds(stale).is_none(), "{stale}");
    }
}

fn click_recovery_check(
    visual: &mut VisualTestContext,
    controls: &async_channel::Receiver<ControlToBrain>,
    request: u64,
) {
    click(visual, "anthropic-check");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::ReconcileAnthropicKey { request }
        ))
    );
}

#[gpui::test]
fn a_lost_save_acknowledgement_replaces_the_old_status(cx: &mut TestAppContext) {
    let (window, owner, controls) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    for (old, reconciled, status) in [
        (
            KeyView::Missing,
            KeyView::Missing,
            "anthropic-status:No key saved",
        ),
        (
            KeyView::Ready,
            KeyView::Ready,
            "anthropic-status:Anthropic accepted the key",
        ),
    ] {
        visual.update(|_, cx| {
            owner.update(cx, |owner, cx| {
                owner.settings.update(cx, |state, cx| {
                    expire_staged_save(&mut state.key_paste, old);
                    cx.notify();
                });
            });
        });
        assert_unknown_save(&mut visual, &owner);

        click_recovery_check(&mut visual, &controls, 2);
        assert_unknown_save(&mut visual, &owner);
        assert!(visual.debug_bounds("anthropic-check").is_none());
        for (view, revision) in [
            (KeyView::Checking, None),
            (KeyView::Missing, None),
            (KeyView::Ready, Some(1)),
        ] {
            receive_key(&mut visual, &owner, view, revision);
            assert_unknown_save(&mut visual, &owner);
        }

        receive_key(&mut visual, &owner, KeyView::Unconfirmed, Some(1));
        let failed = ControlToShell::AnthropicKeyReconciled {
            request: 2,
            read: None,
        };
        key_event(
            &mut visual,
            &owner,
            LinkEvent::KeyStage {
                generation: 7,
                reply: failed,
            },
        );
        assert_unknown_save(&mut visual, &owner);
        click_recovery_check(&mut visual, &controls, 3);

        let (local, revision, read) = if reconciled == KeyView::Ready {
            (
                KeyView::Checking,
                Some(2),
                KeyRecoveryRead::Present {
                    revision: 2,
                    rejection: None,
                },
            )
        } else {
            (KeyView::Missing, None, KeyRecoveryRead::Missing)
        };
        receive_key(&mut visual, &owner, local, revision);
        let done = ControlToShell::AnthropicKeyReconciled {
            request: 3,
            read: Some(read),
        };
        key_event(
            &mut visual,
            &owner,
            LinkEvent::KeyStage {
                generation: 6,
                reply: done.clone(),
            },
        );
        assert_unknown_save(&mut visual, &owner);
        key_event(
            &mut visual,
            &owner,
            LinkEvent::KeyStage {
                generation: 7,
                reply: done,
            },
        );
        if local == KeyView::Checking {
            receive_key(&mut visual, &owner, KeyView::Ready, revision);
        }
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(owner.read(cx).settings.read(cx).key_paste.can_paste());
        });
        assert!(visual.debug_bounds(UNKNOWN_SAVE).is_none());
        assert!(visual.debug_bounds(status).is_some());
        assert_eq!(
            visual.debug_bounds("anthropic-remove").is_some(),
            reconciled == KeyView::Ready
        );
    }
}

#[gpui::test]
fn provider_uncertainty_does_not_block_managing_a_locally_read_key(cx: &mut TestAppContext) {
    let (window, owner, controls) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                expire_staged_save(&mut state.key_paste, KeyView::Ready);
                cx.notify();
            });
        });
    });
    click_recovery_check(&mut visual, &controls, 2);
    receive_key(&mut visual, &owner, KeyView::Unconfirmed, Some(2));
    key_event(
        &mut visual,
        &owner,
        LinkEvent::KeyStage {
            generation: 7,
            reply: ControlToShell::AnthropicKeyReconciled {
                request: 2,
                read: None,
            },
        },
    );
    assert_unknown_save(&mut visual, &owner);
    click_recovery_check(&mut visual, &controls, 3);
    receive_key(&mut visual, &owner, KeyView::Checking, Some(2));
    assert_unknown_save(&mut visual, &owner);
    key_event(
        &mut visual,
        &owner,
        LinkEvent::KeyStage {
            generation: 7,
            reply: ControlToShell::AnthropicKeyReconciled {
                request: 3,
                read: Some(KeyRecoveryRead::Present {
                    revision: 2,
                    rejection: None,
                }),
            },
        },
    );
    for (view, status) in [
        (KeyView::Checking, "anthropic-status:Checking key"),
        (
            KeyView::Unconfirmed,
            "anthropic-status:Couldn't confirm the key. Try again.",
        ),
    ] {
        receive_key(&mut visual, &owner, view, Some(2));
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                owner.read(cx).settings.read(cx).key_paste.can_paste(),
                "provider failure must not lock a locally read key"
            );
        });
        assert!(visual.debug_bounds(status).is_some());
        assert!(visual.debug_bounds(UNKNOWN_SAVE).is_none());
        assert!(visual.debug_bounds(REPLACE_WARNING).is_some());
        assert!(visual.debug_bounds("anthropic-remove").is_some());
        assert!(visual.debug_bounds("Key saved").is_none());
        assert!(
            visual
                .debug_bounds("anthropic-status:Anthropic accepted the key")
                .is_none()
        );
    }
    click(&mut visual, "anthropic-remove");
    click(&mut visual, "confirmation-danger");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::RemoveAnthropicKey {
                revision: 2,
                request: 4
            }
        ))
    );
}

#[gpui::test]
fn a_stale_or_cancelled_confirmation_cannot_remove_a_key(cx: &mut TestAppContext) {
    let (window, owner, controls) = isolated_row(cx);
    owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |state, cx| {
            state.key_paste.receive(KeyView::Ready, Some(2));
            cx.notify();
        });
    });
    let mut visual = VisualTestContext::from_window(window, cx);
    click(&mut visual, "anthropic-remove");
    click(&mut visual, "confirmation-cancel");
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    click(&mut visual, "anthropic-remove");
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                state.key_paste.receive(KeyView::Ready, Some(3));
                cx.notify();
            });
        });
    });
    click(&mut visual, "confirmation-danger");
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    click(&mut visual, "anthropic-remove");
    click(&mut visual, "confirmation-danger");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::RemoveAnthropicKey {
                revision: 3,
                request: 1
            }
        ))
    );
}

#[gpui::test]
fn a_catalog_completion_does_not_stale_an_open_remove_confirmation(cx: &mut TestAppContext) {
    for view in [KeyView::Unconfirmed, KeyView::Ready] {
        let (window, owner, controls) = isolated_row(cx);
        let mut visual = VisualTestContext::from_window(window, cx);
        visual.update(|_, cx| {
            owner.update(cx, |owner, cx| {
                owner.settings.update(cx, |state, cx| {
                    expire_staged_save(&mut state.key_paste, KeyView::Ready);
                    cx.notify();
                });
            });
        });
        click_recovery_check(&mut visual, &controls, 2);
        receive_key(&mut visual, &owner, KeyView::Checking, Some(2));
        key_event(
            &mut visual,
            &owner,
            LinkEvent::KeyStage {
                generation: 7,
                reply: ControlToShell::AnthropicKeyReconciled {
                    request: 2,
                    read: Some(KeyRecoveryRead::Present {
                        revision: 2,
                        rejection: None,
                    }),
                },
            },
        );
        click(&mut visual, "anthropic-remove");
        receive_key(&mut visual, &owner, view, Some(2));
        click(&mut visual, "confirmation-danger");
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::Connection(
                ConnectionOp::RemoveAnthropicKey {
                    revision: 2,
                    request: 3
                }
            ))
        );
    }
}

#[gpui::test]
fn removing_stays_visible_until_its_own_receipt_arrives(cx: &mut TestAppContext) {
    let (window, owner, controls) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                state.key_paste.receive(KeyView::Ready, Some(2));
                cx.notify();
            });
        });
    });
    click(&mut visual, "anthropic-remove");
    click(&mut visual, "confirmation-danger");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::RemoveAnthropicKey {
                revision: 2,
                request: 1
            }
        ))
    );
    let failed = ControlToShell::AnthropicKeyRemoved {
        request: 1,
        result: Err(KeyRemovalFailure::Unconfirmed),
    };
    visual.update(|_, cx| {
        owner.update(cx, |owner, cx| {
            owner.settings.update(cx, |state, cx| {
                state.key_paste.receive(KeyView::Unconfirmed, Some(2));
                cx.notify();
            });
            owner.key_reply(6, &failed, cx);
        });
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        visual
            .debug_bounds("anthropic-status:Removing key")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("anthropic-status:Removal not confirmed — retry")
            .is_none()
    );
    assert!(visual.debug_bounds("anthropic-check").is_none());

    visual.update(|_, cx| owner.update(cx, |owner, cx| owner.key_reply(7, &failed, cx)));
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        visual
            .debug_bounds("anthropic-status:Removing key")
            .is_none()
    );
    assert!(
        visual
            .debug_bounds("anthropic-status:Removal not confirmed — retry")
            .is_some()
    );
    click(&mut visual, "anthropic-check");
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::Connection(
            ConnectionOp::RemoveAnthropicKey {
                revision: 2,
                request: 2
            }
        ))
    );
}

#[gpui::test]
fn the_picker_offers_a_checked_key_catalog_and_sends_its_choice(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{
        ConnectionKind, ControlToBrain, DataToShell, Effort, ModelChoice, ModelInfo, ModelName,
        ModelSlug, SettingsOp,
    };
    let owner = test_fixture::presence(cx);
    let (window, view) = super::super::general::tests::open(cx);
    let controls = view.update(cx, |view, cx| {
        view.owner = owner.downgrade();
        owner.update(cx, |owner, _| {
            owner.settings = view.model.clone();
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner.link.record_controls()
        })
    });
    let info = |slug: &str| ModelInfo {
        slug: ModelSlug::new(slug).expect("slug"),
        display_name: ModelName::new(slug).expect("name"),
        supported_efforts: None,
    };
    for (connection, slug) in [
        (ConnectionKind::ChatGpt, "m-0"),
        (ConnectionKind::AnthropicKey, "claude-haiku-5-5"),
        (ConnectionKind::ClaudeCode, "c-0"),
    ] {
        let event = LinkEvent::Data {
            generation: 7,
            message: DataToShell::Models {
                connection,
                models: vec![info(slug)],
            },
            bytes: 0,
        };
        owner
            .update(cx, |owner, cx| {
                owner.link_events(Batch::Plain(vec![event]), cx)
            })
            .expect("the presence takes the frame");
    }
    let offered: Vec<_> = view.update(cx, |view, cx| {
        view.model
            .read(cx)
            .model_choices()
            .map(|(connection, model)| (connection, model.slug.as_str().to_owned()))
            .collect()
    });
    assert_eq!(
        offered,
        [
            (ConnectionKind::ChatGpt, "m-0".to_owned()),
            (ConnectionKind::AnthropicKey, "claude-haiku-5-5".to_owned()),
            (ConnectionKind::ClaudeCode, "c-0".to_owned()),
        ]
    );
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(768.), gpui_kit::px(1600.)));
    click(&mut visual, "default-model-1");
    let sent: Vec<_> = std::iter::from_fn(|| controls.try_recv().ok()).collect();
    assert_eq!(
        sent,
        [ControlToBrain::Settings(SettingsOp::SetDefaultModel(
            ModelChoice {
                model: ModelSlug::new("claude-haiku-5-5").expect("slug"),
                effort: Effort::Low,
                connection: ConnectionKind::AnthropicKey,
            }
        ))]
    );
}
