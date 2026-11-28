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
    bridge.began(SceneId(2)).unwrap();
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
    bridge.state.lock().unwrap().unacked = Some((old.session, receipt));
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
        bridge.began(SceneId(scene)).unwrap();
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
    assert_eq!(bridge.began(next.scene), Err(()));
    assert!(bridge.retiring(
        &[first.scene],
        None,
        RetirementLane::Scenes,
        NonZeroU64::MIN
    ));
    bridge.began(next.scene).unwrap();
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
        bridge.began(key.scene).unwrap();
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
        bridge.began(old).unwrap();
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
        bridge.began(successor).unwrap();
        if marker_first {
            assert!(bridge.promised(promised));
            assert!(bridge.announce(promised));
        }
        assert_eq!(bridge.unassigned(next_key), Ok(true));
        assert_eq!(bridge.assignment(old_key), Err(()));
        assert_eq!(bridge.assignment(next_key), Ok(None));
    }
}

/// Four retiring scenes fill the absence table. B's promise waits for the presenter to consume
/// their retirement, then B keeps its fallback through `RunEnd` and the next run's rollover.
#[test]
fn a_full_absence_table_keeps_the_unbound_step_s_fallback_through_run_end_and_rollover() {
    let bridge = Narrated::new();
    let old = DrawKey {
        run: RunId(7),
        scene: SceneId(1),
        index: 0,
    };
    for scene in 1..=4 {
        bridge.began(SceneId(scene)).unwrap();
        for index in 0..64 {
            assert_eq!(
                bridge.unassigned(DrawKey {
                    scene: SceneId(scene),
                    index,
                    ..old
                }),
                Ok(true)
            );
        }
    }
    let b = Assignment {
        session: 1,
        output: OutputGeneration(4),
        run: RunId(8),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(5),
            index: 0,
        },
    };
    assert!(bridge.retiring(
        &[SceneId(1), SceneId(2), SceneId(3), SceneId(4)],
        Some(b.step.scene),
        RetirementLane::Scenes,
        NonZeroU64::MIN
    ));
    bridge.began(b.step.scene).unwrap();
    let mut consumed = false;
    while !bridge.can_promise(b.run).unwrap() {
        assert!(!consumed, "only the retirement frees B's slot");
        bridge.retirement_consumed(RetirementLane::Scenes);
        consumed = true;
    }
    assert!(bridge.promised(b));
    assert!(bridge.announce(b));
    bridge.run_end(b.run);
    if !consumed {
        bridge.retirement_consumed(RetirementLane::Scenes);
    }
    assert_eq!(
        bridge.assignment(draw_key(b)),
        Ok(None),
        "B shows on its authored timing"
    );
    bridge.state.lock().unwrap().handed_off = true;
    let c = Assignment {
        run: RunId(9),
        step: StepRef {
            scene: b.step.scene,
            index: 1,
        },
        ..b
    };
    assert!(bridge.can_promise(c.run).unwrap());
    assert!(bridge.promised(c));
    assert_eq!(
        bridge.assignment(draw_key(b)),
        Ok(None),
        "the fallback outlives the rollover"
    );
}

#[test]
fn a_removed_scene_s_unbound_step_never_falls_back() {
    for marker_first in [false, true] {
        let bridge = Narrated::new();
        let b = Assignment {
            session: 1,
            output: OutputGeneration(4),
            run: RunId(8),
            segment: SegmentId(0),
            step: StepRef {
                scene: SceneId(5),
                index: 0,
            },
        };
        bridge.began(b.step.scene).unwrap();
        let removal = |bridge: &Narrated| {
            assert!(bridge.retiring(
                &[b.step.scene],
                None,
                RetirementLane::Scenes,
                NonZeroU64::MIN
            ));
        };
        if marker_first {
            removal(&bridge);
        }
        assert!(bridge.promised(b));
        assert!(bridge.announce(b));
        if !marker_first {
            removal(&bridge);
        }
        bridge.run_end(b.run);
        assert_eq!(
            bridge.assignment(draw_key(b)),
            Err(()),
            "a removed step never paints"
        );
        bridge.retirement_consumed(RetirementLane::Scenes);
        assert_eq!(bridge.assignment(draw_key(b)), Err(()));
    }
}

/// Three authored scenes and a fourth of promised steps fill the table. Replacing the fourth
/// frees its promises' reservations, so the successor's first commit is admitted.
#[test]
fn a_replaced_scene_s_promises_free_their_reservations_for_the_successor() {
    let bridge = Narrated::new();
    let run = RunId(7);
    for scene in 1..=4 {
        bridge.began(SceneId(scene)).unwrap();
    }
    for scene in 1..=3 {
        for index in 0..64 {
            assert_eq!(
                bridge.unassigned(DrawKey {
                    run,
                    scene: SceneId(scene),
                    index,
                }),
                Ok(true)
            );
        }
    }
    for index in 0..64 {
        assert!(bridge.promised(Assignment {
            session: 1,
            output: OutputGeneration(4),
            run,
            segment: SegmentId(index),
            step: StepRef {
                scene: SceneId(4),
                index,
            },
        }));
    }
    let successor = DrawKey {
        run,
        scene: SceneId(5),
        index: 0,
    };
    assert!(bridge.retiring(
        &[SceneId(4)],
        Some(successor.scene),
        RetirementLane::Scenes,
        NonZeroU64::MIN
    ));
    bridge.began(successor.scene).unwrap();
    bridge.retirement_consumed(RetirementLane::Scenes);
    assert_eq!(
        bridge.unassigned(successor),
        Ok(true),
        "the successor's first commit is admitted"
    );
    assert_eq!(bridge.can_promise(run), Ok(true));
}

/// Each zero cut releases its scene, so a fifth begin after four cuts holds, and a late promise
/// for a cut scene is retired.
#[test]
fn a_zero_cut_releases_membership_and_retires_its_scene_s_late_promise() {
    let bridge = Narrated::new();
    for (scene, ordinal) in (1..=5).zip(1..) {
        assert_eq!(bridge.began(SceneId(scene)), Ok(()), "scene {scene} begins");
        assert!(bridge.retiring(
            &[SceneId(scene)],
            None,
            RetirementLane::Cut,
            NonZeroU64::new(ordinal).unwrap()
        ));
    }
    let late = Assignment {
        session: 1,
        output: OutputGeneration(4),
        run: RunId(8),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(5),
            index: 0,
        },
    };
    assert!(bridge.promised(late));
    assert!(bridge.announce(late));
    assert_eq!(
        bridge.assignment(draw_key(late)),
        Err(()),
        "a cut scene's late promise never paints"
    );
}

/// A zero cut, from a failed step for example, before a natural `RunEnd` retires the unbound
/// promise, so it keeps no fallback.
#[test]
fn a_zero_cut_retires_an_existing_unbound_promise_before_run_end() {
    let bridge = Narrated::new();
    let a = Assignment {
        session: 1,
        output: OutputGeneration(4),
        run: RunId(8),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(1),
            index: 0,
        },
    };
    bridge.began(a.step.scene).unwrap();
    assert!(bridge.promised(a));
    assert!(bridge.announce(a));
    assert!(bridge.retiring(&[a.step.scene], None, RetirementLane::Cut, NonZeroU64::MIN));
    bridge.run_end(a.run);
    assert_eq!(
        bridge.assignment(draw_key(a)),
        Err(()),
        "a cut step never paints"
    );
    assert_eq!(bridge.reserved(), 0, "and reserves no fallback");
}

#[test]
fn every_outstanding_removal_retires_its_late_promises() {
    let bridge = Narrated::new();
    for (scene, ordinal) in [(SceneId(1), NonZeroU64::MIN), (SceneId(2), NonZeroU64::MAX)] {
        bridge.began(scene).unwrap();
        assert!(bridge.retiring(&[scene], None, RetirementLane::Scenes, ordinal));
    }
    let late = Assignment {
        session: 1,
        output: OutputGeneration(4),
        run: RunId(8),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(1),
            index: 0,
        },
    };
    assert!(bridge.promised(late));
    assert!(bridge.announce(late));
    bridge.run_end(late.run);
    assert_eq!(
        bridge.assignment(draw_key(late)),
        Err(()),
        "the first marker's victim never paints"
    );
}
