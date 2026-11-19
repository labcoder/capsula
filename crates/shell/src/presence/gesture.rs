//! Carries keyboard gestures from the native Raw Input callback into the presence owner.
//!
//! The callback closes collection authority before deferred UI work. Effects then reach the
//! owner through `update_window`, which only tries to borrow the app. If GPUI is already
//! borrowed, because a nested message loop dispatched this callback, effects wait in order
//! with their original times and are retried until they apply or the owner is gone, instead of
//! panicking or being lost. Intents run when the dock's update has ended, inside the same call:
//! GPUI takes a window out of its table while updating it, and a recognized hold's capture
//! lease must reach every presence window, the dock included.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui_kit::{AnyWindowHandle, App, AppContext as _, AsyncApp, WeakEntity};

use super::{
    Presence,
    voice::{Gates, HoldBack, Stamp},
};
use crate::input::{
    Edge, Event, Gesture, Key, TalkHeld,
    hold::{Delivery, Fence, InputReason, InputStage, Intent},
};
use crate::link::LinkHandle;
use crate::platform::{Deliver, EventTime, cursor_position, performance_counter};
use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{GestureId, ThreadId};

struct Pending {
    delivery: Delivery,
    stamp: Stamp,
    qpc: u64,
    /// Where the pointer was when this delivery recognized a hold or saw its keys come up.
    pointer: Option<PhysicalPoint>,
    input: ReleaseInput,
    cancelled: Option<GestureId>,
    /// An Esc press: every retirement waits until this delivery is decided.
    escape: Option<Rc<HoldBack>>,
    collection: u64,
}

#[derive(Clone)]
pub(super) struct ReleaseInput {
    current: Rc<Cell<(u64, u64)>>,
    stamp: (u64, u64),
    at: EventTime,
}

impl ReleaseInput {
    fn uninterrupted(&self) -> bool {
        self.stamp.1 != u64::MAX && self.stamp.1 == self.current.get().1
    }

    pub(super) fn continuous(&self) -> bool {
        self.stamp.0 != u64::MAX && self.stamp == self.current.get() && self.at.layout_current()
    }

    pub(super) fn held(&self) -> TalkHeld {
        self.at.held
    }

    #[cfg(test)]
    pub(super) fn fixture(held: TalkHeld) -> Self {
        let mut at = EventTime::new(Instant::now());
        at.held = held;
        Self {
            current: Rc::default(),
            stamp: (0, 0),
            at,
        }
    }
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
#[expect(
    clippy::too_many_arguments,
    reason = "one argument for each thing the native callback shares with presence"
)]
pub(super) fn deliver(
    gesture: Rc<Cell<Gesture>>,
    dock: AnyWindowHandle,
    owner: WeakEntity<Presence>,
    link: LinkHandle,
    target: Rc<Cell<(Option<ThreadId>, u64)>>,
    paused: Rc<Cell<bool>>,
    gates: Gates,
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
    let input = Rc::new(Cell::new((0_u64, 0_u64)));
    let reduce: Rc<Reduce> = Rc::new(move |event, at| {
        let (serial, reset) = input.get();
        if event == Event::Reset {
            input.set((serial.saturating_add(1), reset.saturating_add(1)));
        } else if matches!(event, Event::Escape | Event::Key { .. }) && !at.repeated_modifier {
            input.set((serial.saturating_add(1), reset));
        }
        let Ok(qpc) = u64::try_from(performance_counter()) else {
            tracing::warn!(stage = ?InputStage::Delivery, reason = ?InputReason::ClockUnavailable, "held speech input failed");
            input.set((serial.saturating_add(1), reset.saturating_add(1)));
            link.cancel_input();
            let mut state = gesture.get();
            let _cancelled =
                fence
                    .borrow_mut()
                    .reduce(&mut state, Event::Reset, at.instant, &paused);
            gesture.set(state);
            if !state.engaged() {
                gates.disengaged();
            }
            return None;
        };
        if event == Event::Reset {
            escaped.set(None);
            escape_down.set(false);
        }
        if let Event::Key {
            key: Key::Escape(_),
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
        let delivery = fence
            .borrow_mut()
            .reduce(&mut state, event, at.instant, &paused);
        gesture.set(state);
        for intent in delivery.intents.iter().flatten() {
            match intent {
                // Key-down takes the capture inhibit here, never waiting; nothing retires here.
                Intent::Warm(warm) => {
                    gates.warm(
                        link.output(),
                        *warm,
                        link.link_generation(),
                        link.choice_revision(),
                    );
                }
                Intent::Recognized {
                    gesture,
                    generation,
                } => {
                    if generation.is_ok() {
                        link.cancel_input();
                        link.track_input(*gesture);
                    }
                    gates.recognized(*gesture, generation.is_ok());
                }
                Intent::End {
                    cancelled: true, ..
                } => {
                    link.cancel_input();
                }
                _ => {}
            }
        }
        if !state.engaged() {
            gates.disengaged();
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
        // Sampled once, here in the callback, so a screen hold's line starts where the pointer
        // was at recognition and its picture is of the display the pointer was on at release,
        // however late the owner applies the delivery.
        let pointer = delivery
            .intents
            .iter()
            .flatten()
            .any(|intent| {
                matches!(
                    intent,
                    Intent::Recognized {
                        generation: Ok(_),
                        ..
                    } | Intent::End {
                        cancelled: false,
                        ..
                    }
                )
            })
            .then(cursor_position)
            .and_then(Result::ok)
            .map(PhysicalPoint);
        let pending = Pending {
            delivery,
            qpc,
            pointer,
            input: ReleaseInput {
                current: Rc::clone(&input),
                stamp: input.get(),
                at,
            },
            cancelled,
            escape: escape.then(|| gates.hold_back()),
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
            return gesture.get().deadline();
        }
        backlog.set(backlog.get() + 1);
        tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::Deferred, "held speech input boundary");
        if deferred.try_send(pending).is_err() {
            link.cancel_input();
            backlog.set(backlog.get() - 1);
            tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::QueueClosed, "held speech input boundary");
        }
        gesture.get().deadline()
    });
    let scheduled = Rc::new(Cell::new((0, None)));
    let timers = cx.to_async();
    Box::new(move |event, at| {
        let deadline = reduce(event, at);
        schedule(&reduce, &scheduled, deadline, &timers);
        deadline
    })
}

type Reduce = dyn Fn(Event, EventTime) -> Option<Instant>;

/// Backs the native deadline with GPUI's timer, whose wake is a posted message that a thread
/// busy with paint and timer messages can't starve. Whichever arrives first recognises the
/// hold; the other finds nothing due. `scheduled` is the latest timer's serial and deadline.
fn schedule(
    reduce: &Rc<Reduce>,
    scheduled: &Rc<Cell<(u64, Option<Instant>)>>,
    deadline: Option<Instant>,
    cx: &AsyncApp,
) {
    let (serial, armed) = scheduled.get();
    if armed == deadline {
        return;
    }
    let serial = serial.wrapping_add(1);
    scheduled.set((serial, deadline));
    let Some(deadline) = deadline else {
        return;
    };
    let (reduce, scheduled) = (Rc::downgrade(reduce), Rc::downgrade(scheduled));
    cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        executor
            .timer(deadline.saturating_duration_since(executor.now()))
            .await;
        let (Some(reduce), Some(scheduled)) = (reduce.upgrade(), scheduled.upgrade()) else {
            return;
        };
        if scheduled.get().0 != serial {
            return;
        }
        scheduled.set((serial, None));
        let next = reduce(Event::Scheduled, EventTime::new(executor.now()));
        schedule(&reduce, &scheduled, next, cx);
    })
    .detach();
}

fn stale_hold_escape(owner: &Presence, gesture: Option<GestureId>, session: u64) -> bool {
    gesture.is_some()
        && owner.session.as_ref().is_none_or(|current| {
            current.generation() != session || !current.alive().load(Ordering::Acquire)
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
        let uninterrupted = pending.input.uninterrupted();
        let continuous = pending.delivery.intents.iter().any(|intent| {
            matches!(
                intent,
                Some(Intent::End {
                    cancelled: false,
                    ..
                })
            )
        }) && pending.input.continuous();
        let applied = owner.update(app, |this, cx| {
            if pending.escape.is_some()
                && (!uninterrupted
                    || stale_hold_escape(this, pending.cancelled, pending.stamp.session))
            {
                return false;
            }
            let mut effects = pending.delivery.effects;
            if effects.contains(&Some(crate::input::Effect::OpenInput)) {
                if uninterrupted
                    && this.native.escape_trusted()
                    && this.link.microphone_gate().latest() == pending.collection
                {
                    if let Some(gesture) = pending.cancelled {
                        this.cancel_voice_gesture(gesture, cx);
                    }
                } else {
                    effects = effects.map(|effect| {
                        effect.filter(|effect| *effect != crate::input::Effect::OpenInput)
                    });
                }
            }
            this.apply_effects(effects, window, cx);
            true
        });
        if applied.as_ref().is_ok_and(|apply| *apply)
            && (pending.escape.is_some() || pending.delivery.intents.iter().any(Option::is_some))
        {
            let (owner, intents) = (owner.clone(), pending.delivery.intents.clone());
            let (stamp, qpc, pointer) = (pending.stamp, pending.qpc, pending.pointer);
            let input = pending.input.clone();
            let (escaping, cancelled) = (pending.escape.clone(), pending.cancelled);
            let escape = escaping.is_some();
            let intents = move |app: &mut App| {
                #[cfg(test)]
                if continuous && let Some(advance) = BEFORE_DEFERRED_RELEASE.take() {
                    advance();
                }
                let _gone = owner.update(app, |this, cx| {
                    let uninterrupted = uninterrupted && input.uninterrupted();
                    if escape
                        && (!uninterrupted || stale_hold_escape(this, cancelled, stamp.session))
                    {
                        return;
                    }
                    let release = (continuous && input.continuous()).then_some(&input);
                    for mut intent in intents.into_iter().flatten() {
                        if !uninterrupted || !this.native.escape_trusted() {
                            if let Intent::End { cancelled, .. } = &mut intent {
                                *cancelled = true;
                            } else {
                                continue;
                            }
                        }
                        this.voice_delivery(intent, stamp, qpc, pointer, release, cx);
                    }
                    if escape && this.native_escape(cancelled, cx).is_err() {
                        tracing::error!("can't apply native Escape");
                    }
                });
                // The Esc is decided, stop or not: retirement may go on.
                drop(escaping);
            };
            // Deferred to the end of this update, where the dock is back in GPUI's window table.
            app.defer(intents);
        }
        applied.is_ok()
    });
    match result {
        Ok(true) => Applied::Yes,
        Ok(false) => Applied::Gone,
        Err(_) if owner.upgrade().is_none() => Applied::Gone,
        Err(_) => Applied::Busy,
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_DEFERRED_RELEASE: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(super) fn before_deferred_release(advance: impl FnOnce() + 'static) {
    assert!(
        BEFORE_DEFERRED_RELEASE
            .replace(Some(Box::new(advance)))
            .is_none()
    );
}

/// Delivers one intent the way the native callback does, and says whether it applied.
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "one argument for each thing the native callback reads"
)]
pub(super) fn deliver_intent(
    cx: &mut AsyncApp,
    dock: AnyWindowHandle,
    owner: &WeakEntity<Presence>,
    intent: Intent,
    stamp: Stamp,
    qpc: u64,
    pointer: Option<PhysicalPoint>,
    held: TalkHeld,
) -> bool {
    let pending = Pending {
        delivery: Delivery {
            effects: [None, None, None],
            intents: [Some(intent), None, None],
        },
        stamp,
        qpc,
        pointer,
        input: ReleaseInput::fixture(held),
        cancelled: None,
        escape: None,
        collection: 0,
    };
    matches!(apply(cx, dock, owner, &pending), Applied::Yes)
}
