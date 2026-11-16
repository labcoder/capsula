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

    /// The released hold's run. Only the first name counts, and only a run newer than the
    /// release.
    pub(super) fn named(&mut self, named: RunId) {
        if let State::Expecting {
            after,
            run: run @ None,
            ..
        } = &mut self.state
            && named > *after
        {
            *run = Some(named);
        }
    }

    /// The hold's screenshot wasn't sent, or the hold ended with no run.
    pub(super) fn dropped(&mut self) {
        if matches!(self.state, State::Expecting { .. }) {
            self.state = State::Idle;
        }
    }

    /// Something may have changed the screen. True when a shown scene must be cleared. While a
    /// scene is only expected the bit is kept, and nothing later clears it.
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
            State::Idle => false,
        }
    }

    /// A screen scene's first mark, from `run`. True when it is shown, and every other screen
    /// scene then goes; false when it never is. It is shown only if nothing may have changed
    /// the screen since the release and `run` is the hold's: newer than the release, and the
    /// named run once there is one. A second scene from the run already shown takes its place.
    pub(super) fn marked(&mut self, scene: SceneId, run: RunId) -> bool {
        let ends = match self.state {
            State::Expecting {
                after,
                run: named,
                changed,
                ..
            } if run > after && named.is_none_or(|named| named == run) => {
                if changed {
                    self.skipped = Some(run);
                    return false;
                }
                None
            }
            State::Showing {
                run: shown, ends, ..
            } if shown == run => ends,
            State::Idle | State::Expecting { .. } | State::Showing { .. } => return false,
        };
        self.state = State::Showing { scene, run, ends };
        true
    }

    /// A run ended: its terminal state and its `RunEnd` are both in. A watch expecting that
    /// run's scene ends, and a shown scene of that run gets its time limit, once.
    pub(super) fn ended(&mut self, run: RunId, now: Instant) {
        self.state = match self.state {
            State::Expecting {
                run: Some(named), ..
            } if named == run => State::Idle,
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
            State::Idle | State::Expecting { .. } | State::Showing { .. } => return false,
        };
        if now < limit {
            return false;
        }
        self.state = State::Idle;
        shown
    }

    /// `scene` left the mirror, by any road.
    pub(super) fn gone(&mut self, scene: SceneId) {
        if self.shows(scene) {
            self.state = State::Idle;
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Showing { ends, .. } => ends,
            State::Expecting {
                run: None, until, ..
            } => Some(until),
            State::Idle | State::Expecting { .. } => None,
        }
    }

    /// Whether a change still matters, so the sources must listen.
    pub(super) fn listening(&self) -> bool {
        matches!(
            self.state,
            State::Expecting { changed: false, .. } | State::Showing { .. }
        )
    }

    pub(super) fn shows(&self, scene: SceneId) -> bool {
        matches!(self.state, State::Showing { scene: shown, .. } if shown == scene)
    }

    pub(super) fn skipped(&self) -> Option<RunId> {
        self.skipped
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
