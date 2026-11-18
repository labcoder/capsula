use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, Render, TestAppContext, VisualTestContext,
    WindowOptions,
};
use kumochi_protocol::message::{Incarnation, KeyStageId, KeyStagePlan};

use super::*;
use crate::link::Session;
use crate::presence::test_fixture;

const SYNTHETIC: &str = "sk-ant-api-only-for-the-test-clipboard";

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
        view.tab = 1;
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
fn clipboard_requires_one_text_entry_and_never_uses_metadata() {
    let item = || ClipboardItem::new_string_with_metadata(SYNTHETIC.into(), "not key text".into());
    assert_eq!(
        clipboard_text(Some(item())).as_deref().map(String::as_str),
        Some(SYNTHETIC)
    );
    let mut ambiguous = item();
    ambiguous.entries.extend(item().into_entries());
    assert!(clipboard_text(Some(ambiguous)).is_none());
    assert!(
        clipboard_text(Some(ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(
                gpui_kit::ExternalPaths::default()
            )],
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
fn production_accounts_does_not_render_the_dormant_row(cx: &mut TestAppContext) {
    let (window, _) = accounts(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds("anthropic-paste").is_none());
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
                request: 1,
                len: u16::try_from(SYNTHETIC.len()).expect("bounded"),
            }
        ))
    );
    assert_eq!(visual.read_from_clipboard(), Some(item));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

#[gpui::test]
fn a_fresh_unconfirmed_view_warns_that_paste_can_replace_a_key(cx: &mut TestAppContext) {
    let (window, owner, _) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let warning = "Pasting replaces any saved key before Anthropic checks the new one.";
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds(warning).is_none());
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
    assert!(visual.debug_bounds(warning).is_some());
}

#[gpui::test]
fn a_lost_save_acknowledgement_replaces_the_old_status(cx: &mut TestAppContext) {
    let (window, owner, _) = isolated_row(cx);
    let mut visual = VisualTestContext::from_window(window, cx);
    let unknown = "anthropic-status:Couldn’t confirm the save. Check the key before pasting again.";
    for (old, old_status) in [
        (KeyView::Missing, "anthropic-status:No key saved"),
        (
            KeyView::Ready,
            "anthropic-status:Anthropic accepted the key",
        ),
    ] {
        visual.update(|_, cx| {
            owner.update(cx, |owner, cx| {
                owner.settings.update(cx, |state, cx| {
                    let key = &mut state.key_paste;
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
                    cx.notify();
                });
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(visual.debug_bounds(unknown).is_some());
        assert!(visual.debug_bounds(old_status).is_none());
        assert!(visual.debug_bounds("Key saved").is_none());
        assert!(visual.debug_bounds("anthropic-check").is_some());
        assert!(visual.debug_bounds("anthropic-remove").is_none());
    }
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
                state
                    .key_paste
                    .receive(KeyView::Rejected(KeyRejection::Refused), Some(2));
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
        Ok(ControlToBrain::Connection(ConnectionOp::RemoveAnthropicKey { revision: 2 }))
    );
}
