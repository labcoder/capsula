use std::time::Instant;

use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};
use kumochi_scene::record::Start;

use super::{Cut, DrawKey, Presenter, Scene};
use crate::speaker::conductor::{
    Assignment, Binding, Motion, Picture, PictureLevel, PictureState, RouteStamp, View, ViewLevel,
};

type Lookup = dyn Fn(DrawKey) -> Result<Option<Assignment>, ()>;

#[derive(Clone, Copy, Debug)]
pub(crate) enum RetirementLane {
    Scenes,
    Cut,
}

impl RetirementLane {
    pub(crate) const fn slot(self) -> usize {
        match self {
            Self::Scenes => 0,
            Self::Cut => 1,
        }
    }
}

pub(super) struct Conduction {
    origin: Instant,
    lookup: Box<Lookup>,
    retirement_consumed: Box<dyn Fn(RetirementLane)>,
    nonce: u64,
    current: Option<Current>,
    changed: bool,
    detached: [Option<Assignment>; MAX_VISIBLE_SCENES],
}

#[derive(Clone, Copy)]
struct Current {
    level: ViewLevel,
    route: RouteStamp,
    picture: Picture,
    fallback: bool,
    installed: bool,
    shown_reported: bool,
}

fn route_order(route: RouteStamp) -> Option<(u64, bool)> {
    match route {
        RouteStamp::Unstamped => None,
        RouteStamp::Live(generation) => Some((generation, false)),
        RouteStamp::Retired(generation) => Some((generation, true)),
    }
}

fn first(id: SceneId, scene: &Scene) -> Option<DrawKey> {
    let (run, index) = if let Some(head) = scene.ready.front() {
        (head.run, head.step.index)
    } else if let Some(head) = &scene.running {
        (head.run, head.index)
    } else {
        let head = scene.queue.front()?;
        (head.run, head.index)
    };
    Some(DrawKey {
        run,
        scene: id,
        index,
    })
}

fn draw_key(a: Assignment) -> DrawKey {
    DrawKey {
        run: a.run,
        scene: a.step.scene,
        index: a.step.index,
    }
}

impl Conduction {
    pub(super) fn record_installation(&mut self, key: DrawKey) {
        if let Some(current) = &mut self.current
            && current.level.view != View::Fenced
            && draw_key(current.level.binding.assignment) == key
        {
            current.installed = true;
            self.changed = true;
        }
    }

    pub(super) fn fence(&mut self, matches: impl Fn(DrawKey) -> bool) {
        if let Some(current) = &mut self.current
            && matches(draw_key(current.level.binding.assignment))
            && current.level.view != View::Fenced
        {
            current.level.view = View::Fenced;
            self.changed = true;
        }
        for slot in &mut self.detached {
            if slot.is_some_and(|a| matches(draw_key(a))) {
                *slot = None;
            }
        }
    }

    fn detached_for(&self, session: Option<u64>, key: DrawKey) -> bool {
        let matches = |&a: &Assignment| Some(a.session) == session && draw_key(a) == key;
        self.detached.iter().flatten().any(matches)
    }

    fn assignment(&self, session: Option<u64>, key: DrawKey) -> Result<Option<Assignment>, ()> {
        (self.lookup)(key)?
            .map(|a| {
                if Some(a.session) == session && draw_key(a) == key {
                    Ok(a)
                } else {
                    Err(())
                }
            })
            .transpose()
    }
}

impl Presenter {
    /// Unresolved lookup is Err; Ok(None) means the head has no narration assignment.
    pub(crate) fn start_conduction(
        &mut self,
        origin: Instant,
        lookup: impl Fn(DrawKey) -> Result<Option<Assignment>, ()> + 'static,
        retirement_consumed: impl Fn(RetirementLane) + 'static,
    ) {
        self.conduction.get_or_insert_with(|| {
            Box::new(Conduction {
                origin,
                lookup: Box::new(lookup),
                retirement_consumed: Box::new(retirement_consumed),
                nonce: 0,
                current: None,
                changed: false,
                detached: [None; MAX_VISIBLE_SCENES],
            })
        });
    }

    pub(super) fn narration_retirement_consumed(&self, lane: RetirementLane) {
        if let Some(state) = &self.conduction {
            (state.retirement_consumed)(lane);
        }
    }

    fn local_view(&self, key: DrawKey, scene: &Scene, current: Current, now: Instant) -> View {
        let paint = scene.activation.installation(key.run, key.index);
        if current.level.view == View::Shown || paint == Some(true) {
            return View::Shown;
        }
        if current.installed || paint.is_some() {
            return View::Eligible;
        }
        if first(key.scene, scene) != Some(key) {
            return View::Fenced;
        }
        self.head_view(key.scene, scene, || self.visual_guard(key, scene, now))
    }

    fn head_view(&self, id: SceneId, scene: &Scene, eligible: impl FnOnce() -> bool) -> View {
        if scene.ready.front().is_none() {
            View::Preparing
        } else if eligible() && self.lane.as_ref().is_none_or(|lane| lane.scene != id) {
            View::Eligible
        } else {
            View::Prepared
        }
    }

    fn visual_guard(&self, key: DrawKey, scene: &Scene, now: Instant) -> bool {
        !self.frames.borrow().guards_successor(key.scene)
            && scene
                .activation
                .due(scene.state.view(), key.index)
                .is_ok_and(|due| due <= scene.activation.time(now))
    }

    pub(super) fn sync_conduction(&mut self, now: Instant) {
        let Some(mut state) = self.conduction.take() else {
            return;
        };
        let previous = state.current.map(|current| current.level);
        let fenced = |key: DrawKey| {
            self.fenced.is_some_and(|run| key.run <= run)
                || self
                    .scenes
                    .get(&key.scene)
                    .is_none_or(|s| s.cut != Cut::Open || s.skipping)
        };
        state.fence(fenced);
        for slot in &mut state.detached {
            if slot.is_some_and(|a| {
                let key = draw_key(a);
                self.scenes
                    .get(&key.scene)
                    .and_then(|s| first(key.scene, s))
                    != Some(key)
            }) {
                *slot = None;
            }
        }
        let current = state.current;
        let view = current.map_or(View::Fenced, |current| {
            let a = current.level.binding.assignment;
            if current.level.view == View::Fenced
                || current.picture == Picture::Fenced
                || self.generation != Some(a.session)
            {
                return View::Fenced;
            }
            let key = draw_key(a);
            self.local_view(key, &self.scenes[&key.scene], current, now)
        });
        let next = if view == View::Fenced {
            let fenced_key = current.map(|c| draw_key(c.level.binding.assignment));
            self.scenes.iter().find_map(|(&id, scene)| {
                let key = first(id, scene)?;
                if fenced(key)
                    || Some(key) == fenced_key
                    || state.detached_for(self.generation, key)
                    || !matches!(scene.state.view().step(key.index)?.start, Start::Speech)
                    || !self.visual_guard(key, scene, now)
                {
                    return None;
                }
                Some((
                    state.assignment(self.generation, key).ok()??,
                    self.head_view(id, scene, || true),
                ))
            })
        } else {
            None
        };
        if let Some((assignment, view)) = next {
            state.nonce = state
                .nonce
                .checked_add(1)
                .expect("session binding count fits");
            state.current = Some(Current {
                level: ViewLevel {
                    binding: Binding {
                        assignment,
                        nonce: state.nonce,
                    },
                    speech: true,
                    view,
                },
                route: RouteStamp::Unstamped,
                picture: Picture::Wait,
                fallback: false,
                installed: false,
                shown_reported: false,
            });
        } else if let Some(current) = state.current.as_mut() {
            current.level.view = view;
        }
        state.changed |= previous != state.current.map(|current| current.level);
        self.conduction = Some(state);
    }

    /// Whether an existing scene pass changed the local binding or its readiness.
    #[inline]
    pub fn take_narration_change(&mut self) -> bool {
        self.conduction
            .as_mut()
            .is_some_and(|state| std::mem::take(&mut state.changed))
    }

    pub(crate) fn conductor_view(&mut self, now: Instant) -> Option<ViewLevel> {
        self.sync_conduction(now);
        let state = self.conduction.as_mut()?;
        state.changed = false;
        let current = state.current.as_mut()?;
        current.shown_reported |= current.level.view == View::Shown;
        Some(current.level)
    }

    pub(crate) fn conductor_removed(&self, generation: u64, scene: SceneId) -> bool {
        self.generation == Some(generation) && self.cleared.contains(&scene)
    }

    pub(crate) fn conductor_fence(&mut self, binding: Binding) {
        if let Some(current) = self
            .conduction
            .as_mut()
            .and_then(|state| state.current.as_mut())
            && current.level.binding == binding
        {
            current.level.view = View::Fenced;
        }
    }

    /// Retires the audio binding without changing the installed frame or its paint receipt.
    pub(crate) fn conductor_handoff(&mut self, assignment: Assignment) -> bool {
        if let Some(state) = &mut self.conduction
            && let Some(current) = state.current
            && current.level.binding.assignment == assignment
        {
            if current.level.view != View::Fenced && !current.installed {
                return false;
            }
            state.current = None;
        }
        true
    }

    /// Ok(None) is a coherent empty slot; Err is a torn read.
    pub(crate) fn conductor_picture(
        &mut self,
        read: Result<Option<PictureLevel>, ()>,
        now: Instant,
    ) {
        self.sync_conduction(now);
        let Some(state) = &mut self.conduction else {
            return;
        };
        let Some(current) = state
            .current
            .as_mut()
            .filter(|current| current.level.view != View::Fenced)
        else {
            return;
        };
        let Ok(read) = read else { return };
        let Some(level) = read else {
            current.picture = Picture::Wait;
            return;
        };
        if level.binding != current.level.binding
            || ((level.route, level.state) != (RouteStamp::Unstamped, PictureState::Unavailable)
                && route_order(level.route) < route_order(current.route))
        {
            return;
        }
        current.picture = match level.state {
            PictureState::Unavailable
            | PictureState::Ended(
                Picture::Wait
                | Picture::Silent
                | Picture::Authored
                | Picture::Shown(Motion::Authored),
            ) => {
                let a = level.binding.assignment;
                if !current.installed
                    && let Some(scene) = self.scenes.get(&a.step.scene)
                    && scene.activation.installation(a.run, a.step.index).is_none()
                    && !current.shown_reported
                {
                    let slot = state
                        .detached
                        .iter()
                        .position(|slot| slot.is_some_and(|old| old.step.scene == a.step.scene))
                        .or_else(|| state.detached.iter().position(Option::is_none))
                        .expect("one pending head per live scene");
                    state.detached[slot] = Some(a);
                }
                state.current = None;
                return;
            }
            PictureState::Ended(_) => return,
            PictureState::Owned(picture) => {
                current.route = level.route;
                match picture {
                    Picture::At(_) if !matches!(level.route, RouteStamp::Live(_)) => Picture::Wait,
                    other => other,
                }
            }
        };
        // This unit uses the authored clock after paint, even when the worker supplies motion.
        current.fallback |= matches!(
            current.picture,
            Picture::Silent | Picture::Authored | Picture::Shown(Motion::Authored)
        );
    }

    pub(super) fn conducted_onset(&self, key: DrawKey) -> Result<Option<Instant>, ()> {
        let Some(state) = &self.conduction else {
            return Ok(None);
        };
        let current = state.current;
        let scene = &self.scenes[&key.scene];
        if !scene
            .state
            .view()
            .step(key.index)
            .is_some_and(|step| matches!(step.start, Start::Speech))
        {
            return Ok(None);
        }
        if current.is_some_and(|current| {
            let a = current.level.binding.assignment;
            a.step.scene == key.scene
                && current.level.view != View::Fenced
                && !current.shown_reported
                && scene.activation.installation(a.run, a.step.index).is_some()
        }) {
            return Err(());
        }
        if state.detached_for(self.generation, key) {
            return Ok(None);
        }
        let Some(assignment) = state.assignment(self.generation, key)? else {
            return Ok(None);
        };
        let Some(current) = current.filter(|current| {
            current.level.binding.assignment == assignment
                && current.level.view != View::Fenced
                && current.picture != Picture::Fenced
        }) else {
            return Err(());
        };
        if current.fallback {
            Ok(None)
        } else if let Picture::At(at) = current.picture {
            state.origin.checked_add(at).map(Some).ok_or(())
        } else {
            Err(())
        }
    }
}
