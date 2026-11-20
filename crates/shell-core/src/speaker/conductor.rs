//! Dormant policy for one live drawing assignment.

use std::time::Duration;

use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId, StepRef};
use kumochi_scene::record::Start;

const PCM_HOLD: Duration = Duration::from_millis(250);
const REBUILD_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Assignment {
    pub(super) session: u64,
    pub(super) output: OutputGeneration,
    pub(super) run: RunId,
    pub(super) segment: SegmentId,
    pub(super) step: StepRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum View {
    Preparing,
    Prepared,
    Eligible,
    Shown,
    Fenced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Live(u64),
    Rebuilding,
    Virtual,
    Muted,
    NoWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Head {
    WaitingForPcm,
    AdvancingPredecessor,
    EligiblePcm,
    Played,
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SynthesisFinal {
    pub(super) segment: SegmentId,
    pub(super) samples: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourceSpan {
    pub(super) source: u64,
    pub(super) at: Duration,
    pub(super) submitted_through: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Handoff {
    pub(super) generation: u64,
    pub(super) first: Duration,
    pub(super) predicted: Option<Duration>,
    pub(super) latency: Option<Duration>,
    pub(super) motion: Option<SourceSpan>,
}

impl Handoff {
    fn onset(self) -> Duration {
        self.predicted
            .unwrap_or_else(|| self.first.saturating_add(self.latency.unwrap_or_default()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Playback {
    pub(super) route: Route,
    pub(super) head: Head,
    pub(super) handoff: Option<Handoff>,
    pub(super) synthesis: Option<SynthesisFinal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gate {
    Held,
    Allowed,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Motion {
    Frozen,
    Follow(SourceSpan),
    Authored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Picture {
    Wait,
    At(Duration),
    Authored,
    Silent,
    Shown(Motion),
    Fenced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Decision {
    pub(super) audio: Gate,
    pub(super) picture: Picture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Waiting,
    Silent,
    Shown,
    ShownSilent,
    Fenced,
}

pub(super) struct Conductor {
    assignment: Assignment,
    speech: bool,
    stage: Stage,
    audio: Gate,
    submitted: bool,
    eligible_since: Option<Duration>,
    hold_until: Option<Duration>,
    rebuild_until: Option<Duration>,
    generation: Option<u64>,
    route_retired: bool,
    handoff: Option<Handoff>,
}

impl Conductor {
    pub(super) fn new(assignment: Assignment, start: &Start) -> Self {
        Self {
            assignment,
            speech: matches!(start, Start::Speech),
            stage: Stage::Waiting,
            audio: Gate::Held,
            submitted: false,
            eligible_since: None,
            hold_until: None,
            rebuild_until: None,
            generation: None,
            route_retired: false,
            handoff: None,
        }
    }

    pub(super) fn update(
        &mut self,
        assignment: Assignment,
        now: Duration,
        view: View,
        playback: Playback,
    ) -> Option<Decision> {
        if assignment != self.assignment {
            return None;
        }
        let picture = self.picture(now, view, playback);
        Some(Decision {
            audio: self.audio,
            picture,
        })
    }

    fn picture(&mut self, now: Duration, view: View, mut playback: Playback) -> Picture {
        if self.stage == Stage::Fenced {
            return Picture::Fenced;
        }
        let hold_expired = self.audio == Gate::Held && self.hold_until.is_some_and(|at| now >= at);
        let rebuilding = self.observe_route(now, &mut playback);
        if view == View::Fenced {
            self.drop_unsent();
            self.stage = Stage::Fenced;
            return Picture::Fenced;
        }
        let empty = playback
            .synthesis
            .is_some_and(|final_| final_.segment == self.assignment.segment && final_.samples == 0);
        let no_sound = matches!(
            playback.route,
            Route::Virtual | Route::Muted | Route::NoWorker
        );
        if playback.head == Head::Revoked {
            self.drop_unsent();
        }
        if empty || no_sound || matches!(playback.head, Head::Played | Head::Revoked) {
            self.silent();
        }

        if hold_expired && self.audio != Gate::Dropped {
            self.audio = Gate::Allowed;
            self.silent();
        } else if self.audio == Gate::Held {
            if playback.head == Head::EligiblePcm {
                self.hold_until.get_or_insert(now.saturating_add(PCM_HOLD));
            }
            if view != View::Preparing || empty || no_sound || playback.head == Head::Played {
                self.audio = Gate::Allowed;
            }
        }

        if view == View::Shown {
            self.stage = match self.stage {
                Stage::Waiting => Stage::Shown,
                Stage::Silent => Stage::ShownSilent,
                other => other,
            };
        }
        match self.stage {
            Stage::ShownSilent => return Picture::Shown(Motion::Authored),
            Stage::Shown if !self.speech => return Picture::Shown(Motion::Authored),
            Stage::Shown => {
                let motion = self
                    .handoff
                    .and_then(|seen| seen.motion)
                    .filter(|span| span.source <= span.submitted_through)
                    .map_or(Motion::Frozen, Motion::Follow);
                return Picture::Shown(motion);
            }
            _ => {}
        }

        if view == View::Eligible {
            self.eligible_since.get_or_insert(now);
        } else {
            self.eligible_since = None;
        }
        if !self.speech {
            return if view == View::Eligible {
                Picture::Authored
            } else {
                Picture::Wait
            };
        }
        let onset = self.handoff.map(Handoff::onset);
        if let Some(at) = onset
            && now >= at
            && self.eligible_since.is_none_or(|eligible| eligible > at)
        {
            self.silent();
        }
        if view != View::Eligible {
            return Picture::Wait;
        }
        if self.stage == Stage::Silent {
            Picture::Silent
        } else if rebuilding {
            Picture::Wait
        } else {
            onset.map_or(Picture::Wait, Picture::At)
        }
    }

    fn observe_route(&mut self, now: Duration, playback: &mut Playback) -> bool {
        if self.rebuild_until.is_some_and(|at| now >= at) {
            self.silent();
        }
        if let Route::Live(observed) = playback.route
            && let Some(generation) = self.generation
            && (observed < generation || (observed == generation && self.route_retired))
        {
            playback.route = if self.route_retired {
                Route::Rebuilding
            } else {
                Route::Live(generation)
            };
            playback.head = Head::WaitingForPcm;
            playback.handoff = None;
        }
        match playback.route {
            Route::Rebuilding => {
                self.route_retired = true;
                self.rebuild_until
                    .get_or_insert(now.saturating_add(REBUILD_GRACE));
                self.handoff = None;
                true
            }
            Route::Live(generation) => {
                self.route_retired = false;
                if self.generation != Some(generation) {
                    self.generation = Some(generation);
                    self.handoff = None;
                }
                if let Some(seen) = playback
                    .handoff
                    .filter(|seen| seen.generation == generation)
                {
                    self.rebuild_until = None;
                    self.handoff = Some(seen);
                    self.submitted = true;
                    if self.audio == Gate::Held {
                        self.audio = Gate::Allowed;
                    }
                }
                self.rebuild_until.is_some()
            }
            Route::Virtual | Route::Muted | Route::NoWorker => {
                self.route_retired = false;
                self.handoff = None;
                self.rebuild_until = None;
                false
            }
        }
    }

    fn silent(&mut self) {
        match self.stage {
            Stage::Waiting => self.stage = Stage::Silent,
            Stage::Shown => self.stage = Stage::ShownSilent,
            _ => {}
        }
    }

    fn drop_unsent(&mut self) {
        if !self.submitted {
            self.audio = Gate::Dropped;
        }
    }
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::SceneId;
    use kumochi_scene::{Ms, WireId};

    use super::*;

    const STEP: Assignment = Assignment {
        session: 1,
        output: OutputGeneration(2),
        run: RunId(4),
        segment: SegmentId(3),
        step: StepRef {
            scene: SceneId(5),
            index: 1,
        },
    };

    const fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn live() -> Playback {
        Playback {
            route: Route::Live(7),
            head: Head::WaitingForPcm,
            handoff: None,
            synthesis: None,
        }
    }

    fn observed(generation: u64, source: u64) -> Handoff {
        Handoff {
            generation,
            first: ms(10),
            predicted: Some(ms(20)),
            latency: Some(ms(5)),
            motion: Some(SourceSpan {
                source,
                at: ms(20),
                submitted_through: source + 24,
            }),
        }
    }

    fn at(core: &mut Conductor, now: u64, view: View, playback: Playback) -> Decision {
        core.update(STEP, ms(now), view, playback)
            .expect("current assignment")
    }

    fn speech() -> Conductor {
        Conductor::new(STEP, &Start::Speech)
    }

    #[test]
    fn submitted_line_is_never_cut_by_conductor() {
        let mut core = speech();
        let playback = Playback {
            handoff: Some(observed(7, 24)),
            head: Head::EligiblePcm,
            ..live()
        };
        assert_eq!(
            at(&mut core, 10, View::Prepared, playback),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::Wait,
            }
        );
        assert_eq!(
            at(&mut core, 21, View::Eligible, playback),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::Silent,
            }
        );
        assert_eq!(
            at(&mut core, 400, View::Fenced, playback),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::Fenced,
            }
        );

        let mut revoked = speech();
        assert_eq!(
            at(
                &mut revoked,
                1,
                View::Eligible,
                Playback {
                    head: Head::Revoked,
                    ..live()
                }
            ),
            Decision {
                audio: Gate::Dropped,
                picture: Picture::Silent
            }
        );
        let mut failed = speech();
        assert_eq!(
            at(&mut failed, 1, View::Fenced, live()),
            Decision {
                audio: Gate::Dropped,
                picture: Picture::Fenced,
            }
        );
    }

    #[test]
    fn pcm_hold_expiry_plays_whole_and_shows_silently() {
        let mut core = speech();
        assert_eq!(at(&mut core, 0, View::Preparing, live()).audio, Gate::Held);
        let ready = Playback {
            head: Head::EligiblePcm,
            ..live()
        };
        assert_eq!(at(&mut core, 100, View::Preparing, ready).audio, Gate::Held);
        assert_eq!(at(&mut core, 349, View::Preparing, ready).audio, Gate::Held);
        assert_eq!(
            at(&mut core, 350, View::Eligible, ready),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::Silent,
            }
        );
        assert_eq!(
            at(&mut core, 351, View::Shown, ready).picture,
            Picture::Shown(Motion::Authored)
        );
        assert_eq!(
            at(&mut core, 352, View::Eligible, ready).picture,
            Picture::Shown(Motion::Authored)
        );

        let after = Start::After {
            step: WireId::try_from("earlier".to_owned()).expect("valid step"),
            ms: Ms::try_from(50).expect("valid delay"),
        };
        for start in [Start::Now, after] {
            let mut core = Conductor::new(STEP, &start);
            assert_eq!(
                at(&mut core, 0, View::Prepared, live()),
                Decision {
                    audio: Gate::Allowed,
                    picture: Picture::Wait,
                }
            );
            assert_eq!(
                at(&mut core, 50, View::Eligible, live()).picture,
                Picture::Authored
            );
            assert_eq!(
                at(&mut core, 51, View::Shown, live()).picture,
                Picture::Shown(Motion::Authored)
            );
        }
    }

    #[test]
    fn missing_estimate_keeps_sound() {
        for (prediction, latency, want) in [
            (Some(ms(20)), Some(ms(5)), ms(20)),
            (None, Some(ms(5)), ms(15)),
            (None, None, ms(10)),
        ] {
            let mut core = speech();
            assert_eq!(
                at(&mut core, 9, View::Eligible, live()).picture,
                Picture::Wait
            );
            let handoff = Handoff {
                predicted: prediction,
                latency,
                ..observed(7, 0)
            };
            assert_eq!(
                at(
                    &mut core,
                    10,
                    View::Eligible,
                    Playback {
                        handoff: Some(handoff),
                        ..live()
                    }
                ),
                Decision {
                    audio: Gate::Allowed,
                    picture: Picture::At(want)
                }
            );
        }
        let mut core = speech();
        assert_eq!(
            at(
                &mut core,
                10,
                View::Eligible,
                Playback {
                    route: Route::Muted,
                    ..live()
                }
            ),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::Silent
            }
        );
        assert_eq!(
            at(
                &mut core,
                11,
                View::Eligible,
                Playback {
                    handoff: Some(observed(7, 0)),
                    ..live()
                }
            )
            .picture,
            Picture::Silent
        );
    }

    #[test]
    fn rebuild_grace_is_separate_and_does_not_reshow() {
        let after_gap = SourceSpan {
            source: 240,
            at: ms(279),
            submitted_through: 264,
        };
        let shown = |motion| Decision {
            audio: Gate::Allowed,
            picture: Picture::Shown(motion),
        };
        for (open_at, return_at, expected) in [
            (None, 279, Motion::Follow(after_gap)),
            (None, 280, Motion::Authored),
            (Some(278), 279, Motion::Follow(after_gap)),
            (Some(279), 281, Motion::Authored),
        ] {
            let mut core = speech();
            let old = Playback {
                handoff: Some(observed(7, 24)),
                ..live()
            };
            assert_eq!(
                at(&mut core, 10, View::Eligible, old).picture,
                Picture::At(ms(20))
            );
            assert_eq!(
                at(&mut core, 20, View::Shown, old).picture,
                Picture::Shown(Motion::Follow(observed(7, 24).motion.expect("old span")))
            );
            let lost = Playback {
                route: Route::Rebuilding,
                ..live()
            };
            for now in [30, 278] {
                assert_eq!(at(&mut core, now, View::Shown, lost), shown(Motion::Frozen));
            }
            let opened = Playback {
                route: Route::Live(8),
                ..live()
            };
            if let Some(open_at) = open_at {
                assert_eq!(
                    at(&mut core, open_at, View::Shown, opened),
                    shown(Motion::Frozen)
                );
                if return_at > 280 {
                    assert_eq!(
                        at(&mut core, 280, View::Shown, opened),
                        shown(Motion::Authored)
                    );
                }
            }
            let recovered = Playback {
                handoff: Some(Handoff {
                    first: ms(return_at),
                    predicted: None,
                    motion: Some(after_gap),
                    ..observed(8, 240)
                }),
                ..opened
            };
            assert_eq!(
                at(&mut core, return_at, View::Eligible, recovered),
                shown(expected)
            );
            for (now, stale) in [
                (
                    281,
                    Playback {
                        handoff: old.handoff,
                        ..recovered
                    },
                ),
                (
                    282,
                    Playback {
                        handoff: None,
                        ..recovered
                    },
                ),
                (
                    283,
                    Playback {
                        head: Head::Played,
                        ..old
                    },
                ),
            ] {
                assert_eq!(at(&mut core, now, View::Shown, stale), shown(expected));
            }
        }
        for route in [Route::Virtual, Route::NoWorker] {
            let mut core = speech();
            assert_eq!(
                at(&mut core, 0, View::Eligible, Playback { route, ..live() }).picture,
                Picture::Silent
            );
        }
    }

    #[test]
    fn synthesis_cap_is_not_a_conductor_timer() {
        let mut core = speech();
        let capped = Playback {
            synthesis: Some(SynthesisFinal {
                segment: STEP.segment,
                samples: 24_000 * kumochi_protocol::message::MAX_SEGMENT_SECONDS,
            }),
            ..live()
        };
        assert_eq!(
            at(&mut core, 0, View::Eligible, capped).picture,
            Picture::Wait
        );
        assert_eq!(
            at(&mut core, 31_000, View::Eligible, capped).picture,
            Picture::Wait
        );
        let handoff = Handoff {
            first: ms(31_000),
            predicted: Some(ms(31_010)),
            ..observed(7, 0)
        };
        assert_eq!(
            at(
                &mut core,
                31_000,
                View::Eligible,
                Playback {
                    handoff: Some(handoff),
                    ..capped
                }
            ),
            Decision {
                audio: Gate::Allowed,
                picture: Picture::At(ms(31_010))
            }
        );
        assert_eq!(
            at(
                &mut core,
                61_010,
                View::Eligible,
                Playback {
                    head: Head::Played,
                    ..capped
                }
            )
            .picture,
            Picture::Silent
        );
    }

    #[test]
    fn watchdog_terminal_releases_first_pcm_wait() {
        let mut core = speech();
        let preamble = Playback {
            head: Head::AdvancingPredecessor,
            ..live()
        };
        assert_eq!(
            at(&mut core, 0, View::Eligible, preamble).picture,
            Picture::Wait
        );
        assert_eq!(
            at(&mut core, 6_000, View::Eligible, preamble).picture,
            Picture::Wait
        );
        let other_final = Playback {
            synthesis: Some(SynthesisFinal {
                segment: SegmentId(2),
                samples: 0,
            }),
            ..live()
        };
        assert_eq!(
            at(&mut core, 6_001, View::Eligible, other_final).picture,
            Picture::Wait
        );
        let credited_stall = Playback {
            synthesis: Some(SynthesisFinal {
                segment: STEP.segment,
                samples: 0,
            }),
            ..live()
        };
        assert_eq!(
            at(&mut core, 11_001, View::Eligible, credited_stall).picture,
            Picture::Silent
        );
    }

    #[test]
    fn stop_beats_early_allow_and_stale_observations() {
        let mut core = speech();
        assert_eq!(
            at(&mut core, 0, View::Prepared, live()).audio,
            Gate::Allowed
        );
        assert_eq!(
            at(&mut core, 1, View::Fenced, live()),
            Decision {
                audio: Gate::Dropped,
                picture: Picture::Fenced,
            }
        );
        let stale = Assignment { session: 0, ..STEP };
        assert_eq!(core.update(stale, ms(2), View::Eligible, live()), None);
        let late = Playback {
            handoff: Some(observed(9, 24)),
            route: Route::Live(9),
            ..live()
        };
        assert_eq!(
            at(&mut core, 3, View::Eligible, late),
            Decision {
                audio: Gate::Dropped,
                picture: Picture::Fenced,
            }
        );
        let mut core = speech();
        let waiting = Playback {
            head: Head::EligiblePcm,
            ..live()
        };
        assert_eq!(at(&mut core, 0, View::Preparing, waiting).audio, Gate::Held);
        assert_eq!(
            at(&mut core, 250, View::Fenced, waiting),
            Decision {
                audio: Gate::Dropped,
                picture: Picture::Fenced,
            }
        );
    }
}
