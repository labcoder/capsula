use std::num::NonZeroU64;
use std::sync::{
    Arc, Mutex, OnceLock, TryLockError, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, Thread};
use std::time::Instant;

use kumochi_protocol::message::{ControlToBrain, SpeechDecision};
use kumochi_protocol::message::{MAX_VISIBLE_SCENES, RunId, SceneId, SpeechDispatch};
use kumochi_scene::limits::MAX_STEPS;

use super::{LinkEvent, LinkHandle, Session, SessionIo, Shared, output::Denied};
use crate::presenter::{DrawKey, Presenter, RetirementLane};
use crate::speaker::conductor::{Assignment, Binding, PictureLevel, ViewLevel};
use crate::speaker::conductor::{Picture, PictureState, RouteStamp, View};
use crate::speaker::queued::Command;
use crate::speaker::queued::{DrawingIo, Ordered, OwnerExit};

#[derive(Debug)]
struct Pair {
    io: Weak<SessionIo>,
    shared: Weak<Shared>,
    closing: Weak<AtomicBool>,
    reader: Arc<OnceLock<Thread>>,
    events: async_channel::WeakSender<LinkEvent>,
}

#[derive(Debug)]
struct RunWake {
    run: RunId,
    pending: AtomicBool,
}

/// One outstanding notification for a run, never authority to install a picture.
pub struct Wake {
    io: Weak<SessionIo>,
    run: Arc<RunWake>,
}

impl Wake {
    pub(super) fn take(self, io: &Arc<SessionIo>) -> bool {
        self.run.pending.store(false, Ordering::Release);
        self.io.as_ptr() == Arc::as_ptr(io)
            && io.narrated.as_ref().is_some_and(|bridge| {
                bridge.current.load(Ordering::Acquire) == self.run.run.0
                    && !bridge.stopped(self.run.run)
            })
    }
}

#[derive(Debug)]
pub(crate) struct Narrated {
    pub(super) origin: Instant,
    stopped: AtomicU64,
    current: AtomicU64,
    wire_ended: AtomicU64,
    consumed_retirements: [AtomicU64; 2],
    busy: AtomicBool,
    retrying: AtomicBool,
    state: Mutex<State>,
    send: async_channel::Sender<Ordered>,
    reports: async_channel::Receiver<Ordered>,
    pair: OnceLock<Pair>,
}

#[derive(Debug, Default)]
struct State {
    promised: Option<Assignment>,
    replaced_before_promise: [Option<SceneId>; MAX_VISIBLE_SCENES],
    retired: bool,
    absences: Absences,
    assignment: Option<Assignment>,
    wake: Option<Arc<RunWake>>,
    handed_off: bool,
    view: Option<ViewLevel>,
    picture: Option<PictureLevel>,
    applied: Option<Binding>,
    decision: Option<SpeechDispatch>,
    retired_decision: Option<(u64, SpeechDispatch)>,
    decided: Option<SpeechDecision>,
    ended: Option<Binding>,
    worker_ended: bool,
    worker: Option<(RunId, Thread)>,
}

#[derive(Debug, Default)]
struct Absences {
    keys: Vec<Absence>,
    reclaimed: [u64; 2],
}

#[derive(Debug)]
struct Absence {
    key: DrawKey,
    retired_by: Option<(RetirementLane, NonZeroU64)>,
}

impl Absences {
    fn contains(&self, key: DrawKey) -> bool {
        self.keys.iter().any(|entry| entry.key == key)
    }

    fn reclaim(&mut self, consumed: [u64; 2]) {
        if consumed != self.reclaimed {
            self.keys.retain(|entry| {
                entry
                    .retired_by
                    .is_none_or(|(lane, at)| at.get() > consumed[lane.slot()])
            });
            self.reclaimed = consumed;
        }
    }
}

impl State {
    fn picture_for(&self, view: Option<ViewLevel>) -> Option<PictureLevel> {
        let ended = self.ended.or_else(|| {
            view.filter(|view| {
                self.worker_ended && Some(view.binding.assignment) == self.assignment
            })
            .map(|view| view.binding)
        });
        ended.map_or(self.picture, |binding| {
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
    pub(super) fn new() -> Self {
        let (send, reports) = async_channel::bounded(4);
        Self {
            origin: Instant::now(),
            stopped: AtomicU64::new(0),
            current: AtomicU64::new(0),
            wire_ended: AtomicU64::new(0),
            consumed_retirements: Default::default(),
            busy: AtomicBool::new(false),
            retrying: AtomicBool::new(false),
            state: Mutex::default(),
            send,
            reports,
            pair: OnceLock::new(),
        }
    }

    pub(super) fn register_pair(
        &self,
        io: &Arc<SessionIo>,
        shared: &Arc<Shared>,
        closing: &Arc<AtomicBool>,
        reader: &Arc<OnceLock<Thread>>,
        events: &async_channel::Sender<LinkEvent>,
    ) -> bool {
        events.capacity().is_none()
            && self
                .pair
                .set(Pair {
                    io: Arc::downgrade(io),
                    shared: Arc::downgrade(shared),
                    closing: Arc::downgrade(closing),
                    reader: Arc::clone(reader),
                    events: events.downgrade(),
                })
                .is_ok()
    }

    fn notify(&self, state: &State) {
        if let Some(pair) = self.pair.get()
            && let Some(run) = &state.wake
            && !self.stopped(run.run)
            && pair
                .closing
                .upgrade()
                .is_some_and(|closing| !closing.load(Ordering::Acquire))
            && let Some(out) = pair.events.upgrade()
            && !run.pending.swap(true, Ordering::AcqRel)
        {
            let _offered = out.try_send(LinkEvent::NarrationReady(Wake {
                io: Weak::clone(&pair.io),
                run: Arc::clone(run),
            }));
        }
    }

    pub(super) fn run_end(&self, run: RunId) {
        if let Ok(state) = self.state.lock()
            && state.promised.is_some_and(|a| a.run == run)
        {
            self.wire_ended.store(run.0, Ordering::Release);
            self.notify(&state);
        }
    }

    fn closing(&self) -> bool {
        self.pair.get().is_some_and(|pair| {
            pair.closing
                .upgrade()
                .is_none_or(|closing| closing.load(Ordering::Acquire))
        })
    }

    pub(super) fn can_promise(&self, run: RunId) -> Result<bool, ()> {
        if self.closing() {
            return Err(());
        }
        let state = self.state.lock().map_err(|_| ())?;
        Ok(state.promised.is_none_or(|old| {
            run <= old.run
                || self.stopped(old.run)
                || self.wire_ended.load(Ordering::Acquire) != old.run.0
                || state.handed_off
        }))
    }

    pub(super) fn handoff(&self, session: &Session, presenter: &mut Presenter) {
        let run = self.current.load(Ordering::Acquire);
        if run == 0 || self.wire_ended.load(Ordering::Acquire) != run || !session.open() {
            return;
        }
        if let Ok(mut state) = self.try_state()
            && state.promised.is_some_and(|a| a.run.0 == run)
            && !state.handed_off
            && (self.stopped(RunId(run))
                || state.retired && (state.assignment.is_none() || state.decided.is_some())
                || (state.decided != Some(SpeechDecision::Allow)
                    || state.ended.is_some()
                    || state.worker_ended)
                    && !state.retired
                    && state
                        .assignment
                        .is_none_or(|a| presenter.conductor_handoff(a)))
        {
            state.handed_off = true;
            if let Some(reader) = self.pair.get().and_then(|pair| pair.reader.get()) {
                reader.unpark();
            }
        }
    }

    fn try_state(&self) -> Result<std::sync::MutexGuard<'_, State>, ()> {
        self.state.try_lock().map_err(|error| {
            if matches!(error, TryLockError::WouldBlock) {
                self.busy.store(true, Ordering::Release);
            }
        })
    }

    pub(super) fn retry(
        self: &Arc<Self>,
        session: &Session,
    ) -> Option<impl FnOnce() + Send + use<>> {
        if !session.open()
            || !self.busy.load(Ordering::Acquire)
            || self.retrying.swap(true, Ordering::AcqRel)
        {
            return None;
        }
        self.busy.store(false, Ordering::Release);
        let bridge = Arc::clone(self);
        Some(move || {
            if let Some(shared) = bridge.pair.get().and_then(|pair| pair.shared.upgrade()) {
                shared.output.wait_narrated_gate();
            }
            drop(bridge.state.lock());
        })
    }

    pub(super) fn resume(&self) -> bool {
        self.retrying.swap(false, Ordering::AcqRel)
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
                state.absences.reclaim(self.consumed());
                if state.absences.contains(key) {
                    return false;
                }
                if let Some(old) = state.promised {
                    if old.run >= assignment.run
                        || (!self.stopped(old.run)
                            && (self.wire_ended.load(Ordering::Acquire) != old.run.0
                                || !state.handed_off))
                    {
                        return false;
                    }
                    let debt = state.decision.map(|key| (old.session, key));
                    if debt.is_some() && state.retired_decision.is_some() {
                        return false;
                    }
                    *state = State {
                        absences: std::mem::take(&mut state.absences),
                        replaced_before_promise: state.replaced_before_promise,
                        retired_decision: debt.or(state.retired_decision),
                        worker: state.worker.take(),
                        ..State::default()
                    };
                }
                state.retired = state.replaced_before_promise.contains(&Some(key.scene));
                state.promised = Some(assignment);
                state.wake = Some(Arc::new(RunWake {
                    run: assignment.run,
                    pending: AtomicBool::new(false),
                }));
                self.current.store(assignment.run.0, Ordering::Release);
                true
            })
    }

    fn consumed(&self) -> [u64; 2] {
        self.consumed_retirements
            .each_ref()
            .map(|at| at.load(Ordering::Acquire))
    }

    pub(super) fn retiring(
        &self,
        scenes: &[SceneId],
        successor: Option<SceneId>,
        lane: RetirementLane,
        ordinal: NonZeroU64,
    ) -> bool {
        if scenes.len() > MAX_VISIBLE_SCENES
            || successor.is_some_and(|successor| {
                scenes.is_empty()
                    || scenes.contains(&successor)
                    || scenes
                        .iter()
                        .enumerate()
                        .any(|(at, id)| scenes[..at].contains(id))
            })
        {
            return false;
        }
        self.state.lock().is_ok_and(|mut state| {
            if successor.is_some() {
                if let Some(assignment) = state.promised.filter(|a| !self.stopped(a.run)) {
                    state.retired |= scenes.contains(&assignment.step.scene);
                } else {
                    state.replaced_before_promise =
                        std::array::from_fn(|at| scenes.get(at).copied());
                }
            }
            state.absences.reclaim(self.consumed());
            for entry in &mut state.absences.keys {
                if scenes.contains(&entry.key.scene) {
                    entry.retired_by.get_or_insert((lane, ordinal));
                }
            }
            true
        })
    }

    fn retirement_consumed(&self, lane: RetirementLane) {
        self.consumed_retirements[lane.slot()].fetch_add(1, Ordering::Release);
        if let Some(reader) = self.pair.get().and_then(|pair| pair.reader.get()) {
            reader.unpark();
        }
    }

    pub(super) fn unassigned(&self, key: DrawKey) -> Result<bool, ()> {
        if !valid_key(key) || self.closing() {
            return Err(());
        }
        let mut state = self.state.lock().map_err(|_| ())?;
        state.absences.reclaim(self.consumed());
        if state.promised.is_some_and(|a| draw_key(a) == key) || state.absences.contains(key) {
            return Err(());
        }
        if state.absences.keys.len() == MAX_VISIBLE_SCENES * MAX_STEPS {
            return state
                .absences
                .keys
                .iter()
                .any(|key| key.retired_by.is_some())
                .then_some(false)
                .ok_or(());
        }
        state.absences.keys.push(Absence {
            key,
            retired_by: None,
        });
        Ok(true)
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
            self.notify(&state);
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

    fn assignment(&self, key: DrawKey) -> Result<Option<Assignment>, ()> {
        if self.stopped(key.run) {
            return Err(());
        }
        let state = self.try_state()?;
        if let Some(assignment) = state
            .assignment
            .filter(|&a| !state.retired && !state.handed_off && draw_key(a) == key)
        {
            Ok(Some(assignment))
        } else {
            state.absences.contains(key).then_some(None).ok_or(())
        }
    }

    pub(crate) fn attach(self: &Arc<Self>, presenter: &mut Presenter) {
        let bridge = Arc::clone(self);
        let retire = Arc::clone(self);
        presenter.start_conduction(
            self.origin,
            move |key| bridge.assignment(key),
            move |lane| retire.retirement_consumed(lane),
        );
    }

    fn offer_gate(
        &self,
        link: &LinkHandle,
        command: Command,
        before: impl FnOnce() -> bool,
    ) -> Result<bool, ()> {
        match link.shared.output.narrated_gate(command, before) {
            Ok(()) => Ok(true),
            Err(Denied::Busy) => {
                self.busy.store(true, Ordering::Release);
                Ok(false)
            }
            Err(_) => Err(()),
        }
    }

    pub(crate) fn update(
        &self,
        link: &LinkHandle,
        session: &Session,
        presenter: &mut Presenter,
        now: Instant,
    ) -> Result<(), ()> {
        if !self.pair.get().is_some_and(|pair| {
            pair.io.as_ptr() == Arc::as_ptr(&session.io)
                && pair.shared.as_ptr() == Arc::as_ptr(&link.shared)
        }) {
            return Err(());
        }
        self.drain_reports(session, presenter)?;
        let view = presenter.conductor_view(now);
        let picture = {
            let Ok(mut state) = self.try_state() else {
                return Ok(());
            };
            let Some(assignment) = state.assignment.filter(|a| {
                session.open() && a.session == session.generation() && !state.handed_off
            }) else {
                return Ok(());
            };
            if self.stopped(assignment.run) {
                if let Some(level) = state.view.take() {
                    presenter.conductor_fence(level.binding);
                }
                return Ok(());
            }
            state.retired |=
                presenter.conductor_removed(session.generation(), assignment.step.scene);
            let removed = state.retired;
            let view = if removed {
                if let Some(level) = view {
                    presenter.conductor_fence(level.binding);
                }
                None
            } else {
                view
            };
            let ended = self.wire_ended.load(Ordering::Acquire) == assignment.run.0;
            if (state.view.is_none() || ended) && state.decided.is_none() && removed {
                if !self.offer_gate(link, Command::RevokeSource(assignment), || true)? {
                    return Ok(());
                }
                if !Self::decide(&mut state, session, assignment, SpeechDecision::Discard) {
                    return Err(());
                }
            }
            if ended {
                let picture = (!removed).then(|| state.picture_for(view)).flatten();
                drop(state);
                presenter.conductor_picture(Ok(picture), now);
                return Ok(());
            }
            let fenced = state.view.filter(|old| {
                old.view != View::Fenced
                    && view
                        .is_none_or(|next| next.view == View::Fenced || next.binding != old.binding)
            });
            if let Some(old) = fenced
                && !self.offer_gate(link, Command::Revoke(old.binding), || true)?
            {
                return Ok(());
            }
            if state.view != view {
                state.view = view;
                if let Some((_, owner)) = state
                    .worker
                    .as_ref()
                    .filter(|(run, _)| *run == assignment.run)
                {
                    owner.unpark();
                }
            }
            let allow = state.applied.filter(|&binding| {
                view.is_some_and(|view| view.binding == binding && view.view == View::Eligible)
            });
            if let Some(binding) = allow {
                if !self.offer_gate(link, Command::Allow(binding), || {
                    Self::decide(
                        &mut state,
                        session,
                        binding.assignment,
                        SpeechDecision::Allow,
                    )
                })? {
                    return Ok(());
                }
                state.applied = None;
            }
            state.picture_for(view)
        };
        presenter.conductor_picture(Ok(picture), now);
        Ok(())
    }

    fn drain_reports(&self, session: &Session, presenter: &mut Presenter) -> Result<(), ()> {
        while let Ok(mut state) = self.try_state() {
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
                    let accepted = state.decided.is_some()
                        || Self::decide(
                            &mut state,
                            session,
                            binding.assignment,
                            SpeechDecision::Discard,
                        );
                    drop(state);
                    presenter.conductor_fence(binding);
                    if !accepted {
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

    fn decide(
        state: &mut State,
        session: &Session,
        assignment: Assignment,
        decision: SpeechDecision,
    ) -> bool {
        let key = dispatch(assignment);
        state.decision = Some(key);
        state.decided = Some(decision);
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

    pub(super) fn worker(
        self: &Arc<Self>,
        run: RunId,
        event_failure: impl FnMut() + Send + 'static,
        exit: impl FnMut(bool) + Send + 'static,
    ) -> impl DrawingIo {
        Io {
            bridge: Arc::clone(self),
            run,
            event_failure,
            exit,
        }
    }
}

struct Io<F, E> {
    bridge: Arc<Narrated>,
    run: RunId,
    event_failure: F,
    exit: E,
}

impl<F: FnMut() + Send + 'static, E: FnMut(bool) + Send + 'static> DrawingIo for Io<F, E> {
    fn requires_narration(&self) -> bool {
        true
    }

    fn view(&mut self) -> Result<Option<ViewLevel>, ()> {
        let mut state = self.bridge.state.lock().map_err(|_| ())?;
        if state.worker.as_ref().is_none_or(|(run, _)| *run < self.run) {
            state.worker = Some((self.run, thread::current()));
        }
        Ok(state.view.filter(|view| {
            view.binding.assignment.run == self.run && !self.bridge.stopped(self.run)
        }))
    }

    fn picture(&mut self, key: Binding, level: Option<PictureLevel>) {
        if self.run == key.assignment.run
            && let Ok(mut state) = self.bridge.state.lock()
            && state.assignment == Some(key.assignment)
            && !self.bridge.stopped(key.assignment.run)
        {
            state.picture = level;
            self.bridge.notify(&state);
        }
    }

    fn owner_exited(&mut self, exit: OwnerExit) {
        let refused = exit == OwnerExit::ReportRefused;
        if refused {
            (self.event_failure)();
        } else if let Ok(mut state) = self.bridge.state.lock()
            && state.assignment.is_some_and(|a| a.run == self.run)
            && !self.bridge.stopped(self.run)
        {
            state.worker_ended = true;
            self.bridge.notify(&state);
        }
        (self.exit)(refused);
    }

    fn ordered(&mut self, event: Ordered) -> bool {
        let Ok(state) = self.bridge.state.lock() else {
            return false;
        };
        if state.assignment.is_none_or(|a| a.run != self.run) || self.bridge.stopped(self.run) {
            return true;
        }
        if self.bridge.send.try_send(event).is_err() {
            return false;
        }
        self.bridge.notify(&state);
        true
    }
}

#[cfg(all(test, windows, debug_assertions))]
mod tests {
    use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId, StepRef};

    use super::*;

    #[test]
    fn an_empty_contended_pass_rearms_once_and_only_on_its_pair() {
        use crate::link::fixture::{attended_queued_shared, connected};

        let pipe = attended_queued_shared(
            "narration-empty-rearm",
            Arc::new(Shared {
                narrated: true,
                ..Shared::default()
            }),
        );
        let session = connected(pipe.events.recv_blocking().unwrap());
        let other = Session::detached(session.generation(), session.incarnation());
        let bridge = session.io.narrated.as_ref().unwrap();
        let mut presenter = crate::presenter::fixture::presenter();
        presenter.connected(session.generation());
        session.attach_narration(&mut presenter);
        let held = bridge.state.lock().unwrap();
        assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
        let retry = session
            .retry_narration()
            .expect("contention needs a new scene pass");
        assert!(
            session.retry_narration().is_none(),
            "only one wait is in flight"
        );
        assert!(
            pipe.events.is_empty(),
            "no queued message can rescue the interrupted pass"
        );
        drop(held);
        retry();
        assert!(!other.resumes_narration(&session));
        assert!(session.resumes_narration(&session));
        assert!(!session.resumes_narration(&session));
        assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
        assert!(session.retry_narration().is_none());
        pipe.handle.end(&session);
        assert!(pipe.link.join().unwrap());
    }

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
    fn a_full_absence_table_waits_only_for_an_ordered_retirement() {
        let bridge = Narrated::new();
        let first = DrawKey {
            run: RunId(7),
            scene: SceneId(1),
            index: 0,
        };
        for scene in 1..=4 {
            for index in 0..64 {
                assert_eq!(
                    bridge.unassigned(DrawKey {
                        scene: SceneId(scene),
                        index,
                        ..first
                    }),
                    Ok(true)
                );
            }
        }
        let next = DrawKey {
            scene: SceneId(5),
            ..first
        };
        assert_eq!(bridge.unassigned(next), Err(()));
        assert!(bridge.retiring(
            &[first.scene],
            None,
            RetirementLane::Scenes,
            NonZeroU64::MIN
        ));
        assert_eq!(bridge.unassigned(next), Ok(false));
        assert_eq!(bridge.assignment(first), Ok(None));
        assert_eq!(bridge.unassigned(first), Err(()));

        bridge.retirement_consumed(RetirementLane::Scenes);
        assert_eq!(bridge.unassigned(next), Ok(true));
        assert_eq!(bridge.assignment(first), Err(()));
        assert_eq!(bridge.assignment(next), Ok(None));
        assert_eq!(
            bridge.unassigned(DrawKey {
                scene: SceneId(2),
                ..first
            }),
            Err(())
        );
    }

    #[test]
    fn an_applied_removal_does_not_acknowledge_a_held_zero_cut() {
        let bridge = Narrated::new();
        let cut = DrawKey {
            run: RunId(7),
            scene: SceneId(1),
            index: 0,
        };
        let removed = DrawKey {
            scene: SceneId(2),
            ..cut
        };
        let live = DrawKey {
            scene: SceneId(3),
            ..cut
        };
        for key in [cut, removed, live] {
            assert_eq!(bridge.unassigned(key), Ok(true));
        }
        assert!(bridge.retiring(&[cut.scene], None, RetirementLane::Cut, NonZeroU64::MIN));
        assert!(bridge.retiring(
            &[removed.scene],
            None,
            RetirementLane::Scenes,
            NonZeroU64::MIN
        ));
        bridge.retirement_consumed(RetirementLane::Scenes);
        assert_eq!(bridge.unassigned(live), Err(()));
        assert_eq!(bridge.assignment(cut), Ok(None));
        assert_eq!(bridge.assignment(removed), Err(()));
        bridge.retirement_consumed(RetirementLane::Cut);
        assert_eq!(bridge.unassigned(live), Err(()));
        assert_eq!(bridge.assignment(cut), Err(()));
        assert_eq!(bridge.assignment(live), Ok(None));
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
                assert!(!bridge.retiring(
                    &[old, old],
                    Some(successor),
                    RetirementLane::Scenes,
                    NonZeroU64::MIN
                ));
                assert_eq!(bridge.assignment(old_key), Ok(Some(promised)));
            }
            assert!(bridge.retiring(
                &[old],
                Some(successor),
                RetirementLane::Scenes,
                NonZeroU64::MIN
            ));
            if marker_first {
                assert!(bridge.promised(promised));
                assert!(bridge.announce(promised));
            }
            assert_eq!(bridge.unassigned(next_key), Ok(true));
            assert_eq!(bridge.assignment(old_key), Err(()));
            assert_eq!(bridge.assignment(next_key), Ok(None));
        }
    }
}
