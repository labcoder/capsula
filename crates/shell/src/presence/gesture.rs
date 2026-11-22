//! Carries keyboard gestures from the native Raw Input callback into the presence owner.
//!
//! The callback closes collection authority before applying or queuing UI work. Cancellations
//! always wait in the ordered queue, where the task sends the wire cancel before applying the
//! corresponding effects. Other effects also queue when the app is busy. Intents run when the dock's update has ended, inside the same call:
//! GPUI takes a window out of its table while updating it, and a recognized hold's capture
//! lease must reach every presence window, the dock included.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, BackgroundExecutor, Context, WeakEntity,
};

use super::{
    Presence,
    voice::{Gates, HoldBack, Stamp},
};
use crate::input::{
    Edge, Effect, Event, Gesture, Key, TalkHeld,
    hold::{Delivery, Fence, InputReason, InputStage, Intent},
};
use crate::link::{InputCancel, LinkHandle};
use crate::platform::{Deliver, EventTime, cursor_position, performance_counter};
use crate::strings::Text;
use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{GestureId, ThreadId};

struct Pending {
    cancel: Option<InputCancel>,
    gesture: Option<PendingGesture>,
}

struct PendingGesture {
    delivery: Delivery,
    stamp: Stamp,
    qpc: u64,
    /// Where the pointer was when this delivery recognized a hold or saw its keys come up.
    pointer: Option<PhysicalPoint>,
    input: ReleaseInput,
    cancelled: Option<(GestureId, u64)>,
    /// An Esc press: every retirement waits until this delivery is decided.
    escape: Option<Rc<HoldBack>>,
    collection: u64,
}

impl PendingGesture {
    fn typing_effects(
        &self,
        owner: &mut Presence,
        cx: &mut Context<'_, Presence>,
    ) -> [Option<Effect>; 3] {
        let effects = self.delivery.effects;
        if !effects.contains(&Some(Effect::OpenInput)) {
            return effects;
        }
        if !self.input.uninterrupted()
            || !owner.native.escape_trusted()
            || owner.link.microphone_gate().latest() != self.collection
        {
            return effects.map(|effect| effect.filter(|effect| *effect != Effect::OpenInput));
        }
        if let Some((gesture, _)) = self.cancelled {
            owner.cancel_voice_gesture(gesture, cx);
        }
        effects
    }
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

#[derive(Clone, Copy)]
enum CancelDispatch {
    Inline,
    Clear,
    Refused,
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
    let backlog = Rc::new(Cell::new(0_usize));
    let waiting = (owner.clone(), link.clone(), Rc::clone(&backlog));
    cx.spawn(async move |cx| {
        let (owner, link, backlog) = waiting;
        while let Ok(Pending { cancel, gesture }) = pending.recv().await {
            let cancel = if let Some(cancel) = cancel {
                let link = link.clone();
                let sent = cx.background_executor().spawn(async move { link.dispatch_input_cancel(&cancel) }).await;
                #[cfg(test)]
                if let Some((arrived, resume)) = WIRE_CANCEL_BARRIER.take() {
                    arrived.try_send(()).expect("the test waits for the wire cancel");
                    let _resume = resume.recv().await;
                }
                if sent {
                    CancelDispatch::Clear
                } else {
                    CancelDispatch::Refused
                }
            } else {
                CancelDispatch::Clear
            };
            if let Some(gesture) = gesture {
                loop {
                    match apply(cx, dock, &owner, &gesture, cancel) {
                        Applied::Yes => break,
                        Applied::Gone => {
                            tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::OwnerGone, "held speech input boundary");
                            break;
                        }
                        Applied::Busy => cx.background_executor().timer(RETRY).await,
                    }
                }
            }
            backlog.set(backlog.get() - 1);
        }
    })
    .detach();
    let executor = cx.background_executor().clone();
    let async_cx = cx.to_async();
    let fence = std::cell::RefCell::new(Fence::new(link.microphone_gate()));
    let escaped = Cell::new(None);
    let escape_down = Cell::new(false);
    let recovered_hold = Cell::new(None);
    let input = Rc::new(Cell::new((0_u64, 0_u64)));
    let reduce: Rc<Reduce> = Rc::new(move |event, mut at| {
        let (serial, reset) = input.get();
        if matches!(event, Event::Reset | Event::Escape) {
            input.set((serial.saturating_add(1), reset.saturating_add(1)));
        } else if matches!(event, Event::Key { .. }) && !at.repeated_modifier {
            input.set((serial.saturating_add(1), reset));
        }
        let qpc = performance_counter();
        #[cfg(test)]
        let qpc = if FAIL_CLOCK.take() { -1 } else { qpc };
        let Ok(qpc) = u64::try_from(qpc) else {
            tracing::warn!(stage = ?InputStage::Delivery, reason = ?InputReason::ClockUnavailable, "held speech input failed");
            input.set((serial.saturating_add(1), reset.saturating_add(1)));
            if let Some(cancel) = link.revoke_input_for_dispatch() {
                enqueue(
                    &deferred,
                    &backlog,
                    &executor,
                    &link,
                    Pending {
                        cancel: Some(cancel),
                        gesture: None,
                    },
                );
            }
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
        let mut wire_cancel = None;
        if let Event::Key {
            key: Key::Escape(_),
            edge,
        } = event
        {
            if edge == Edge::Down && !escape_down.replace(true) {
                wire_cancel = link.revoke_input_for_dispatch();
                escaped.set(wire_cancel.as_ref().map(InputCancel::target));
            } else if edge == Edge::Up {
                escape_down.set(false);
                escaped.set(None);
            }
        }
        let escape = event == Event::Escape;
        let mut cancelled = if escape {
            escaped.take().or_else(|| {
                wire_cancel = link.revoke_input_for_dispatch();
                wire_cancel.as_ref().map(InputCancel::target)
            })
        } else {
            None
        };
        let mut state = gesture.get();
        let mut recovered = false;
        let session = link.link_generation();
        let voice_paused = Cell::new(paused.get() || !link.can_start_voice_gesture(session));
        let delivery = fence.borrow_mut().reduce_with_keyboard_read(
            &mut state,
            event,
            at.instant,
            &voice_paused,
            || {
                let now = at.recovery_time()?;
                recovered = true;
                Some(now)
            },
        );
        gesture.set(state);
        let mut cancellation = cancelled.is_some();
        for intent in delivery.intents.iter().flatten() {
            if let Intent::End { gesture, .. } = intent
                && recovered_hold.get() == Some(*gesture)
            {
                at.held = TalkHeld::default();
            }
            match intent {
                // Key-down takes the capture inhibit here, never waiting; nothing retires here.
                Intent::Warm(warm) => {
                    if recovered {
                        recovered_hold.set(Some(*warm));
                    }
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
                        debug_assert!(
                            wire_cancel.is_none(),
                            "recognition cannot carry an earlier cancel"
                        );
                        wire_cancel = link.revoke_input_for_dispatch();
                        link.track_input_on(*gesture, session);
                    }
                    gates.recognized(*gesture, generation.is_ok());
                }
                Intent::End {
                    cancelled: true, ..
                } => {
                    cancellation |= !escape;
                    wire_cancel = wire_cancel.or_else(|| link.revoke_input_for_dispatch());
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
            wire_cancel = wire_cancel.or_else(|| link.revoke_input_for_dispatch());
            cancellation = true;
            cancelled = wire_cancel.as_ref().map(InputCancel::target);
        }
        if delivery.effects.iter().all(Option::is_none)
            && delivery.intents.iter().all(Option::is_none)
            && !escape
        {
            if let Some(cancel) = wire_cancel {
                enqueue(
                    &deferred,
                    &backlog,
                    &executor,
                    &link,
                    Pending {
                        cancel: Some(cancel),
                        gesture: None,
                    },
                );
            }
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
        let pending = PendingGesture {
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
                session,
                choice: link.choice_revision(),
            },
        };
        if !cancellation
            && wire_cancel.is_none()
            && !at.defer_terminal
            && backlog.get() == 0
            && matches!(
                apply(
                    &mut async_cx.clone(),
                    dock,
                    &owner,
                    &pending,
                    CancelDispatch::Inline
                ),
                Applied::Yes
            )
        {
            return gesture.get().deadline();
        }
        enqueue(
            &deferred,
            &backlog,
            &executor,
            &link,
            Pending {
                cancel: wire_cancel,
                gesture: Some(pending),
            },
        );
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

fn enqueue(
    deferred: &async_channel::Sender<Pending>,
    backlog: &Cell<usize>,
    executor: &BackgroundExecutor,
    link: &LinkHandle,
    pending: Pending,
) {
    #[cfg(test)]
    if CLOSE_DELIVERY.take() {
        deferred.close();
    }
    if let Err(refused) = deferred.try_send(pending) {
        let Pending { cancel, .. } = refused.into_inner();
        let current = link.revoke_input_for_dispatch();
        if cancel.is_some() || current.is_some() {
            let link = link.clone();
            executor
                .spawn(async move {
                    for cancel in cancel.into_iter().chain(current) {
                        let _sent = link.dispatch_input_cancel(&cancel);
                    }
                })
                .detach();
        }
        tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::QueueClosed, "held speech input boundary");
    } else {
        backlog.set(backlog.get() + 1);
        tracing::debug!(stage = ?InputStage::Delivery, reason = ?InputReason::Deferred, "held speech input boundary");
    }
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

fn stale_hold_escape(owner: &Presence, cancelled: Option<(GestureId, u64)>) -> bool {
    cancelled.is_some_and(|(_, generation)| {
        owner.session.as_ref().is_none_or(|current| {
            current.generation() != generation || !current.alive().load(Ordering::Acquire)
        })
    })
}

/// Applies effects through a fallible borrow of the app.
fn apply(
    cx: &mut AsyncApp,
    dock: AnyWindowHandle,
    owner: &WeakEntity<Presence>,
    pending: &PendingGesture,
    cancel: CancelDispatch,
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
                && matches!(cancel, CancelDispatch::Inline)
                && this.voice.pending.is_some()
            {
                return None;
            }
            if pending.escape.is_some()
                && (!uninterrupted || stale_hold_escape(this, pending.cancelled))
            {
                return Some(false);
            }
            let effects = pending.typing_effects(this, cx);
            this.apply_effects(effects, window, cx);
            Some(true)
        });
        let applied = match applied {
            Ok(Some(applied)) => applied,
            Ok(None) => return Applied::Busy,
            Err(_) => return Applied::Gone,
        };
        if applied
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
                    if escape && (!uninterrupted || stale_hold_escape(this, cancelled)) {
                        return;
                    }
                    let release = (continuous && input.continuous()).then_some(&input);
                    let trusted = uninterrupted && this.native.escape_trusted();
                    for mut intent in intents.into_iter().flatten() {
                        if matches!(cancel, CancelDispatch::Refused) || !trusted {
                            if let Intent::End { cancelled, .. } = &mut intent {
                                *cancelled = true;
                            } else {
                                if let Intent::Recognized {
                                    gesture,
                                    generation: Ok(token),
                                } = intent
                                    && this.voice.gates.skip_recognition(token)
                                {
                                    let refused = trusted
                                        && matches!(cancel, CancelDispatch::Refused)
                                        && this.link.input_is(gesture);
                                    this.link.clear_input(gesture);
                                    if refused {
                                        this.voice.lost = None;
                                        this.voice_notice(Text::HoldFailed, cx);
                                    }
                                }
                                continue;
                            }
                        }
                        this.voice_delivery(intent, stamp, qpc, pointer, release, cx);
                    }
                    if escape
                        && this
                            .native_escape(cancelled.map(|(gesture, _)| gesture), cx)
                            .is_err()
                    {
                        tracing::error!("can't apply native Escape");
                    }
                });
                // The Esc is decided, stop or not: retirement may go on.
                drop(escaping);
            };
            // Deferred to the end of this update, where the dock is back in GPUI's window table.
            app.defer(intents);
        }
        Applied::Yes
    });
    match result {
        Ok(applied) => applied,
        Err(_) if owner.upgrade().is_none() => Applied::Gone,
        Err(_) => Applied::Busy,
    }
}

#[cfg(test)]
type WireCancelBarrier = (async_channel::Sender<()>, async_channel::Receiver<()>);

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_CLOCK: Cell<bool> = const { Cell::new(false) };
    pub(super) static CLOSE_DELIVERY: Cell<bool> = const { Cell::new(false) };
    static WIRE_CANCEL_BARRIER: Cell<Option<WireCancelBarrier>> = const { Cell::new(None) };
    static BEFORE_DEFERRED_RELEASE: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(super) fn wire_cancel_before_effects(
    cx: &mut gpui_kit::TestAppContext,
    send: impl FnOnce(&mut gpui_kit::TestAppContext),
) {
    let (arrived, wire) = async_channel::bounded(1);
    let (release, resume) = async_channel::bounded(1);
    assert!(
        WIRE_CANCEL_BARRIER
            .replace(Some((arrived, resume)))
            .is_none()
    );
    send(cx);
    cx.run_until_parked();
    assert_eq!(wire.try_recv(), Ok(()));
    drop(release);
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
    let pending = PendingGesture {
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
    matches!(
        apply(cx, dock, owner, &pending, CancelDispatch::Clear),
        Applied::Yes
    )
}
