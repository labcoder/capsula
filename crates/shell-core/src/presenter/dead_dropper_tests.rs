//! A dead dropper stops scene intake and new preparation; the watchdog still fails what waits.

use std::time::Instant;

use kumochi_protocol::message::{DataToShell, SceneFailure, SceneId};
use kumochi_scene::deferred::Deferred;

use super::{
    Effect, FRAME, GENERATION, LIVE, RUN, Shaper, begin, clear_drawing, commit, controls,
    described, failed, header, jobs, live, ms, only_job, presenter, recovering_exhausted, rect,
    snapshot,
};

const OTHER: SceneId = SceneId(21);

/// Every pairing of a current or stale connection with a running or dead dropper.
const CASES: [(u64, bool); 4] = [
    (GENERATION, true),
    (GENERATION, false),
    (GENERATION + 1, true),
    (GENERATION + 1, false),
];

#[test]
fn live_frames_apply_only_on_the_current_connection_while_the_dropper_runs() {
    for (generation, alive) in CASES {
        let case = format!("generation {generation}, dropper running {alive}");
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        presenter.dropper = Some(alive);
        let frames = [
            DataToShell::SceneBegin {
                scene: OTHER,
                header: header(),
            },
            DataToShell::SceneStepCommit {
                run: RUN,
                scene: LIVE,
                index: 0,
                step: rect("a"),
                speech: None,
            },
            DataToShell::ScenesRemoved { scenes: vec![LIVE] },
        ];
        let effects: Vec<_> = frames
            .into_iter()
            .map(|frame| {
                presenter
                    .data(generation, Deferred::new(frame, 0), FRAME, now)
                    .expect("a legal frame")
            })
            .collect();
        if generation == GENERATION && alive {
            let [begun, committed, removed]: [Vec<Effect>; 3] =
                effects.try_into().expect("three frames");
            assert!(matches!(begun[..], [Effect::Credit(FRAME)]), "{case}");
            assert!(removed.is_empty(), "the removal waits behind the admission");
            let [Effect::Admit(admission)] = <[Effect; 1]>::try_from(committed).expect("one job")
            else {
                panic!("an admission");
            };
            let effects = presenter
                .completed(admission.run(), now)
                .expect("the admission");
            let credited: u32 = effects
                .iter()
                .map(|effect| match effect {
                    Effect::Credit(bytes) => *bytes,
                    other => panic!("unexpected effect {other:?}: {case}"),
                })
                .sum();
            assert_eq!(credited, 2 * FRAME, "{case}");
            assert_eq!(
                presenter.scenes.keys().copied().collect::<Vec<_>>(),
                [OTHER],
                "{case}"
            );
        } else {
            assert!(effects.iter().all(Vec::is_empty), "{case}");
            assert_eq!(
                presenter.scenes.keys().copied().collect::<Vec<_>>(),
                [LIVE],
                "{case}"
            );
            assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 0, "{case}");
            assert_eq!(presenter.workers(), 0, "{case}");
        }
        if generation == GENERATION {
            assert_eq!(presenter.refused, !alive, "{case}");
        }
    }
}

#[test]
fn snapshot_chunks_assemble_only_on_the_current_connection_while_the_dropper_runs() {
    for (generation, alive) in CASES {
        let case = format!("generation {generation}, dropper running {alive}");
        let mut presenter = presenter();
        let now = Instant::now();
        assert!(described(&mut presenter, &[LIVE], now).is_empty(), "{case}");
        presenter.dropper = Some(alive);
        let effects = presenter
            .data(
                generation,
                Deferred::new(snapshot(LIVE, rect("r")), 0),
                FRAME,
                now,
            )
            .expect("a legal chunk");
        if generation == GENERATION && alive {
            let (mut credit, mut recovery) = (0, 0);
            for effect in effects {
                match effect {
                    Effect::Credit(bytes) => credit += bytes,
                    Effect::Recover(_) => recovery += 1,
                    other => panic!("unexpected effect {other:?}: {case}"),
                }
            }
            assert_eq!((credit, recovery), (FRAME, 1), "{case}");
        } else {
            assert!(effects.is_empty(), "{case}");
            assert_eq!(presenter.workers(), 0, "{case}");
            assert_eq!(presenter.deadline(), None, "{case}");
        }
        if generation == GENERATION {
            assert_eq!(presenter.refused, !alive, "{case}");
        }
    }
}

#[test]
fn a_dead_dropper_starts_no_queued_step_and_the_watchdog_fails_it() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    let queued = commit(&mut presenter, LIVE, 1, rect("b"), now).expect("a step");
    assert!(jobs(queued).is_empty());
    presenter.dropper = Some(false);
    let effects = presenter
        .completed(first.run(&mut Shaper), ms(now, 100))
        .expect("an answer");
    assert!(effects.is_empty());
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.deadline(), Some(ms(now, 350)));
    let effects = presenter.tick(ms(now, 350)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 1, SceneFailure::Timeout)]
    );
    assert!(jobs(effects).is_empty());
    assert_eq!(presenter.deadline(), None);
}

#[test]
fn a_dead_dropper_starts_no_waiting_recovery_and_its_deadline_clears_it() {
    let (mut presenter, now, mut zombies, recovered) = recovering_exhausted();
    presenter.dropper = Some(false);
    let zombie = zombies.pop().expect("a held slot");
    let effects = presenter
        .completed(zombie.run(&mut Shaper), ms(now, 1_000))
        .expect("a stale answer");
    assert!(effects.is_empty());
    assert_eq!(presenter.workers(), 7);
    assert_eq!(presenter.deadline(), Some(ms(now, 2_000)));
    let effects = presenter.tick(ms(now, 2_000)).expect("a tick");
    assert_eq!(controls(&effects), [&clear_drawing(vec![recovered])]);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Recover(_)))
    );
    assert!(!presenter.holds_scenes());
}
