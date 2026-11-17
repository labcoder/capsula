//! The screen watch: when a screen scene may be shown, and when it must go. Plain state with
//! no clock and no operating-system call: the presenter owns one, feeds it events and does what
//! it answers. It keeps one bit about the screen and nothing about what set it.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{RunId, SceneId};

/// How long screen marks stay up after their answer ends.
pub(super) const MARKS_KEPT: Duration = Duration::from_secs(60);
/// How long a released hold may go without its run being named before the watch ends, so no
/// road the shell misses leaves the sources listening.
pub(super) const UNNAMED_KEPT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// Nothing is expected or shown, and nothing listens.
    #[default]
    Idle,
    /// A hold that sends the screen was released, and no screen scene has been shown since.
    Expecting {
        /// The newest run named when the keys came up; the hold's run is newer.
        after: RunId,
        /// The hold's run, once named.
        run: Option<RunId>,
        /// Something may have changed the screen since the release.
        changed: bool,
        /// When the watch ends unless the run is named first.
        until: Instant,
    },
    /// A screen scene is up.
    Showing {
        scene: SceneId,
        run: RunId,
        /// When the time limit clears it; none until its answer has ended.
        ends: Option<Instant>,
    },
    /// The brain replaced the shown scene while its run went on. `successor` takes its place
    /// if it turns out to be that run's screen scene.
    Passing { successor: SceneId, run: RunId },
}

#[derive(Debug, Default)]
pub(super) struct Watch {
    state: State,
    /// The run whose marks were skipped because the screen changed first, until the next
    /// release.
    skipped: Option<RunId>,
}

impl Watch {
    /// The keys of a hold that sends the screen came up at `now`; `after` is the newest run
    /// named so far.
    pub(super) fn released(&mut self, after: RunId, now: Instant) {
        self.state = State::Expecting {
            after,
            run: None,
            changed: false,
            until: now + UNNAMED_KEPT,
        };
        self.skipped = None;
    }

    /// The released hold's run, named at `now`. Only the first name counts, and only a run
    /// newer than the release. A name that comes at or after the unnamed limit ends the watch
    /// instead, whether or not the watchdog has run yet.
    pub(super) fn named(&mut self, named: RunId, now: Instant) {
        if let State::Expecting {
            after,
            run: None,
            changed,
            until,
        } = self.state
        {
            if now >= until {
                self.state = State::Idle;
            } else if named > after {
                self.state = State::Expecting {
                    after,
                    run: Some(named),
                    changed,
                    until,
                };
            }
        }
    }

    /// The hold's screenshot wasn't sent, or the hold ended with no run.
    pub(super) fn dropped(&mut self) {
        if matches!(self.state, State::Expecting { .. }) {
            self.state = State::Idle;
        }
    }

    /// Something may have changed the screen. True when a shown scene must be cleared. While a
    /// scene is only expected the bit is kept, and nothing later clears it. A successor that
    /// hasn't taken its place yet loses it.
    pub(super) fn changed(&mut self) -> bool {
        match &mut self.state {
            State::Expecting { changed, .. } => {
                *changed = true;
                false
            }
            State::Showing { .. } => {
                self.state = State::Idle;
                true
            }
            State::Passing { .. } => {
                self.state = State::Idle;
                false
            }
            State::Idle => false,
        }
    }

    /// A screen scene's first mark, from `run`. True when it is shown, and every other screen
    /// scene then goes; false when it never is. It is shown only if nothing may have changed
    /// the screen since the release and `run` is the run the released hold named. Until a run
    /// is named none is shown: a newer run can be an earlier hold's, whose scene was queued
    /// before either hold's first state came down the other pipe. A second scene from the run
    /// already shown takes its place, and so does the declared successor of a replaced one.
    pub(super) fn marked(&mut self, scene: SceneId, run: RunId) -> bool {
        let ends = match self.state {
            State::Expecting {
                after,
                run: named,
                changed,
                ..
            } if run > after && named.is_none_or(|named| named == run) => {
                // shortcut: the hold's own scene is skipped too when it comes before its
                // run's first state, with the skipped line; hold such a scene back until the
                // name comes if that order is ever seen in use.
                if changed || named.is_none() {
                    self.skipped = Some(run);
                    return false;
                }
                None
            }
            State::Showing {
                run: shown, ends, ..
            } if shown == run => ends,
            State::Passing {
                successor,
                run: shown,
            } if successor == scene && shown == run => None,
            State::Idle
            | State::Expecting { .. }
            | State::Showing { .. }
            | State::Passing { .. } => return false,
        };
        self.state = State::Showing { scene, run, ends };
        true
    }

    /// A run ended: its terminal state and its `RunEnd` are both in. A watch expecting that
    /// run's scene ends, and a shown scene of that run gets its time limit, once. While no run
    /// is named, no run's end ends the watch: a newer run that ends first can be an earlier
    /// hold's, and the unnamed limit already bounds the wait.
    pub(super) fn ended(&mut self, run: RunId, now: Instant) {
        self.state = match self.state {
            State::Expecting {
                run: Some(named), ..
            } if named == run => State::Idle,
            State::Passing { run: shown, .. } if shown == run => State::Idle,
            State::Showing {
                scene,
                run: shown,
                ends: None,
            } if shown == run => State::Showing {
                scene,
                run,
                ends: Some(now + MARKS_KEPT),
            },
            state => state,
        };
    }

    /// Ends a watch whose limit has passed: a shown scene's, or an unnamed release's. True when
    /// the shown scene must be cleared.
    pub(super) fn expired(&mut self, now: Instant) -> bool {
        let (limit, shown) = match self.state {
            State::Showing {
                ends: Some(ends), ..
            } => (ends, true),
            State::Expecting {
                run: None, until, ..
            } => (until, false),
            State::Idle
            | State::Expecting { .. }
            | State::Showing { .. }
            | State::Passing { .. } => return false,
        };
        if now < limit {
            return false;
        }
        self.state = State::Idle;
        shown
    }

    /// The brain replaced `scenes` with `successor`. While the shown scene's run goes on, its
    /// place passes to the successor, which keeps the run's watch only by showing that run's
    /// first mark. True when the shown scene went this way.
    pub(super) fn replaced(&mut self, scenes: &[SceneId], successor: SceneId) -> bool {
        match self.state {
            State::Showing {
                scene,
                run,
                ends: None,
            } if scenes.contains(&scene) => {
                self.state = State::Passing { successor, run };
                true
            }
            _ => false,
        }
    }

    /// `scene` left the mirror, by any road, or will never show a mark. True when it was the
    /// shown scene. A successor that goes before its first mark ends the watch too.
    pub(super) fn gone(&mut self, scene: SceneId) -> bool {
        let shown = self.shows(scene);
        if shown || matches!(self.state, State::Passing { successor, .. } if successor == scene) {
            self.state = State::Idle;
        }
        shown
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Showing { ends, .. } => ends,
            State::Expecting {
                run: None, until, ..
            } => Some(until),
            State::Idle | State::Expecting { .. } | State::Passing { .. } => None,
        }
    }

    /// Whether a change still matters, so the sources must listen.
    pub(super) fn listening(&self) -> bool {
        matches!(
            self.state,
            State::Expecting { changed: false, .. } | State::Showing { .. } | State::Passing { .. }
        )
    }

    pub(super) fn shows(&self, scene: SceneId) -> bool {
        self.shown() == Some(scene)
    }

    /// The screen scene that is up.
    pub(super) fn shown(&self) -> Option<SceneId> {
        match self.state {
            State::Showing { scene, .. } => Some(scene),
            State::Idle | State::Expecting { .. } | State::Passing { .. } => None,
        }
    }

    /// The run whose screen scene is up.
    pub(super) fn shown_run(&self) -> Option<RunId> {
        match self.state {
            State::Showing { run, .. } => Some(run),
            State::Idle | State::Expecting { .. } | State::Passing { .. } => None,
        }
    }

    pub(super) fn skipped(&self) -> Option<RunId> {
        self.skipped
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
