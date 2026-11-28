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

#[cfg(all(windows, debug_assertions))]
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Runs once on this thread just before its next Allow takes the output gate.
    pub(super) static BEFORE_ALLOW: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

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
    entries: Vec<Entry>,
    absences: Absences,
    /// Scenes the data reader saw begin, or open in a recovered snapshot, and not yet retire.
    live: Vec<SceneId>,
    wake: Option<Arc<RunWake>>,
    handed_off: bool,
    view: Option<ViewLevel>,
    unacked: Option<(u64, SpeechDispatch)>,
    worker_ended: bool,
    worker: Option<(RunId, Thread)>,
}

/// One promised step of the run, in segment order.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent step fact the bridge reconciles"
)]
struct Entry {
    assignment: Assignment,
    announced: bool,
    narrated: bool,
    retired: bool,
    picture: Option<PictureLevel>,
    applied: Option<Binding>,
    decided: Option<SpeechDecision>,
    sent: bool,
    ended: Option<Binding>,
    fenced: bool,
    settled: bool,
}

impl Entry {
    fn finished(&self) -> bool {
        self.settled
            || self.fenced
            || self.ended.is_some()
            || self.decided == Some(SpeechDecision::Discard)
    }
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
            let pending =
                |(lane, at): (RetirementLane, NonZeroU64)| at.get() > consumed[lane.slot()];
            self.keys
                .retain(|entry| entry.retired_by.is_none_or(pending));
            self.reclaimed = consumed;
        }
    }
}

impl State {
    fn entry(&mut self, assignment: Assignment) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.assignment == assignment)
    }

    /// The lowest announced step that hasn't settled; only it is narrated or answers lookup.
    fn head(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| !e.finished())
            .filter(|&at| self.entries[at].announced)
    }

    /// Absence keys held, plus the one each step that can still fall back reserved.
    fn reserved(&self) -> usize {
        self.absences.keys.len()
            + self
                .entries
                .iter()
                .filter(|e| !e.sent && !e.settled && !e.retired)
                .count()
    }

    /// Exact `RunEnd` or worker exit: no decision that hasn't gone out is sent any more. A step
    /// nobody bound and nothing stopped or removed falls back to its authored timing, in the
    /// absence slot it reserved when it was promised.
    fn settle(&mut self, consumed: [u64; 2], stopped: impl Fn(RunId) -> bool) {
        let bound = self.view.map(|view| view.binding.assignment);
        let Self {
            entries, absences, ..
        } = self;
        absences.reclaim(consumed);
        let stopped = entries.first().is_some_and(|e| stopped(e.assignment.run));
        for entry in entries.iter_mut().filter(|e| !e.sent && !e.settled) {
            entry.settled = true;
            if !stopped && !entry.retired && bound != Some(entry.assignment) {
                absences.keys.push(Absence {
                    key: draw_key(entry.assignment),
                    retired_by: None,
                });
            }
        }
    }

    fn picture_for(&self, view: Option<ViewLevel>) -> Option<PictureLevel> {
        let view = view?;
        let entry = self
            .entries
            .iter()
            .find(|e| e.assignment == view.binding.assignment)?;
        let ended = entry
            .ended
            .or((self.worker_ended || entry.settled).then_some(view.binding));
        ended.map_or(entry.picture, |binding| {
            let pending_onset = view.binding == binding
                && view.view == View::Eligible
                && entry.picture.is_some_and(|level| {
                    level.binding == binding
                        && matches!(level.state, PictureState::Owned(Picture::At(_)))
                });
            if pending_onset {
                entry.picture
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

/// Every visible step's authored fallback, held until its scene's retirement is consumed.
const FALLBACKS: usize = MAX_VISIBLE_SCENES * MAX_STEPS;

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
        if let Ok(mut state) = self.state.lock()
            && state
                .entries
                .first()
                .is_some_and(|e| e.assignment.run == run)
        {
            self.wire_ended.store(run.0, Ordering::Release);
            state.settle(self.consumed(), |run| self.stopped(run));
            if self.stopped(run) && !state.handed_off {
                state.handed_off = true;
                self.unpark_reader();
            }
            self.notify(&state);
        }
    }

    fn unpark_reader(&self) {
        if let Some(reader) = self.pair.get().and_then(|pair| pair.reader.get()) {
            reader.unpark();
        }
    }

    /// The run's speaker input exists now, so a deferred narration can be offered again.
    pub(super) fn input_installed(&self) {
        if let Ok(state) = self.state.lock() {
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

    /// `Ok(false)` waits for the previous run's handoff, or for an ordered retirement to free the
    /// absence slot the promise reserves.
    pub(super) fn can_promise(&self, run: RunId) -> Result<bool, ()> {
        if self.closing() {
            return Err(());
        }
        let mut state = self.state.lock().map_err(|_| ())?;
        let same = match state.entries.first().map(|e| e.assignment.run) {
            Some(old) if old == run => true,
            Some(old) => {
                if !(run < old
                    || self.stopped(old)
                    || self.wire_ended.load(Ordering::Acquire) != old.0
                    || state.handed_off)
                {
                    return Ok(false);
                }
                false
            }
            None => false,
        };
        if self.stopped(run) {
            return Ok(true);
        }
        state.absences.reclaim(self.consumed());
        let reserved = if same {
            state.reserved()
        } else {
            state.absences.keys.len()
        };
        if reserved < FALLBACKS {
            return Ok(true);
        }
        state
            .absences
            .keys
            .iter()
            .any(|key| key.retired_by.is_some())
            .then_some(false)
            .ok_or(())
    }

    pub(super) fn handoff(&self, session: &Session, presenter: &mut Presenter) {
        let run = self.current.load(Ordering::Acquire);
        if run == 0 || self.wire_ended.load(Ordering::Acquire) != run || !session.open() {
            return;
        }
        if let Ok(mut state) = self.try_state()
            && state
                .entries
                .first()
                .is_some_and(|e| e.assignment.run.0 == run)
            && !state.handed_off
            && (self.stopped(RunId(run))
                || state
                    .entries
                    .iter()
                    .all(|e| e.settled || e.sent && (e.finished() || state.worker_ended))
                    && state
                        .entries
                        .iter()
                        .all(|e| presenter.conductor_handoff(e.assignment)))
        {
            state.handed_off = true;
            self.unpark_reader();
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

    /// The last promised step, if any, has been handed off. Contention reads as busy.
    pub(super) fn idle(&self) -> bool {
        self.state
            .try_lock()
            .is_ok_and(|state| state.entries.is_empty() || state.handed_off)
    }

    /// `idle` that waits out contention, where a refusal would end the pair.
    pub(super) fn wait_idle(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.entries.is_empty() || state.handed_off)
    }

    /// Each promised step with whether it was narrated and fenced.
    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the crate's own tests call it")
    )]
    pub(super) fn steps(&self) -> Vec<(Assignment, bool, bool)> {
        let state = self.state.lock().expect("an unpoisoned bridge");
        state
            .entries
            .iter()
            .map(|e| (e.assignment, e.narrated, e.fenced))
            .collect()
    }

    /// Whether the worker applied `assignment`'s binding, or a decision already took it.
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn applied(&self, assignment: Assignment) -> bool {
        let state = self.state.lock().expect("an unpoisoned bridge");
        state
            .entries
            .iter()
            .any(|e| e.assignment == assignment && (e.applied.is_some() || e.sent))
    }

    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the crate's own tests call it")
    )]
    pub(super) fn reserved(&self) -> usize {
        self.state.lock().expect("an unpoisoned bridge").reserved()
    }

    #[cfg(any(test, feature = "test-support"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the crate's own tests call it")
    )]
    pub(super) fn hold_state(&self) -> impl Sized + '_ {
        self.state.lock().expect("an unpoisoned bridge")
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
                match state.entries.first().map(|e| e.assignment.run) {
                    Some(run) if run == assignment.run => {
                        if state.entries.iter().any(|e| {
                            draw_key(e.assignment) == key
                                || e.assignment.segment >= assignment.segment
                        }) {
                            return false;
                        }
                    }
                    Some(old) => {
                        if old > assignment.run
                            || (!self.stopped(old)
                                && (self.wire_ended.load(Ordering::Acquire) != old.0
                                    || !state.handed_off))
                        {
                            return false;
                        }
                        *state = State {
                            absences: std::mem::take(&mut state.absences),
                            live: std::mem::take(&mut state.live),
                            unacked: state.unacked,
                            worker: state.worker.take(),
                            ..State::default()
                        };
                    }
                    None => {}
                }
                if state.entries.is_empty() {
                    state.wake = Some(Arc::new(RunWake {
                        run: assignment.run,
                        pending: AtomicBool::new(false),
                    }));
                    self.current.store(assignment.run.0, Ordering::Release);
                }
                let retired = !state.live.contains(&key.scene);
                state.entries.push(Entry {
                    assignment,
                    announced: false,
                    narrated: false,
                    retired,
                    picture: None,
                    applied: None,
                    decided: None,
                    sent: false,
                    ended: None,
                    fenced: false,
                    settled: false,
                });
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
            state.absences.reclaim(self.consumed());
            for entry in &mut state.entries {
                entry.retired |= scenes.contains(&entry.assignment.step.scene);
            }
            state.live.retain(|scene| !scenes.contains(scene));
            for entry in &mut state.absences.keys {
                if scenes.contains(&entry.key.scene) {
                    entry.retired_by.get_or_insert((lane, ordinal));
                }
            }
            true
        })
    }

    /// A scene began, or its recovered snapshot opened, at most as many as can be visible.
    pub(super) fn began(&self, scene: SceneId) -> Result<(), ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        if state.live.len() == MAX_VISIBLE_SCENES || state.live.contains(&scene) {
            return Err(());
        }
        state.live.push(scene);
        Ok(())
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
        if state.entries.iter().any(|e| draw_key(e.assignment) == key)
            || state.absences.contains(key)
        {
            return Err(());
        }
        if !state.live.contains(&key.scene) {
            return Ok(true);
        }
        if state.reserved() >= FALLBACKS {
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
            let Some(entry) = state.entries.iter_mut().find(|e| !e.announced) else {
                return false;
            };
            if entry.assignment != assignment {
                return false;
            }
            entry.announced = true;
            self.notify(&state);
            true
        })
    }

    pub(super) fn taken(&self, session: u64, key: SpeechDispatch) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            if state
                .unacked
                .take_if(|debt| *debt == (session, key))
                .is_none()
            {
                return false;
            }
            self.notify(&state);
            true
        })
    }

    fn assignment(&self, key: DrawKey) -> Result<Option<Assignment>, ()> {
        if self.stopped(key.run) {
            return Err(());
        }
        let state = self.try_state()?;
        if state.absences.contains(key) {
            return Ok(None);
        }
        let head = state.head();
        state
            .entries
            .iter()
            .enumerate()
            .find(|(at, e)| {
                draw_key(e.assignment) == key
                    && !e.retired
                    && !state.handed_off
                    && (head == Some(*at)
                        || state
                            .view
                            .is_some_and(|view| view.binding.assignment == e.assignment))
            })
            .map(|(_, e)| Some(e.assignment))
            .ok_or(())
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

    /// `Ok(false)` waits: contention retries on the busy path, a missing input on its install, and
    /// a run that stopped or ended first publishes nothing.
    fn offer_gate(
        &self,
        link: &LinkHandle,
        command: Command,
        before: impl FnOnce() -> bool,
    ) -> Result<bool, ()> {
        #[cfg(all(windows, debug_assertions))]
        #[cfg(any(test, feature = "test-support"))]
        if matches!(command, Command::Allow(_))
            && let Some(race) = BEFORE_ALLOW.take()
        {
            race();
        }
        match link.shared.output.narrated_gate(command, before) {
            Ok(sent) => Ok(sent),
            Err(Denied::Busy) => {
                self.busy.store(true, Ordering::Release);
                Ok(false)
            }
            Err(Denied::Opening) => Ok(false),
            Err(_) => Err(()),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one pass reconciles removal, the presenter's view, activation and decisions"
    )]
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
        self.drain_reports(session, presenter);
        let mut view = presenter.conductor_view(now);
        let picture = {
            let Ok(mut state) = self.try_state() else {
                return Ok(());
            };
            let Some(run) = state.entries.first().map(|e| e.assignment.run).filter(|_| {
                session.open()
                    && state.entries[0].assignment.session == session.generation()
                    && !state.handed_off
            }) else {
                return Ok(());
            };
            if self.stopped(run) {
                if let Some(level) = state.view.take() {
                    presenter.conductor_fence(level.binding);
                }
                return Ok(());
            }
            for entry in &mut state.entries {
                entry.retired |=
                    presenter.conductor_removed(session.generation(), entry.assignment.step.scene);
            }
            if let Some(level) = view
                && state
                    .entry(level.binding.assignment)
                    .is_some_and(|e| e.retired)
            {
                presenter.conductor_fence(level.binding);
                view = None;
            }
            let ended = self.wire_ended.load(Ordering::Acquire) == run.0;
            for at in 0..state.entries.len() {
                let entry = &state.entries[at];
                if !entry.retired || entry.decided.is_some() || entry.settled {
                    continue;
                }
                if entry.narrated
                    && !self.offer_gate(link, Command::RevokeSource(entry.assignment), || true)?
                {
                    return Ok(());
                }
                state.entries[at].decided = Some(SpeechDecision::Discard);
            }
            if ended {
                let picture = state.picture_for(view);
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
                if let Some((_, owner)) = state.worker.as_ref().filter(|(worker, _)| *worker == run)
                {
                    owner.unpark();
                }
            }
            if let Some(at) = state.head() {
                let head = state.entries[at].assignment;
                if !state.entries[at].narrated {
                    if !self.offer_gate(link, Command::Narrate(head), || true)? {
                        return Ok(());
                    }
                    state.entries[at].narrated = true;
                }
                let allow = state.entries[at].applied.filter(|&binding| {
                    view.is_some_and(|view| view.binding == binding && view.view == View::Eligible)
                });
                if let Some(binding) = allow
                    && state.unacked.is_none()
                    && state.entries[..at].iter().all(|e| e.sent)
                {
                    if !self.offer_gate(link, Command::Allow(binding), || {
                        Self::decide(&mut state, at, session, SpeechDecision::Allow)
                    })? {
                        return Ok(());
                    }
                    state.entries[at].applied = None;
                }
            }
            if state.unacked.is_none()
                && let Some(at) = state.entries.iter().position(|e| !e.sent && !e.settled)
                && state.entries[at].announced
                && state.entries[at].decided == Some(SpeechDecision::Discard)
                && !Self::decide(&mut state, at, session, SpeechDecision::Discard)
            {
                return Err(());
            }
            state.picture_for(view)
        };
        presenter.conductor_picture(Ok(picture), now);
        Ok(())
    }

    fn drain_reports(&self, session: &Session, presenter: &mut Presenter) {
        while let Ok(mut state) = self.try_state() {
            if !session.open()
                || state
                    .entries
                    .first()
                    .is_none_or(|e| e.assignment.session != session.generation())
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
            if self.stopped(binding.assignment.run) {
                continue;
            }
            let Some(entry) = state.entry(binding.assignment) else {
                continue;
            };
            entry.applied = None;
            match event {
                Ordered::Applied(_) => entry.applied = Some(binding),
                Ordered::Fenced(_) => {
                    entry.fenced = true;
                    entry.decided.get_or_insert(SpeechDecision::Discard);
                    drop(state);
                    presenter.conductor_fence(binding);
                }
                Ordered::Ended { .. } => entry.ended = Some(binding),
            }
        }
    }

    /// Sends entry `at`'s decision, the pair's one unacknowledged decision.
    fn decide(state: &mut State, at: usize, session: &Session, decision: SpeechDecision) -> bool {
        let entry = &mut state.entries[at];
        let key = dispatch(entry.assignment);
        entry.decided = Some(decision);
        entry.sent = true;
        state.unacked = Some((entry.assignment.session, key));
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
            && !self.bridge.stopped(key.assignment.run)
            && let Ok(mut state) = self.bridge.state.lock()
            && let Some(entry) = state.entry(key.assignment)
        {
            entry.picture = level;
            self.bridge.notify(&state);
        }
    }

    fn owner_exited(&mut self, exit: OwnerExit) {
        let refused = exit == OwnerExit::ReportRefused;
        if refused {
            (self.event_failure)();
        } else if let Ok(mut state) = self.bridge.state.lock()
            && state
                .entries
                .first()
                .is_some_and(|e| e.assignment.run == self.run)
            && !self.bridge.stopped(self.run)
        {
            state.worker_ended = true;
            state.settle(self.bridge.consumed(), |run| self.bridge.stopped(run));
            self.bridge.notify(&state);
        }
        (self.exit)(refused);
    }

    fn ordered(&mut self, event: Ordered) -> bool {
        let Ok(state) = self.bridge.state.lock() else {
            return false;
        };
        if state
            .entries
            .first()
            .is_none_or(|e| e.assignment.run != self.run)
            || self.bridge.stopped(self.run)
        {
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
#[path = "narrated_tests.rs"]
mod tests;
