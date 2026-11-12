//! Carries keyboard gestures from the native Raw Input callback into the presence owner.
//!
//! The callback closes collection authority before deferred UI work. Effects then reach the
//! owner through `update_window`, which only tries to borrow the app. If GPUI is already
//! borrowed, because a nested message loop dispatched this callback, effects wait in order
//! with their original times and are retried until they apply or the owner is gone, instead of
//! panicking or being lost.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{AnyWindowHandle, App, AppContext as _, AsyncApp, WeakEntity};

use super::{Presence, voice::Stamp};
use crate::input::{
    Edge, Event, Gesture, Key,
    hold::{Delivery, Fence, InputReason, InputStage, Intent},
};
use crate::link::LinkHandle;
use crate::platform::{Deliver, EventTime, performance_counter};
use kumochi_protocol::message::{GestureId, ThreadId};

struct Pending {
    delivery: Delivery,
    at: EventTime,
    stamp: Stamp,
    qpc: u64,
    cancelled: Option<GestureId>,
    escape: bool,
    collection: u64,
}

/// How long a busy app is waited for between retries.
const RETRY: Duration = Duration::from_millis(1);

enum Applied {
    Yes,
    Busy,
    Gone,
}

/// Builds the native callback. `dock` is always open, so it anchors the app borrow.
#[expect(
    clippy::too_many_lines,
    reason = "native revocation and ordered deferred delivery share one owner"
)]
pub(super) fn deliver(
    gesture: Rc<Cell<Gesture>>,
    dock: AnyWindowHandle,
    owner: WeakEntity<Presence>,
    link: LinkHandle,
    target: Rc<Cell<(Option<ThreadId>, u64)>>,
    cx: &mut App,
) -> Deliver {
    let (deferred, pending) = async_channel::unbounded::<Pending>();
    // Queued plus in flight, so a newer effect never overtakes one still being retried.
    let backlog = Rc::new(Cell::new(0_usize));
    let waiting = (owner.clone(), Rc::clone(&backlog));
    cx.spawn(async move |cx| {
        let (owner, backlog) = waiting;
        while let Ok(pending) = pending.recv().await {
            loop {
                match apply(cx, dock, &owner, &pending) {
                    Applied::Yes => break,
                    Applied::Gone => {
                        tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::OwnerGone, "held speech input boundary");
                        return;
                    }
                    Applied::Busy => cx.background_executor().timer(RETRY).await,
                }
            }
            backlog.set(backlog.get().saturating_sub(1));
        }
    })
    .detach();
    let async_cx = cx.to_async();
    let fence = std::cell::RefCell::new(Fence::new(link.microphone_gate()));
    let escaped = Cell::new(None);
    let escape_down = Cell::new(false);
    Box::new(move |event, at| {
        let Ok(qpc) = u64::try_from(performance_counter()) else {
            tracing::warn!(stage = ?InputStage::Delivery, reason = ?InputReason::ClockUnavailable, "held speech input failed");
            link.cancel_input();
            let mut state = gesture.get();
            let _cancelled = fence
                .borrow_mut()
                .reduce(&mut state, Event::Reset, at.instant);
            gesture.set(state);
            return None;
        };
        if event == Event::Reset {
            escaped.set(None);
            escape_down.set(false);
        }
        if let Event::Key {
            key: Key::Other(1),
            edge,
        } = event
        {
            if edge == Edge::Down && !escape_down.replace(true) {
                escaped.set(link.cancel_input_gesture());
            } else if edge == Edge::Up {
                escape_down.set(false);
                escaped.set(None);
            }
        }
        let escape = event == Event::Escape;
        let mut cancelled = if escape {
            escaped.take().or_else(|| link.cancel_input_gesture())
        } else {
            None
        };
        let mut state = gesture.get();
        let delivery = fence.borrow_mut().reduce(&mut state, event, at.instant);
        gesture.set(state);
        for intent in delivery.intents.iter().flatten() {
            match intent {
                Intent::Recognized {
                    gesture,
                    generation: Ok(_),
                    ..
                } => {
                    link.cancel_input();
                    link.track_input(*gesture);
                }
                Intent::End {
                    cancelled: true, ..
                } => {
                    link.cancel_input();
                }
                _ => {}
            }
        }
        if delivery
            .effects
            .iter()
            .any(|e| matches!(e, Some(crate::input::Effect::OpenInput)))
        {
            cancelled = link.cancel_input_gesture();
        }
        if delivery.effects.iter().all(Option::is_none)
            && delivery.intents.iter().all(Option::is_none)
            && !escape
        {
            return state.deadline();
        }
        let pending = Pending {
            delivery,
            at,
            qpc,
            cancelled,
            escape,
            collection: link.microphone_gate().latest(),
            stamp: Stamp {
                target: target.get(),
                session: link.link_generation(),
                choice: link.choice_revision(),
            },
        };
        if backlog.get() == 0
            && matches!(
                apply(&mut async_cx.clone(), dock, &owner, &pending),
                Applied::Yes
            )
        {
            return state.deadline();
        }
        backlog.set(backlog.get() + 1);
        tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::Deferred, "held speech input boundary");
        if deferred.try_send(pending).is_err() {
            link.cancel_input();
            backlog.set(backlog.get() - 1);
            tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::QueueClosed, "held speech input boundary");
        }
        state.deadline()
    })
}

/// Applies effects through a fallible borrow of the app.
fn apply(
    cx: &mut AsyncApp,
    dock: AnyWindowHandle,
    owner: &WeakEntity<Presence>,
    pending: &Pending,
) -> Applied {
    let result = cx.update_window(dock, |_, window, app| {
        owner
            .update(app, |this, cx| {
                let mut effects = pending.delivery.effects;
                if effects.contains(&Some(crate::input::Effect::OpenInput)) {
                    if this.link.microphone_gate().latest() == pending.collection {
                        if let Some(gesture) = pending.cancelled {
                            this.cancel_voice_gesture(gesture, cx);
                        }
                    } else {
                        effects = effects.map(|effect| {
                            effect.filter(|effect| *effect != crate::input::Effect::OpenInput)
                        });
                    }
                }
                this.apply_effects(effects, pending.at, window, cx);
                for intent in pending.delivery.intents.iter().flatten().cloned() {
                    this.voice_intent(intent, pending.stamp, pending.qpc, cx);
                }
                if pending.escape && this.native_escape(pending.cancelled, cx).is_err() {
                    tracing::error!("can't apply native Escape");
                }
            })
            .is_ok()
    });
    match result {
        Ok(true) => Applied::Yes,
        Ok(false) => Applied::Gone,
        Err(_) if owner.upgrade().is_none() => Applied::Gone,
        Err(_) => Applied::Busy,
    }
}
