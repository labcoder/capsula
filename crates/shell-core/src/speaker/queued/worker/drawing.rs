use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kumochi_protocol::message::MAX_SEGMENTS_PER_RUN;
use kumochi_scene::record::Start;

use super::gate::DropOutcome;
use super::{Anchor, Engine, Worker};
use crate::speaker::conductor::{
    Binding, Completion, Conductor, Gate, Handoff, Head, Motion, Picture, PictureLevel,
    PictureState, Playback, Route, RouteStamp, SynthesisFinal, View, ViewLevel,
};

pub(in crate::speaker::queued) trait DrawingIo: Send + 'static {
    fn view(&mut self) -> Result<Option<ViewLevel>, ()>;
    fn picture(&mut self, binding: Binding, level: Option<PictureLevel>);
    fn owner_exited(&mut self);
}

pub(super) struct Drawing {
    session: u64,
    closed: Arc<AtomicBool>,
    io: Box<dyn DrawingIo>,
    newest: Option<u64>,
    wanted: Option<ViewLevel>,
    pending_revoke: Option<Binding>,
    bound: Option<Bound>,
    published: Option<PictureLevel>,
}

struct Bound {
    key: Binding,
    core: Conductor,
    handoff: Option<Handoff>,
    revoked: bool,
}

impl Drawing {
    pub(super) fn new(session: u64, closed: Arc<AtomicBool>, io: impl DrawingIo) -> Self {
        Self {
            session,
            closed,
            io: Box::new(io),
            newest: None,
            wanted: None,
            pending_revoke: None,
            bound: None,
            published: None,
        }
    }

    fn accepts(&self, key: Binding, engine: &Engine) -> bool {
        let a = key.assignment;
        a.session == self.session
            && a.run == engine.queue.run
            && a.output == engine.queue.logical
            && a.segment.0 < MAX_SEGMENTS_PER_RUN
    }

    fn read(&mut self, engine: &Engine) -> bool {
        let Ok(next) = self.io.view() else {
            return false;
        };
        if let Some(next) = next {
            let key = next.binding;
            if !self.accepts(key, engine) {
                return true;
            }
            if self.newest.is_none_or(|nonce| key.nonce > nonce) {
                self.newest = Some(key.nonce);
            } else if self.wanted.is_none_or(|old| {
                old.binding != key
                    || old.speech != next.speech
                    || old.view == View::Fenced && next.view != View::Fenced
            }) {
                return true;
            }
        }
        self.wanted = next;
        true
    }

    pub(super) fn revoke(&mut self, key: Binding, engine: &Engine) {
        if !self.accepts(key, engine) {
            return;
        }
        if let Some(bound) = &mut self.bound
            && bound.key == key
        {
            bound.revoked = true;
        } else if self.newest.is_none_or(|nonce| key.nonce > nonce)
            && self.pending_revoke.is_none_or(|old| key.nonce > old.nonce)
        {
            self.pending_revoke = Some(key);
        }
    }

    pub(super) fn deadline(&self) -> Option<Duration> {
        self.bound.as_ref()?.core.deadline()
    }

    pub(super) fn observe(&mut self, anchor: Anchor, now: Duration) {
        let Some(bound) = &mut self.bound else {
            return;
        };
        if bound.key.assignment.segment != anchor.segment
            || bound
                .handoff
                .is_some_and(|seen| seen.generation == anchor.generation)
        {
            return;
        }
        let frames = anchor.through - anchor.first - 1;
        let back = Duration::from_nanos(frames * 1_000_000_000 / u64::from(super::super::RATE));
        bound.handoff = Some(Handoff {
            generation: anchor.generation,
            first: now,
            predicted: Some(anchor.at.saturating_sub(back)),
            latency: None,
            motion: None,
        });
    }

    fn publish(
        &mut self,
        key: Binding,
        route: RouteStamp,
        picture: Picture,
        completion: Completion,
    ) {
        let Some(state) = picture_state(picture, completion) else {
            return;
        };
        let next = PictureLevel {
            binding: key,
            route,
            state,
        };
        if self.published != Some(next) {
            self.published = Some(next);
            self.io.picture(key, Some(next));
        }
    }

    fn clear(&mut self) {
        if let Some(old) = self.published.take() {
            self.io.picture(old.binding, None);
        }
    }
}

fn picture_state(picture: Picture, completion: Completion) -> Option<PictureState> {
    if completion == Completion::Ended && picture != Picture::Fenced {
        return match picture {
            Picture::Wait
            | Picture::Silent
            | Picture::Authored
            | Picture::Shown(Motion::Authored) => Some(PictureState::Ended(picture)),
            _ => None,
        };
    }
    // shortcut: post-onset following isn't emitted yet; add it if D1 is approved to follow audio.
    Some(PictureState::Owned(match picture {
        Picture::Shown(_) => Picture::Shown(Motion::Authored),
        other => other,
    }))
}

impl Drop for Drawing {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.io.owner_exited();
    }
}

impl Worker {
    pub(super) fn read_drawing(&mut self) -> bool {
        self.drawing
            .as_mut()
            .is_none_or(|drawing| drawing.read(&self.engine))
    }

    pub(super) fn update_drawing(&mut self, now: Duration) {
        if let Some(mut drawing) = self.drawing.take() {
            self.apply_drawing(&mut drawing, now);
            self.drawing = Some(drawing);
        }
    }

    fn apply_drawing(&mut self, drawing: &mut Drawing, now: Duration) {
        let mut bound = drawing.bound.take();
        if bound
            .as_ref()
            .is_some_and(|old| drawing.wanted.is_none_or(|next| next.binding != old.key))
            && let Some(mut old) = bound.take()
        {
            self.reduce_drawing(&mut old, View::Fenced, now);
        }
        let Some(next) = drawing.wanted else {
            drawing.clear();
            return;
        };
        let mut bound = bound.unwrap_or_else(|| {
            let start = if next.speech {
                Start::Speech
            } else {
                Start::Now
            };
            let revoked = drawing.pending_revoke == Some(next.binding);
            if drawing
                .pending_revoke
                .is_some_and(|key| key.nonce <= next.binding.nonce)
            {
                drawing.pending_revoke = None;
            }
            Bound {
                key: next.binding,
                core: Conductor::new(next.binding.assignment, &start),
                handoff: None,
                revoked,
            }
        });
        let view = if self.engine.queue.stopped {
            View::Fenced
        } else {
            next.view
        };
        let (route, picture) = self.reduce_drawing(&mut bound, view, now);
        drawing.publish(bound.key, route, picture, bound.core.completion());
        drawing.bound = Some(bound);
    }

    fn reduce_drawing(
        &mut self,
        bound: &mut Bound,
        view: View,
        now: Duration,
    ) -> (RouteStamp, Picture) {
        let id = bound.key.assignment.segment;
        let segment = self.engine.queue.segment(id);
        let unheard = self.engine.queue.has_unheard(id);
        let fully_retired = self.engine.queue.completed > id.0;
        let head = if bound.revoked || self.engine.queue.discarded(id) {
            Head::Revoked
        } else if fully_retired && !unheard {
            Head::Played
        } else if self.engine.queue.completed < id.0 {
            Head::AdvancingPredecessor
        } else if self.engine.source_ready()
            && segment.is_some_and(|value| value.end() > self.engine.progress.consumed)
        {
            Head::EligiblePcm
        } else {
            Head::WaitingForPcm
        };
        let synthesis = segment
            .filter(|value| value.ended)
            .map(|value| SynthesisFinal {
                segment: id,
                samples: value.samples,
            });
        let (stamp, mut route) = self.drawing_route();
        if unheard
            && route != Route::Rebuilding
            && (fully_retired || stamp == RouteStamp::Unstamped)
        {
            route = Route::Virtual;
        }
        let decision = bound
            .core
            .update(
                bound.key.assignment,
                now,
                view,
                Playback {
                    route,
                    head,
                    handoff: bound.handoff,
                    synthesis,
                },
            )
            .expect("the worker reduces its own drawing binding");
        match decision.audio {
            Gate::Held => self.engine.hold(Some(id), now),
            Gate::Allowed => self.engine.release(id, now),
            Gate::Dropped => {
                if self.drop_unsubmitted(id, now) == DropOutcome::Kept {
                    bound.core.keep_audio();
                }
            }
        }
        (stamp, decision.picture)
    }

    fn drawing_route(&self) -> (RouteStamp, Route) {
        if let Some(route) = &self.physical
            && route.retire_deadline.is_none()
        {
            return (
                RouteStamp::Live(route.generation),
                Route::Live(route.generation),
            );
        }
        let stamp = self
            .retired_route
            .map_or(RouteStamp::Unstamped, RouteStamp::Retired);
        let route = if self.virtual_route || self.route_closed {
            Route::Virtual
        } else if self.retired_route.is_some() {
            Route::Rebuilding
        } else {
            Route::Pending
        };
        (stamp, route)
    }
}

#[cfg(test)]
#[path = "drawing_payload_tests.rs"]
mod tests;
