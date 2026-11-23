use std::num::NonZeroU64;

use async_channel::TryRecvError;
use kumochi_protocol::geometry::PhysicalRect;
use kumochi_protocol::message::{
    ConnectionKind, Effort, Incarnation, ModelChoice, ModelSlug, TypedScreenRecipient,
};

use super::*;

#[test]
fn a_draft_change_cancels_a_late_offer_without_sending_the_draft() {
    let (mut link, session, data) = LinkHandle::record_typed_screen();
    let control = link.record_controls_bounded();
    let mut runs = Runs::default();
    runs.met(Incarnation(1));
    runs.connected(1);
    let thread = Some(ThreadId(1));
    runs.settings(ThreadId(1));
    let recipient = TypedScreenRevision(NonZeroU64::MIN);
    runs.typed_screen_recipient_displayed(&LinkEvent::TypedScreenRecipient {
        generation: 1,
        recipient: Some(TypedScreenRecipient {
            revision: recipient,
            choice: ModelChoice {
                connection: ConnectionKind::ChatGpt,
                model: ModelSlug::new("fixture-model").unwrap(),
                effort: Effort::Low,
            },
        }),
    });
    let display = TypedDisplay {
        id: NonZeroU64::MIN,
        bounds: PhysicalRect {
            origin: [-100, 0],
            size: [100, 100],
        },
        dpi: 96,
    };
    let draft = runs.typed_draft();
    assert!(runs.request_typed_screen(&link, &session, display));
    assert_eq!(
        control.try_recv(),
        Ok(ControlToBrain::RequestTypedScreenOffer {
            turn: 1,
            thread,
            draft,
            recipient,
            display: Box::new(display),
        })
    );
    runs.typed_draft_changed();
    let offer = TypedScreenOffer(NonZeroU64::MIN);
    assert!(!runs.typed_screen_offered(
        &link,
        &session,
        &LinkEvent::TypedScreenOffered {
            generation: 1,
            turn: 1,
            thread,
            draft,
            recipient,
            offer: Some(offer),
        },
    ));
    assert_eq!(
        control.try_recv(),
        Ok(ControlToBrain::CancelTypedScreen { turn: 1, offer })
    );
    assert_eq!(data.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(runs.thread(), thread);
}
