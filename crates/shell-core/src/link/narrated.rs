use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use std::sync::{OnceLock, Weak};
use std::thread::{self, Thread};
use std::time::Instant;

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use kumochi_protocol::message::{ControlToBrain, SpeechDecision};
use kumochi_protocol::message::{MAX_VISIBLE_SCENES, RunId, SceneId, SpeechDispatch};
use kumochi_scene::limits::MAX_STEPS;

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use super::{
    LinkHandle, Session, SessionIo,
    output::{Abort, Denied},
};
use crate::presenter::DrawKey;
#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use crate::presenter::Presenter;
use crate::speaker::conductor::{Assignment, Binding, PictureLevel, ViewLevel};
#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use crate::speaker::conductor::{Picture, PictureState, RouteStamp, View};
#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
use crate::speaker::queued::Command;
use crate::speaker::queued::{DrawingIo, Ordered, OwnerExit};

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct Pair {
    io: Weak<SessionIo>,
    abort: Weak<dyn Fn() + Send + Sync>,
}

#[derive(Debug)]
pub(crate) struct Narrated {
    pub(super) origin: Instant,
    stopped: AtomicU64,
    state: Mutex<State>,
    send: async_channel::Sender<Ordered>,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    reports: async_channel::Receiver<Ordered>,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pair: OnceLock<Pair>,
}

#[derive(Debug, Default)]
struct State {
    promised: Option<Assignment>,
    replaced_before_promise: [Option<SceneId>; MAX_VISIBLE_SCENES],
    retired: bool,
    unassigned: Vec<DrawKey>,
    assignment: Option<Assignment>,
    view: Option<ViewLevel>,
    picture: Option<PictureLevel>,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    applied: Option<Binding>,
    decision: Option<SpeechDispatch>,
    retired_decision: Option<(u64, SpeechDispatch)>,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    decided: bool,
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    ended: Option<Binding>,
    worker: Option<Thread>,
}

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
impl State {
    fn picture_for(&self, view: Option<ViewLevel>) -> Option<PictureLevel> {
        self.ended.map_or(self.picture, |binding| {
            let pending_onset = view
                .is_some_and(|level| level.binding == binding && level.view == View::Eligible)
                && self.picture.is_some_and(|level| {
                    level.binding == binding
                        && matches!(level.state, PictureState::Owned(Picture::At(_)))
                });
            if pending_onset {
                self.picture
            } else {
                Some(PictureLevel {
                    binding,
                    route: RouteStamp::Unstamped,
                    state: PictureState::Unavailable,
                })
            }
        })
    }
}

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
fn dispatch(a: Assignment) -> SpeechDispatch {
    SpeechDispatch {
        run: a.run,
        output: a.output,
        segment: a.segment,
    }
}

fn draw_key(a: Assignment) -> DrawKey {
    DrawKey {
        run: a.run,
        scene: a.step.scene,
        index: a.step.index,
    }
}

pub(super) fn valid_key(key: DrawKey) -> bool {
    usize::try_from(key.index).is_ok_and(|index| index < MAX_STEPS)
}

impl Narrated {
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn new() -> Self {
        let (send, reports) = async_channel::bounded(4);
        Self {
            origin: Instant::now(),
            stopped: AtomicU64::new(0),
            state: Mutex::default(),
            send,
            reports,
            pair: OnceLock::new(),
        }
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn register_pair(&self, io: &Arc<SessionIo>, abort: &Abort) -> bool {
        self.pair
            .set(Pair {
                io: Arc::downgrade(io),
                abort: Arc::downgrade(abort),
            })
            .is_ok()
    }

    pub(super) fn stop(&self, run: RunId) {
        self.stopped.fetch_max(run.0, Ordering::Release);
    }

    fn stopped(&self, run: RunId) -> bool {
        run.0 <= self.stopped.load(Ordering::Acquire)
    }

    pub(super) fn promised(&self, assignment: Assignment) -> bool {
        let key = draw_key(assignment);
        valid_key(key)
            && self.state.lock().is_ok_and(|mut state| {
                if self.stopped(assignment.run) {
                    return true;
                }
                if state.unassigned.contains(&key) {
                    return false;
                }
                if let Some(old) = state.promised {
                    if old.run >= assignment.run || !self.stopped(old.run) {
                        return false;
                    }
                    let debt = state.decision.map(|key| (old.session, key));
                    if debt.is_some() && state.retired_decision.is_some() {
                        return false;
                    }
                    *state = State {
                        unassigned: std::mem::take(&mut state.unassigned),
                        replaced_before_promise: state.replaced_before_promise,
                        retired_decision: debt.or(state.retired_decision),
                        ..State::default()
                    };
                }
                state.retired = state.replaced_before_promise.contains(&Some(key.scene));
                state.promised = Some(assignment);
                true
            })
    }

    pub(super) fn replaced(&self, scenes: &[SceneId], successor: SceneId) -> bool {
        if scenes.is_empty()
            || scenes.len() > MAX_VISIBLE_SCENES
            || scenes.contains(&successor)
            || scenes
                .iter()
                .enumerate()
                .any(|(at, id)| scenes[..at].contains(id))
        {
            return false;
        }
        self.state.lock().is_ok_and(|mut state| {
            if let Some(assignment) = state.promised.filter(|a| !self.stopped(a.run)) {
                state.retired |= scenes.contains(&assignment.step.scene);
            } else {
                state.replaced_before_promise = std::array::from_fn(|at| scenes.get(at).copied());
            }
            true
        })
    }

    pub(super) fn unassigned(&self, key: DrawKey) -> bool {
        valid_key(key)
            && self.state.lock().is_ok_and(|mut state| {
                if state.promised.is_some_and(|a| draw_key(a) == key)
                    || state.unassigned.len() == MAX_VISIBLE_SCENES * MAX_STEPS
                    || state.unassigned.contains(&key)
                {
                    return false;
                }
                state.unassigned.push(key);
                true
            })
    }

    pub(super) fn announce(&self, assignment: Assignment) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            if self.stopped(assignment.run) {
                return true;
            }
            if state.promised != Some(assignment) || state.assignment.is_some() {
                return false;
            }
            state.assignment = Some(assignment);
            true
        })
    }

    pub(super) fn taken(&self, session: u64, key: SpeechDispatch) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            if state
                .retired_decision
                .take_if(|debt| *debt == (session, key))
                .is_some()
            {
                return true;
            }
            if state.assignment.is_none_or(|a| a.session != session) || state.decision != Some(key)
            {
                return false;
            }
            state.decision = None;
            true
        })
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    fn assignment(&self, key: DrawKey) -> Result<Option<Assignment>, ()> {
        if self.stopped(key.run) {
            return Err(());
        }
        let state = self.state.try_lock().map_err(|_| ())?;
        if let Some(assignment) = state
            .assignment
            .filter(|&a| !state.retired && draw_key(a) == key)
        {
            Ok(Some(assignment))
        } else {
            state.unassigned.contains(&key).then_some(None).ok_or(())
        }
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn attach(self: &Arc<Self>, presenter: &mut Presenter) {
        let bridge = Arc::clone(self);
        presenter.start_conduction(self.origin, move |key| bridge.assignment(key));
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn update(
        &self,
        link: &LinkHandle,
        session: &Session,
        presenter: &mut Presenter,
        now: Instant,
    ) -> Result<(), ()> {
        if !link
            .shared
            .narrated
            .as_deref()
            .is_some_and(|mode| std::ptr::eq(mode, self))
            || !self
                .pair
                .get()
                .is_some_and(|pair| std::ptr::eq(pair.io.as_ptr(), Arc::as_ptr(&session.io)))
        {
            return Err(());
        }
        self.drain_reports(session, presenter)?;
        let view = presenter.conductor_view(now);
        let (picture, refused) = {
            let Ok(mut state) = self.state.try_lock() else {
                return Ok(());
            };
            let Some(assignment) = state
                .assignment
                .filter(|a| session.open() && a.session == session.generation())
            else {
                return Ok(());
            };
            if self.stopped(assignment.run) {
                if let Some(level) = state.view.take() {
                    presenter.conductor_fence(level.binding);
                }
                return Ok(());
            }
            let removed = state.retired
                || presenter.conductor_removed(session.generation(), assignment.step.scene);
            let view = if removed {
                if let Some(level) = view {
                    presenter.conductor_fence(level.binding);
                }
                None
            } else {
                view
            };
            if state.view.is_none() && !state.decided && removed {
                let revoked = link
                    .shared
                    .output
                    .narrated_gate(Command::RevokeSource(assignment));
                if revoked == Err(Denied::Busy) {
                    return Ok(());
                }
                if revoked.is_err()
                    || !Self::decide(&mut state, session, assignment, SpeechDecision::Discard)
                {
                    drop(state);
                    self.refuse(session);
                    return Err(());
                }
            }
            let fenced = state.view.filter(|old| {
                old.view != View::Fenced
                    && view
                        .is_none_or(|next| next.view == View::Fenced || next.binding != old.binding)
            });
            let fence_accepted = fenced.is_none_or(|old| {
                link.shared
                    .output
                    .narrated_gate(Command::Revoke(old.binding))
                    .is_ok()
            });
            if state.view != view {
                state.view = view;
                if let Some(owner) = &state.worker {
                    owner.unpark();
                }
            }
            let allow = state.applied.filter(|&binding| {
                view.is_some_and(|view| view.binding == binding && view.view == View::Eligible)
            });
            let allow_accepted = fence_accepted
                && allow.is_none_or(|binding| {
                    state.applied = None;
                    Self::decide(
                        &mut state,
                        session,
                        binding.assignment,
                        SpeechDecision::Allow,
                    ) && link
                        .shared
                        .output
                        .narrated_gate(Command::Allow(binding))
                        .is_ok()
                });
            (state.picture_for(view), !fence_accepted || !allow_accepted)
        };
        if refused {
            self.refuse(session);
            return Err(());
        }
        presenter.conductor_picture(Ok(picture), now);
        Ok(())
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    fn drain_reports(&self, session: &Session, presenter: &mut Presenter) -> Result<(), ()> {
        while let Ok(mut state) = self.state.try_lock() {
            if !session.open()
                || state
                    .assignment
                    .is_none_or(|a| a.session != session.generation())
            {
                break;
            }
            let Ok(event) = self.reports.try_recv() else {
                break;
            };
            let binding = match event {
                Ordered::Applied(binding)
                | Ordered::Fenced(binding)
                | Ordered::Ended { binding, .. } => binding,
            };
            if state.assignment != Some(binding.assignment) || self.stopped(binding.assignment.run)
            {
                continue;
            }
            match event {
                Ordered::Applied(_) => {
                    state.applied = Some(binding);
                }
                Ordered::Fenced(_) => {
                    state.applied = None;
                    let accepted = state.decided
                        || Self::decide(
                            &mut state,
                            session,
                            binding.assignment,
                            SpeechDecision::Discard,
                        );
                    drop(state);
                    presenter.conductor_fence(binding);
                    if !accepted {
                        self.refuse(session);
                        return Err(());
                    }
                }
                Ordered::Ended { .. } => {
                    state.applied = None;
                    state.ended = Some(binding);
                }
            }
        }
        Ok(())
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    fn decide(
        state: &mut State,
        session: &Session,
        assignment: Assignment,
        decision: SpeechDecision,
    ) -> bool {
        let key = dispatch(assignment);
        state.decision = Some(key);
        state.decided = true;
        session
            .io
            .control
            .as_ref()
            .and_then(async_channel::WeakSender::upgrade)
            .is_some_and(|out| {
                session.open()
                    && out
                        .try_send(ControlToBrain::SpeechDecision(key, decision))
                        .is_ok()
            })
    }

    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    fn refuse(&self, session: &Session) {
        session.close();
        if let Some(abort) = self.pair.get().and_then(|pair| pair.abort.upgrade()) {
            abort();
        }
    }

    pub(super) fn worker(
        self: &Arc<Self>,
        assignment_session: u64,
        event_failure: impl FnMut() + Send + 'static,
        exit: impl FnMut(bool) + Send + 'static,
    ) -> impl DrawingIo {
        Io {
            bridge: Arc::clone(self),
            session: assignment_session,
            event_failure,
            exit,
        }
    }
}

struct Io<F, E> {
    bridge: Arc<Narrated>,
    session: u64,
    event_failure: F,
    exit: E,
}

impl<F: FnMut() + Send + 'static, E: FnMut(bool) + Send + 'static> DrawingIo for Io<F, E> {
    fn requires_narration(&self) -> bool {
        true
    }

    fn view(&mut self) -> Result<Option<ViewLevel>, ()> {
        let mut state = self.bridge.state.lock().map_err(|_| ())?;
        state.worker.get_or_insert_with(thread::current);
        Ok(state.view.filter(|view| {
            view.binding.assignment.session == self.session
                && !self.bridge.stopped(view.binding.assignment.run)
        }))
    }

    fn picture(&mut self, key: Binding, level: Option<PictureLevel>) {
        if let Ok(mut state) = self.bridge.state.lock()
            && state.assignment == Some(key.assignment)
            && !self.bridge.stopped(key.assignment.run)
        {
            state.picture = level;
        }
    }

    fn owner_exited(&mut self, exit: OwnerExit) {
        let refused = exit == OwnerExit::ReportRefused;
        if refused {
            (self.event_failure)();
        }
        (self.exit)(refused);
    }

    fn ordered(&mut self, event: Ordered) -> bool {
        self.bridge.send.try_send(event).is_ok()
    }
}

#[cfg(all(test, windows, debug_assertions))]
mod tests {
    use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId, StepRef};

    use super::*;

    #[test]
    fn stopped_assignment_loses_authority_but_keeps_its_own_decision_receipt() {
        let bridge = Narrated::new();
        let old = Assignment {
            session: 1,
            output: OutputGeneration(4),
            run: RunId(7),
            segment: SegmentId(0),
            step: StepRef {
                scene: SceneId(2),
                index: 0,
            },
        };
        let new = Assignment {
            run: RunId(8),
            ..old
        };
        assert!(bridge.promised(old));
        assert!(bridge.announce(old));
        let receipt = dispatch(old);
        bridge.state.lock().unwrap().decision = Some(receipt);
        bridge.stop(old.run);
        assert_eq!(bridge.assignment(draw_key(old)), Err(()));
        assert!(bridge.promised(new));
        assert!(bridge.announce(new));
        assert!(!bridge.taken(old.session + 1, receipt));
        assert!(bridge.taken(old.session, receipt));
        assert!(!bridge.taken(old.session, receipt));
        assert_eq!(bridge.assignment(draw_key(new)), Ok(Some(new)));
    }

    #[test]
    fn wire_replacement_fences_only_the_victim_and_keeps_its_exact_metadata() {
        for marker_first in [false, true] {
            let bridge = Narrated::new();
            let (old, successor) = (SceneId(2), SceneId(3));
            let promised = Assignment {
                session: 1,
                output: OutputGeneration(4),
                run: RunId(7),
                segment: SegmentId(0),
                step: StepRef {
                    scene: old,
                    index: 0,
                },
            };
            let old_key = draw_key(promised);
            let next_key = DrawKey {
                scene: successor,
                ..old_key
            };
            if !marker_first {
                assert!(bridge.promised(promised));
                assert!(bridge.announce(promised));
                assert!(!bridge.replaced(&[old, old], successor));
                assert_eq!(bridge.assignment(old_key), Ok(Some(promised)));
            }
            assert!(bridge.replaced(&[old], successor));
            if marker_first {
                assert!(bridge.promised(promised));
                assert!(bridge.announce(promised));
            }
            assert!(bridge.unassigned(next_key));
            assert_eq!(bridge.assignment(old_key), Err(()));
            assert_eq!(bridge.assignment(next_key), Ok(None));
        }
    }
}
