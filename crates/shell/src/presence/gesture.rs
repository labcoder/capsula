//! Carries keyboard gestures from the native Raw Input callback into the presence owner.
//!
//! The callback reduces the gesture itself, so Listening changes in the same `WM_INPUT`
//! callback that completes the chord, before timers, channels or GPUI. Effects then reach the
//! owner through `update_window`, which only tries to borrow the app. If GPUI is already
//! borrowed, because a nested message loop dispatched this callback, effects wait in order
//! with their original times and are retried until they apply or the owner is gone, instead of
//! panicking or being lost.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{AnyWindowHandle, App, AppContext as _, AsyncApp, WeakEntity};

use super::Presence;
use crate::input::{Effect, Gesture};
use crate::platform::{Deliver, EventTime};

type Effects = [Option<Effect>; 3];

/// How long a busy app is waited for between retries.
const RETRY: Duration = Duration::from_millis(1);

enum Applied {
    Yes,
    Busy,
    Gone,
}

/// Builds the native callback. `dock` is always open, so it anchors the app borrow.
pub(super) fn deliver(
    gesture: Rc<Cell<Gesture>>,
    dock: AnyWindowHandle,
    owner: WeakEntity<Presence>,
    cx: &mut App,
) -> Deliver {
    let (deferred, pending) = async_channel::unbounded::<(Effects, EventTime)>();
    // Queued plus in flight, so a newer effect never overtakes one still being retried.
    let backlog = Rc::new(Cell::new(0_usize));
    let waiting = (owner.clone(), Rc::clone(&backlog));
    cx.spawn(async move |cx| {
        let (owner, backlog) = waiting;
        while let Ok((effects, at)) = pending.recv().await {
            loop {
                match apply(cx, dock, &owner, effects, at) {
                    Applied::Yes => break,
                    Applied::Gone => return,
                    Applied::Busy => cx.background_executor().timer(RETRY).await,
                }
            }
            backlog.set(backlog.get().saturating_sub(1));
        }
    })
    .detach();
    let async_cx = cx.to_async();
    Box::new(move |event, at| {
        let mut state = gesture.get();
        let effects = state.handle(event, at.instant);
        gesture.set(state);
        if effects.iter().all(Option::is_none) {
            return state.deadline();
        }
        if backlog.get() == 0
            && matches!(
                apply(&mut async_cx.clone(), dock, &owner, effects, at),
                Applied::Yes
            )
        {
            return state.deadline();
        }
        backlog.set(backlog.get() + 1);
        if deferred.try_send((effects, at)).is_err() {
            backlog.set(backlog.get() - 1);
            tracing::debug!("presence closed; gesture effects dropped");
        }
        state.deadline()
    })
}

/// Applies effects through a fallible borrow of the app.
fn apply(
    cx: &mut AsyncApp,
    dock: AnyWindowHandle,
    owner: &WeakEntity<Presence>,
    effects: Effects,
    at: EventTime,
) -> Applied {
    let result = cx.update_window(dock, |_, window, app| {
        owner
            .update(app, |this, cx| this.apply_effects(effects, at, window, cx))
            .is_ok()
    });
    match result {
        Ok(true) => Applied::Yes,
        Ok(false) => Applied::Gone,
        Err(_) if owner.upgrade().is_none() => Applied::Gone,
        Err(_) => Applied::Busy,
    }
}
