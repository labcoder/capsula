//! The brain's own layout of every visible scene, measured for the current pair, so a step's
//! paint work is counted before its write. A step is prepared off the coordinator thread while
//! its admission seals the reducer, as a write does.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kumochi_protocol::message::SceneId;
use kumochi_scene::display::{PaintWork, ProfileMeasure, TextMetricsV1};
use kumochi_scene::layout::{
    DEFAULT_CANVAS, LayoutState, PrepareError, PrepareInput, SurfaceGeometry, prepare, recover,
};
use kumochi_scene::theme::Theme;
use kumochi_scene::{Begin, ErrorCode, FieldPath, SceneEvent, SceneState, StepError};

use super::{CallError, Drawing, Input, Output, Pend, Pending, StepWait, Wait, Write};
use crate::coordinator::{Effect, Stale};

/// The shell's drawing surface.
pub(super) const SURFACE: SurfaceGeometry = SurfaceGeometry::Overlay {
    default_canvas: DEFAULT_CANVAS,
};

/// The most the brain keeps of its scenes' wide layouts: a step that would take it past this is
/// written uncounted, and its scene gives its layouts back.
// shortcut: 16 MiB is a guess, checked against one scene at the paint budgets with eight unshown steps; size it from use.
pub(super) const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// A scene's wide layouts, as the shell holds them: the shown one and every committed step's,
/// each with its resident bytes.
pub(super) struct Paint {
    /// The last shown layout.
    rest: (LayoutState, usize),
    /// Committed steps' layouts not shown yet, by lifetime index.
    // shortcut: a full layout per unshown step, up to MAX_RETAINED_BYTES across scenes; keep only each step's charge and lay the cut step out again if scenes go uncounted in use.
    unshown: VecDeque<(u32, LayoutState, usize)>,
}

impl Paint {
    /// A layout the shell shows as it is.
    pub(super) fn at_rest(rest: LayoutState) -> Self {
        let bytes = rest.resident_bytes();
        Self {
            rest: (rest, bytes),
            unshown: VecDeque::new(),
        }
    }

    /// A new scene, shown `rest` empty, whose first step `index` isn't shown yet.
    pub(super) fn begun(
        rest: LayoutState,
        index: u32,
        (layout, bytes): (LayoutState, usize),
    ) -> Self {
        let mut paint = Self::at_rest(rest);
        paint.unshown.push_back((index, layout, bytes));
        paint
    }

    /// The layout after the last committed step, which the next step is prepared from.
    pub(super) fn latest(&self) -> &LayoutState {
        self.unshown
            .back()
            .map_or(&self.rest.0, |(_, layout, _)| layout)
    }

    /// The most the shell may paint for this scene: the shown layout, or any step it may show
    /// next, field by field as the shell charges it.
    pub(super) fn charge(&self) -> PaintWork {
        self.unshown
            .iter()
            .fold(self.rest.0.paint_work(), |charge, (_, layout, _)| {
                charge.max(layout.paint_work())
            })
    }

    /// Step `index` committed with `layout`.
    pub(super) fn committed(&mut self, index: u32, (layout, bytes): (LayoutState, usize)) {
        self.unshown.push_back((index, layout, bytes));
    }

    /// Step `index` was painted, so it's the rest now and the steps before it are gone.
    pub(super) fn shown(&mut self, index: u32) {
        while self.unshown.front().is_some_and(|(at, _, _)| *at <= index) {
            if let Some((_, layout, bytes)) = self.unshown.pop_front() {
                self.rest = (layout, bytes);
            }
        }
    }

    /// Bytes its layouts are charged at, counting what they share once per layout.
    pub(super) fn resident_bytes(&self) -> usize {
        self.unshown
            .iter()
            .fold(self.rest.1, |total, (_, _, bytes)| total + bytes)
    }

    /// The scene was stopped: the shell keeps the last activated step's layout, cut.
    pub(super) fn cut(self, scene: &SceneState) -> Option<Self> {
        let activated = scene.activated();
        let base = self
            .unshown
            .into_iter()
            .take_while(|(index, _, _)| *index < activated)
            .last()
            .map_or(self.rest.0, |(_, layout, _)| layout);
        base.cut(scene).ok().map(Self::at_rest)
    }
}

impl Drawing {
    /// Lays an admitted step out before its write, sealing the reducer as a write does.
    pub(super) fn prepare_step(&mut self, step: StepWait, write: Write, out: &mut Output) {
        let input = self.step_input(&step);
        if self.paint_metrics.is_none() && !matches!(input, Ok(Some(_))) {
            self.write(Wait::Step(Box::new(step)), write, out);
            return;
        }
        self.serial += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.pending = Some(Pending {
            serial: self.serial,
            in_flight: true,
            wait: Wait::Prepare {
                step: Box::new(step),
                write,
                cancel: Arc::clone(&cancel),
            },
        });
        let input = match input {
            Ok(Some(input)) => input,
            failed => {
                let error = failed.err().unwrap_or(PrepareError::Stale);
                self.prepared(self.serial, error.into(), out)
                    .expect("the step with rejected input is the pending one");
                return;
            }
        };
        let job = PrepareJob {
            serial: self.serial,
            input: Shared(Arc::new(Mutex::new(Some(input)))),
            cancel,
            metrics: self.paint_metrics,
        };
        #[cfg(test)]
        if !self.defer_preparation {
            self.prepared(job.serial, job.outcome(), out)
                .expect("the step just laid out is the pending one");
            return;
        }
        out.effects.push(Effect::PrepareDrawing(job));
    }

    fn step_input(&self, step: &StepWait) -> Result<Option<StepInput>, PrepareError> {
        let (ticket, before) = match &step.scene {
            Pend::Held { scene, admitted } => {
                let at = self.live(*scene).ok_or(PrepareError::Stale)?;
                let held = &self.scenes[at];
                let Some(paint) = &held.paint else {
                    if self.paint_metrics.is_none() {
                        return Ok(None);
                    }
                    let prefix = held.state.recovery_log();
                    let revision = held.state.revision() + 1;
                    let mut log = Vec::with_capacity(prefix.len() + 1);
                    log.extend_from_slice(prefix);
                    log.push(SceneEvent::Step {
                        revision,
                        step: Arc::clone(&step.step),
                    });
                    return Ok(Some(StepInput::Replay {
                        header: held.state.header().clone(),
                        log,
                        theme: Arc::clone(&self.theme),
                        revision,
                    }));
                };
                let ticket = held
                    .state
                    .preview(admitted)
                    .map_err(|_| PrepareError::Stale)?;
                (ticket, paint.latest().clone())
            }
            Pend::New { admission, .. } => admission.preview(),
        };
        let input = PrepareInput::new(ticket, before, SURFACE, Arc::clone(&self.theme))
            .map_err(|(error, _)| error)?;
        Ok(Some(StepInput::Live(
            input.with_paint_cap(Some(PaintWork::LIMITS)),
        )))
    }

    /// A step was laid out: it's written if everything the shell may then paint fits, and
    /// refused before anything is written if not. A negotiated measure also requires successful
    /// geometry validation even when the scene has no retained paint charge.
    pub(super) fn prepared(
        &mut self,
        serial: u64,
        outcome: Prepared,
        out: &mut Output,
    ) -> Result<(), Stale> {
        let Some(Pending {
            wait: Wait::Prepare {
                mut step, write, ..
            },
            ..
        }) = self.pending.take_if(|pending| {
            pending.serial == serial && matches!(pending.wait, Wait::Prepare { .. })
        })
        else {
            return Err(Stale);
        };
        if step.fenced {
            return Ok(());
        }
        let Some(at) = self.open_call(step.run, &step.call) else {
            return Ok(());
        };
        if let Some(error) = step.interrupted.take() {
            self.end(at, Some(error), out);
            return Ok(());
        }
        if let Pend::Held { scene, .. } = &step.scene
            && self.live(*scene).is_none()
        {
            let error =
                StepError::in_step(&step.step.id, FieldPath::default(), ErrorCode::SceneCleared);
            self.fail(at, CallError::Step(error), out);
            return Ok(());
        }
        let outcome = match outcome {
            Prepared::Refused(StepError {
                code: ErrorCode::OutOfRange,
                ..
            }) if self.paint_metrics.is_some() => Prepared::Geometry,
            outcome => outcome,
        };
        step.laid = match outcome {
            Prepared::Layout { layout, bytes } => Some((Arc::unwrap_or_clone(layout.0), bytes)),
            Prepared::Geometry => {
                self.fail(
                    at,
                    CallError::Step(StepError::in_step(
                        &step.step.id,
                        FieldPath::default(),
                        ErrorCode::OutOfRange,
                    )),
                    out,
                );
                return Ok(());
            }
            Prepared::Refused(error)
                if self.paint_metrics.is_some() || self.calls[at].picture.is_some() =>
            {
                self.fail(at, CallError::Step(error), out);
                return Ok(());
            }
            Prepared::Failed if self.paint_metrics.is_some() => {
                self.end(at, Some(CallError::Failed), out);
                return Ok(());
            }
            Prepared::VerifiedUncounted | Prepared::Failed | Prepared::Refused(_) => None,
            Prepared::TooMuch => {
                self.end(at, Some(CallError::TooMuchToPaint { cleared: false }), out);
                return Ok(());
            }
            Prepared::Cancelled => {
                self.end(at, Some(CallError::Interrupted), out);
                return Ok(());
            }
        };
        if let Some((after, _)) = &step.laid
            && !self.fits(&step.scene, after.paint_work())
        {
            self.end(at, Some(CallError::TooMuchToPaint { cleared: false }), out);
            return Ok(());
        }
        let victims: &[SceneId] = match &step.scene {
            Pend::New { victims, .. } => victims,
            Pend::Held { .. } => &[],
        };
        let retained: usize = self
            .scenes
            .iter()
            .filter(|held| !victims.contains(&held.scene))
            .filter_map(|held| held.paint.as_ref())
            .map(Paint::resident_bytes)
            .sum();
        if step
            .laid
            .as_ref()
            .is_some_and(|(_, bytes)| retained + bytes > MAX_RETAINED_BYTES)
        {
            step.laid = None;
        }
        self.write(Wait::Step(step), write, out);
        Ok(())
    }

    /// Whether the shell can paint every counted scene once `scene` may show `work`: the scene
    /// keeps what it may already show, and a new scene's replacement victims leave with its write.
    fn fits(&self, scene: &Pend, work: PaintWork) -> bool {
        let (own, victims): (Option<SceneId>, &[SceneId]) = match scene {
            Pend::Held { scene, .. } => (Some(*scene), &[]),
            Pend::New { victims, .. } => (None, victims),
        };
        let mut others = PaintWork::default();
        let mut charge = work;
        for held in &self.scenes {
            let Some(paint) = held.paint.as_ref().filter(|_| !held.gone) else {
                continue;
            };
            if Some(held.scene) == own {
                charge = charge.max(paint.charge());
            } else if !victims.contains(&held.scene) {
                others = others + paint.charge();
            }
        }
        (others + charge).within_limits()
    }

    /// Cancels the step being laid out if `which` names it.
    pub(super) fn cancel_preparation(&self, which: impl Fn(&StepWait) -> bool) {
        if let Some(Pending {
            wait: Wait::Prepare { step, cancel, .. },
            ..
        }) = &self.pending
            && which(step)
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "the job shares the input by pointer, so boxing would add a second allocation"
)]
pub(super) enum StepInput {
    Live(PrepareInput),
    Replay {
        header: Begin,
        log: Vec<SceneEvent>,
        theme: Arc<Theme>,
        revision: u32,
    },
}

/// A step to lay out, and the flag that cancels it.
#[derive(Clone)]
pub struct PrepareJob {
    pub(super) serial: u64,
    /// Taken by the first copy that runs.
    pub(super) input: Shared<Mutex<Option<StepInput>>>,
    pub(super) cancel: Arc<AtomicBool>,
    metrics: Option<TextMetricsV1>,
}

impl PrepareJob {
    /// The answer when the job can't start.
    pub fn unprepared(&self) -> Input {
        Input::Prepared {
            serial: self.serial,
            outcome: Prepared::Failed,
        }
    }

    /// Lays the step out with the pair's measure, for the drawing system's answer.
    pub fn run(self) -> Input {
        Input::Prepared {
            serial: self.serial,
            outcome: self.outcome(),
        }
    }

    fn outcome(&self) -> Prepared {
        if self.cancel.load(Ordering::Relaxed) {
            return Prepared::Cancelled;
        }
        let input = self.input.0.lock().ok().and_then(|mut input| input.take());
        let mut measure = ProfileMeasure::new(self.metrics);
        let (result, failed_prefix) = match input {
            Some(StepInput::Live(input)) => (
                prepare(input, &mut measure, &self.cancel).map(|step| Some(step.after)),
                false,
            ),
            Some(StepInput::Replay {
                header,
                log,
                theme,
                revision,
            }) => {
                let result = recover(
                    header,
                    &log,
                    SURFACE,
                    &theme,
                    &mut measure,
                    &self.cancel,
                    Some(PaintWork::LIMITS),
                );
                let failed_prefix = result
                    .as_ref()
                    .is_err_and(|error| error.revision != revision);
                (
                    result.map(|_| None).map_err(|error| error.error),
                    failed_prefix,
                )
            }
            None => return Prepared::Failed,
        };
        if measure.geometry_exceeded() {
            return Prepared::Geometry;
        }
        match result {
            Ok(Some(after)) => Prepared::Layout {
                bytes: after.resident_bytes(),
                layout: Shared(Arc::new(after)),
            },
            Ok(None) => Prepared::VerifiedUncounted,
            Err(error) => match Prepared::from(error) {
                Prepared::Refused(error)
                    if failed_prefix && error.code != ErrorCode::OutOfRange =>
                {
                    Prepared::Failed
                }
                outcome => outcome,
            },
        }
    }
}

impl PartialEq for PrepareJob {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial && self.input == other.input
    }
}

impl fmt::Debug for PrepareJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrepareJob")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

/// How a step's preparation ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Prepared {
    /// Its layout after the step, and the bytes that layout retains.
    Layout {
        /// The layout.
        layout: Shared<LayoutState>,
        /// Its resident bytes, counted off the coordinator thread.
        bytes: usize,
    },
    /// Detached replay verified the geometry, but yields no live layout or paint charge.
    VerifiedUncounted,
    /// The scene alone paints more than the shell can.
    TooMuch,
    /// A negotiated measurement or its derived layout exceeded a geometry bound.
    Geometry,
    /// The reducer cancelled it.
    Cancelled,
    /// Layout refused the step. Screen scenes and negotiated pairs reject it before writing.
    Refused(StepError),
    /// Validation could not finish. Without negotiated metrics the shell's guard checks the step.
    Failed,
}

impl From<PrepareError> for Prepared {
    fn from(error: PrepareError) -> Self {
        match error {
            PrepareError::OverPaintWork => Self::TooMuch,
            PrepareError::Cancelled => Self::Cancelled,
            PrepareError::Layout(error) => Self::Refused(error),
            _ => Self::Failed,
        }
    }
}

/// A value handed between threads whole; equal only to itself, and printed without content.
pub struct Shared<T>(pub(super) Arc<T>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Shared")
    }
}
