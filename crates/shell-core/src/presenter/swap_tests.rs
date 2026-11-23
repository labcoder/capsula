use std::time::Instant;

use kumochi_protocol::message::{DataToShell, SceneId};
use kumochi_scene::{Step, limits::MAX_RESIDENT_BYTES, motion::FrameItem, record::Start};

use super::{
    Broken, Charged, GENERATION, LIVE, Presenter, RUN, Shaper, Witness, begin, bound_everywhere,
    commit_as, controls, data, live, ms, only_job, paint_shown, recovering, rect, step,
};

const NEXT: SceneId = SceneId(30);

#[test]
fn owned_stop_preserves_unbound_and_unrelated_retirement_but_discards_its_bound_successor() {
    for (owner, keep) in [
        (None, true),
        (Some(RUN), false),
        (Some(kumochi_protocol::message::RunId(9)), true),
    ] {
        let (mut presenter, now) = fixture(&[LIVE]);
        let old = picture(&presenter);
        replace(&mut presenter, &[LIVE], NEXT, now);
        begin(&mut presenter, NEXT, now);
        if let Some(run) = owner {
            let job = only_job(
                commit_as(&mut presenter, run, NEXT, 0, rect("successor"), now)
                    .expect("bound successor"),
            );
            presenter
                .completed(job.run(&mut Shaper), now)
                .expect("prepared successor");
        }
        presenter.stop_owned(RUN, now).expect("owned stop");
        let expected = if keep { old } else { Vec::new() };
        assert_eq!(picture(&presenter), expected, "owner {owner:?}");
        if owner.is_none() {
            let effects = presenter
                .data_with_disposition(
                    GENERATION,
                    kumochi_scene::deferred::Deferred::new(
                        super::commit_frame(NEXT, 0, rect("stopped")),
                        0,
                    ),
                    super::FRAME,
                    crate::runs::RunDisposition::Stopped,
                    now,
                )
                .expect("first tagged commit");
            assert!(picture(&presenter).is_empty());
            let effects = presenter
                .completed(super::only_admission(effects).run(), now)
                .expect("mirrored tagged successor");
            assert!(super::jobs(effects).is_empty());
        }
    }
}

fn fixture(ids: &[SceneId]) -> (Presenter, Instant) {
    let (mut presenter, now, job) = recovering(ids.iter().map(|id| (*id, rect("old"))).collect());
    presenter.stage = Box::new(Witness);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovery");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    presenter.frames.borrow_mut().shown = true;
    (presenter, now)
}

fn replace(presenter: &mut Presenter, ids: &[SceneId], next: SceneId, now: Instant) {
    let effects = data(
        presenter,
        DataToShell::ScenesReplaced {
            scenes: ids.to_vec(),
            successor: next,
        },
        now,
    )
    .expect("replacement marker");
    assert!(controls(&effects).is_empty());
}

fn picture(presenter: &Presenter) -> Vec<Vec<FrameItem>> {
    let mut pictures = Vec::new();
    assert!(!presenter.frames.borrow().paint(|frame, _| {
        if !frame.items().is_empty() {
            pictures.push(frame.items().to_vec());
        }
        Ok::<_, ()>(())
    }));
    pictures
}

fn prepare(presenter: &mut Presenter, index: u32, drawing: Step, now: Instant) {
    let effects = commit_as(presenter, RUN, NEXT, index, drawing, now).expect("admitted");
    let effects = presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    assert!(controls(&effects).is_empty());
}

#[test]
fn replacement_keeps_the_exact_old_sample_until_one_atomic_successor_selection() {
    let (mut presenter, now) = fixture(&[LIVE]);
    let old = picture(&presenter);
    let bytes = presenter.residency() - super::super::Activation::BYTES;
    replace(&mut presenter, &[LIVE], NEXT, now);
    assert_eq!(presenter.global.usage().objects, 0);
    assert_eq!(presenter.global.usage().scenes, 0);
    assert_eq!(presenter.residency(), bytes);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    assert!(
        presenter
            .sample_at(ms(now, 10))
            .expect("a sample")
            .0
            .visible
    );
    assert_eq!(picture(&presenter), old);
    assert_eq!(presenter.frames.borrow().observations(), [None; 4]);
    assert!(bound_everywhere(&presenter));
    begin(&mut presenter, NEXT, now);
    prepare(
        &mut presenter,
        0,
        step(
            r#"{"step":{"id":"new","put":[{"shape":{"id":"new","kind":"rect","box":[40,0,30,20],"enter":"none"}}]}}"#,
        ),
        now,
    );
    presenter.advance(GENERATION, now).expect("activation");
    assert_eq!(picture(&presenter), old);
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    assert_ne!(picture(&presenter), old);
    assert_eq!(picture(&presenter).len(), 1);
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert!(bound_everywhere(&presenter));
    assert!(presenter.drawn());
}

#[test]
#[expect(
    clippy::float_cmp,
    reason = "the authored entrance endpoints must be exactly zero and one"
)]
fn a_zero_alpha_first_sample_hands_off_and_its_paint_starts_the_entrance_clock() {
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    begin(&mut presenter, NEXT, now);
    prepare(
        &mut presenter,
        0,
        step(
            r#"{"step":{"id":"fade","put":[{"shape":{"id":"new","kind":"rect","box":[0,0,10,10],"enter":"fade","dur":100}}]}}"#,
        ),
        now,
    );
    presenter.advance(GENERATION, now).expect("activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    let picture_at_start = picture(&presenter);
    let [items] = picture_at_start.as_slice() else {
        panic!("only the successor paints")
    };
    let [first] = items.as_slice() else {
        panic!("one successor shape")
    };
    assert_eq!(first.opacity, 0.0);
    paint_shown(&mut presenter, now);
    assert!(
        presenter
            .sample_at(ms(now, 100))
            .expect("a sample")
            .0
            .visible
    );
    let picture_at_end = picture(&presenter);
    let [items] = picture_at_end.as_slice() else {
        panic!("only the successor paints")
    };
    let [last] = items.as_slice() else {
        panic!("one successor shape")
    };
    assert_eq!(last.opacity, 1.0);
}

#[test]
fn a_survivor_does_not_trigger_handoff_and_an_empty_prefix_can_advance() {
    let (mut presenter, now) = fixture(&[SceneId(10), LIVE]);
    let old = picture(&presenter);
    replace(&mut presenter, &[LIVE], NEXT, now);
    begin(&mut presenter, NEXT, now);
    prepare(
        &mut presenter,
        0,
        step(r#"{"step":{"id":"empty","start":"now"}}"#),
        now,
    );
    let second = step(
        r#"{"step":{"id":"new","start":"now","put":[{"shape":{"id":"new","kind":"rect","box":[40,0,30,20],"enter":"none"}}]}}"#,
    );
    prepare(&mut presenter, 1, second, now);
    presenter
        .advance(GENERATION, now)
        .expect("empty activation");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    assert_eq!(picture(&presenter), old);
    presenter.advance(GENERATION, now).expect("next prefix");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    let current = picture(&presenter);
    let [survivor, successor] = current.as_slice() else {
        panic!("only the survivor and successor paint");
    };
    assert_eq!(survivor, &old[0]);
    assert_ne!(successor, &old[1], "the frozen picture no longer paints");
    let shown: Vec<_> = presenter
        .take_shown(GENERATION)
        .into_iter()
        .flatten()
        .map(|ack| (ack.generation, ack.run, ack.scene, ack.index))
        .collect();
    assert_eq!(shown, [(GENERATION, RUN, NEXT, 1)]);
    assert_eq!(presenter.scenes[&NEXT].view.activated(), 2);
}

#[test]
fn the_selected_epoch_waits_only_for_its_first_paint_or_the_original_expiry() {
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    begin(&mut presenter, NEXT, now);
    prepare(
        &mut presenter,
        0,
        step(
            r#"{"step":{"id":"first","put":[{"shape":{"id":"first","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
        ),
        now,
    );
    let mut second = rect("second");
    second.start = Start::Now;
    prepare(&mut presenter, 1, second, now);
    presenter.advance(GENERATION, now).expect("first");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    assert!(!presenter.animating(now));
    assert_eq!(presenter.next_wake(now), None);
    presenter
        .advance(GENERATION, ms(now, 249))
        .expect("guarded");
    assert_eq!(presenter.scenes[&NEXT].view.activated(), 1);
    assert_eq!(presenter.next_wake(ms(now, 249)), None);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    presenter
        .advance(GENERATION, ms(now, 250))
        .expect("expired guard");
    assert_eq!(presenter.scenes[&NEXT].view.activated(), 2);
}

#[test]
fn a_missing_successor_expires_without_a_failure_and_cannot_revive_old_ids() {
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    assert!(
        presenter
            .sample_at(ms(now, 249))
            .expect("a sample")
            .0
            .visible
    );
    let effects = presenter.tick(ms(now, 250)).expect("expiry");
    assert!(controls(&effects).is_empty());
    assert!(
        !presenter
            .sample_at(ms(now, 250))
            .expect("a sample")
            .0
            .visible
    );
    assert_eq!(presenter.residency(), 0);
    assert_eq!(presenter.deadline(), None);
    begin(&mut presenter, LIVE, ms(now, 251));
    assert!(!presenter.scenes.contains_key(&LIVE));
    assert!(
        !presenter
            .sample_at(ms(now, 251))
            .expect("a sample")
            .0
            .visible
    );
}

#[test]
fn a_later_admission_does_not_reset_visual_expiry_and_its_own_failure_stays_final() {
    use kumochi_protocol::message::ControlToBrain;
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    begin(&mut presenter, NEXT, now);
    let effects = super::arrive(
        &mut presenter,
        super::commit_frame(NEXT, 0, rect("late")),
        ms(now, 30),
    )
    .expect("admission");
    let job = super::only_admission(effects);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let effects = presenter.tick(ms(now, 250)).expect("visual expiry only");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.deadline(), Some(ms(now, 280)));
    let effects = presenter.tick(ms(now, 280)).expect("admission expiry");
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: NEXT }]
    );
    let effects = presenter
        .completed(job.run(), ms(now, 281))
        .expect("late answer");
    assert!(controls(&effects).is_empty());
    assert!(!presenter.scenes.contains_key(&NEXT));
    assert!(
        !presenter
            .sample_at(ms(now, 281))
            .expect("a sample")
            .0
            .visible
    );
}

#[test]
fn stop_before_successor_begin_names_only_the_successor_and_drains_its_late_commit() {
    use kumochi_protocol::message::ControlToBrain;
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    let effects = presenter
        .stop(RUN, now)
        .expect("stop without successor run");
    let [ControlToBrain::StopScenes { run, cuts }] = controls(&effects).as_slice() else {
        panic!("one covering stop");
    };
    assert_eq!(*run, RUN);
    assert_eq!(
        cuts.iter()
            .map(|cut| (cut.scene, cut.cut.activated))
            .collect::<Vec<_>>(),
        [(NEXT, 0)]
    );
    begin(&mut presenter, NEXT, now);
    let effects =
        commit_as(&mut presenter, RUN, NEXT, 0, rect("late"), now).expect("fenced commit");
    assert!(super::jobs(effects).is_empty());
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    assert!(picture(&presenter).is_empty());
    assert!(presenter.take_shown(GENERATION).iter().all(Option::is_none));
}

#[test]
fn clear_before_successor_begin_preserves_the_clear_intent_for_late_traffic() {
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    presenter.clear(now).expect("clear retiring-only ink");
    begin(&mut presenter, NEXT, now);
    let effects = commit_as(&mut presenter, RUN, NEXT, 0, rect("late"), now).expect("cleared step");
    assert!(super::jobs(effects).is_empty());
    assert!(!presenter.scenes.contains_key(&NEXT));
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn a_refused_host_reports_only_current_members_and_the_unbegun_successor_as_failures() {
    use kumochi_protocol::message::ControlToBrain;
    let (mut presenter, now) = fixture(&[SceneId(10), LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    let effects = presenter.refuse_all(now).expect("refused host");
    assert_eq!(
        controls(&effects),
        [
            &ControlToBrain::SceneFailed { scene: SceneId(10) },
            &ControlToBrain::SceneFailed { scene: NEXT },
        ]
    );
    begin(&mut presenter, NEXT, now);
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn retained_and_incoming_objects_share_the_cap_without_refusing_the_new_scene() {
    let labels = |count: usize| {
        step(&format!(
            r#"{{"step":{{"id":"many","put":[{}]}}}}"#,
            (0..count)
                .map(|id| format!(r#"{{"text":{{"id":"n{id}","at":[0,0],"text":"x"}}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        ))
    };
    for extra in [0, 1] {
        let (mut presenter, now, job) = recovering(vec![
            (SceneId(10), labels(450 + extra)),
            (LIVE, labels(450)),
        ]);
        presenter
            .completed(job.run(&mut Shaper), now)
            .expect("900 outgoing objects");
        assert!(presenter.sample_at(now).expect("a sample").0.visible);
        let old = picture(&presenter);
        assert_eq!(old.len(), 2);
        replace(&mut presenter, &[SceneId(10), LIVE], NEXT, now);
        begin(&mut presenter, NEXT, now);
        prepare(&mut presenter, 0, labels(600), now);
        assert_eq!(presenter.global.usage().objects, 600);
        let expected = if extra == 0 { old } else { Vec::new() };
        assert_eq!(picture(&presenter), expected, "extra objects: {extra}");
        presenter
            .advance(GENERATION, now)
            .expect("new scene fits after retirement");
        assert_eq!(presenter.scenes[&NEXT].view.activated(), 1);
        assert!(presenter.sample_at(now).expect("a sample").0.visible);
    }
}

#[test]
fn stop_clear_disconnect_removal_and_a_second_marker_dispose_the_first_attempt() {
    for terminal in 0..5 {
        let (mut presenter, now) = fixture(&[LIVE]);
        replace(&mut presenter, &[LIVE], NEXT, now);
        match terminal {
            0 => {
                presenter.stop(RUN, now).expect("stop before run binding");
            }
            1 => {
                presenter.clear(now).expect("clear");
            }
            2 => presenter.disconnected(),
            3 => {
                data(
                    &mut presenter,
                    DataToShell::ScenesRemoved { scenes: vec![NEXT] },
                    now,
                )
                .expect("remove");
            }
            _ => replace(&mut presenter, &[NEXT], SceneId(40), now),
        }
        assert!(!presenter.sample_at(now).expect("a sample").0.visible);
        assert_eq!(presenter.residency(), 0, "terminal case {terminal}");
        assert!(picture(&presenter).is_empty());
        assert!(presenter.take_shown(GENERATION).iter().all(Option::is_none));
    }
}

#[test]
fn malformed_and_stale_markers_cannot_take_current_owners() {
    for victims in [
        vec![],
        vec![LIVE; 2],
        vec![NEXT],
        (1..=5).map(SceneId).collect(),
    ] {
        let (mut presenter, now) = fixture(&[LIVE]);
        let old = picture(&presenter);
        assert_eq!(
            data(
                &mut presenter,
                DataToShell::ScenesReplaced {
                    scenes: victims,
                    successor: NEXT
                },
                now
            )
            .err(),
            Some(Broken::Scenes)
        );
        assert_eq!(picture(&presenter), old);
        assert_eq!(
            presenter.global.usage().scenes,
            1,
            "the current scene keeps its slot"
        );
    }
    let (mut presenter, now) = fixture(&[LIVE]);
    let old = picture(&presenter);
    let effects = presenter
        .data(
            GENERATION + 1,
            kumochi_scene::deferred::Deferred::new(
                DataToShell::ScenesReplaced {
                    scenes: vec![LIVE],
                    successor: NEXT,
                },
                0,
            ),
            40,
            now,
        )
        .expect("stale frame");
    assert!(controls(&effects).is_empty());
    assert_eq!(picture(&presenter), old);
    let effects = commit_as(&mut presenter, RUN, LIVE, 1, rect("still-current"), now)
        .expect("the current scene is still writable");
    assert_eq!(only_job(effects).scene(), LIVE);
}

#[test]
fn residency_at_the_limit_keeps_retirement_and_one_over_falls_back_without_refusal() {
    let (mut empty, now) = live();
    begin(&mut empty, NEXT, now);
    let incoming = empty.residency();
    for extra in [0, 1] {
        let (mut presenter, now) = fixture(&[LIVE]);
        let old = picture(&presenter);
        let outgoing = presenter.residency() - super::super::Activation::BYTES;
        replace(&mut presenter, &[LIVE], NEXT, now);
        presenter.stage = Box::new(Charged(MAX_RESIDENT_BYTES - incoming - outgoing + extra));
        let effects = begin(&mut presenter, NEXT, now);
        assert!(controls(&effects).is_empty());
        let expected = if extra == 0 { old } else { Vec::new() };
        assert_eq!(picture(&presenter), expected, "extra bytes: {extra}");
        assert!(presenter.residency() <= MAX_RESIDENT_BYTES);
        assert_eq!(
            data(
                &mut presenter,
                DataToShell::SceneBegin {
                    scene: NEXT,
                    header: super::header()
                },
                now,
            )
            .err(),
            Some(Broken::Order),
            "a duplicate header is refused because the successor was admitted"
        );
    }
}

#[test]
fn a_refused_successor_header_reports_failure_without_guessing_a_run() {
    use kumochi_protocol::message::ControlToBrain;
    let (mut presenter, now) = fixture(&[LIVE]);
    replace(&mut presenter, &[LIVE], NEXT, now);
    presenter.stage = Box::new(Charged(MAX_RESIDENT_BYTES));
    let effects = begin(&mut presenter, NEXT, now);
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: NEXT }]
    );
    assert!(!presenter.holds_scenes());
    presenter.stage = Box::new(Witness);
    let late = begin(&mut presenter, NEXT, now);
    assert!(controls(&late).is_empty());
    assert!(
        !presenter.holds_scenes(),
        "a later valid header cannot revive the refusal"
    );
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    assert!(picture(&presenter).is_empty());
    assert!(presenter.take_shown(GENERATION).iter().all(Option::is_none));
}

#[test]
fn combined_paint_work_at_the_limit_keeps_the_hold_and_one_over_relinquishes_it() {
    use kumochi_scene::display::PaintWork;
    for extra in [0, 1] {
        let (mut presenter, now) = fixture(&[LIVE]);
        replace(&mut presenter, &[LIVE], NEXT, now);
        presenter.make_room(
            presenter.residency(),
            PaintWork {
                fill_vertices: PaintWork::LIMITS.fill_vertices + extra,
                ..PaintWork::default()
            },
        );
        assert_eq!(presenter.frames.borrow().retirement.is_some(), extra == 0);
        assert_eq!(presenter.global.usage().objects, 0);
    }
}

#[test]
fn frozen_text_is_redacted_by_its_outer_owners_and_reconnect_cannot_recover_it() {
    let marker = "PRIVATE_SWAP_SENTINEL";
    let (mut presenter, now, job) = recovering(vec![(LIVE, super::label("old", marker))]);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("text recovery");
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    replace(&mut presenter, &[LIVE], NEXT, now);
    let debug = format!("{presenter:?} {:?}", presenter.frames.borrow().retirement);
    assert!(!debug.contains(marker));
    assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
    presenter.connected(GENERATION + 1);
    assert_eq!(presenter.residency(), 0);
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
}

#[test]
fn a_cancelled_old_worker_cannot_replace_the_frozen_sample_or_acknowledge_it() {
    let (mut presenter, now) = fixture(&[LIVE]);
    let old = picture(&presenter);
    let effects = commit_as(&mut presenter, RUN, LIVE, 1, rect("queued"), now).expect("old work");
    let job = only_job(effects);
    replace(&mut presenter, &[LIVE], NEXT, now);
    let effects = presenter
        .completed(job.run(&mut Shaper), now)
        .expect("late old answer");
    assert!(controls(&effects).is_empty());
    assert!(presenter.sample_at(now).expect("a sample").0.visible);
    assert_eq!(picture(&presenter), old);
    assert!(presenter.take_shown(GENERATION).iter().all(Option::is_none));
}

#[test]
fn stop_or_clear_after_handoff_cannot_deliver_a_deferred_paint_acknowledgement() {
    use kumochi_protocol::message::ControlToBrain;
    for clear in [false, true] {
        let (mut presenter, now) = fixture(&[LIVE]);
        replace(&mut presenter, &[LIVE], NEXT, now);
        begin(&mut presenter, NEXT, now);
        prepare(
            &mut presenter,
            0,
            step(
                r#"{"step":{"id":"first","put":[{"shape":{"id":"first","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
            ),
            now,
        );
        presenter
            .advance(GENERATION, now)
            .expect("selected successor");
        assert!(presenter.sample_at(now).expect("a sample").0.visible);
        assert!(!presenter.frames.borrow().paint(|_, _| Ok::<_, ()>(())));
        let effects = if clear {
            presenter.clear(now)
        } else {
            presenter.stop(RUN, now)
        }
        .expect("terminal local action");
        if clear {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![NEXT]
                }]
            );
        } else {
            let controls = controls(&effects);
            let [ControlToBrain::StopScenes { run, cuts }] = controls.as_slice() else {
                panic!("one covering stop");
            };
            assert_eq!(*run, RUN);
            assert_eq!(cuts.len(), 1);
            assert_eq!(cuts[0].scene, NEXT);
            assert_eq!(cuts[0].cut.activated, 1);
            assert_eq!(
                &*cuts[0].cut.revealed,
                &[kumochi_scene::Handle::new(1, 0).expect("first object")]
            );
        }
        assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    }
}

#[test]
fn an_incoming_success_prepared_before_a_second_marker_or_reconnect_stays_stale() {
    let after = SceneId(40);
    for reconnect in [false, true] {
        let (mut presenter, now) = fixture(&[LIVE]);
        replace(&mut presenter, &[LIVE], NEXT, now);
        begin(&mut presenter, NEXT, now);
        let effects =
            commit_as(&mut presenter, RUN, NEXT, 0, rect("late"), now).expect("incoming work");
        let answer = only_job(effects).run(&mut Shaper);
        if reconnect {
            presenter.connected(GENERATION + 1);
        } else {
            replace(&mut presenter, &[NEXT], after, now);
            begin(&mut presenter, after, now);
            let effects = commit_as(&mut presenter, RUN, after, 0, rect("current"), now)
                .expect("current work");
            presenter
                .completed(only_job(effects).run(&mut Shaper), now)
                .expect("current answer");
            presenter
                .advance(GENERATION, now)
                .expect("current activation");
            assert!(presenter.sample_at(now).expect("a sample").0.visible);
        }
        let before = picture(&presenter);
        let mut shown = [None; 4];
        if !reconnect {
            shown[0] = Some(super::super::activation::Shown {
                generation: GENERATION,
                run: RUN,
                scene: after,
                index: 0,
            });
        }
        assert_eq!(presenter.take_shown(GENERATION), shown);
        let effects = presenter
            .completed(answer, now)
            .expect("late incoming answer");
        assert!(effects.is_empty());
        assert_eq!(picture(&presenter), before);
        assert_eq!(
            presenter.take_shown(if reconnect {
                GENERATION + 1
            } else {
                GENERATION
            }),
            [None; 4]
        );
        if reconnect {
            assert!(!presenter.sample_at(now).expect("a sample").0.visible);
            assert_eq!(presenter.residency(), 0);
        } else {
            assert_eq!(presenter.scenes[&after].view.activated(), 1);
        }
    }
}

#[test]
fn four_retiring_frames_never_paint_a_fifth_and_their_failures_emit_no_old_ack() {
    let ids = [SceneId(1), SceneId(2), SceneId(3), SceneId(4)];
    let (mut presenter, now) = fixture(&ids);
    replace(&mut presenter, &ids, NEXT, now);
    begin(&mut presenter, NEXT, now);
    assert_eq!(picture(&presenter).len(), 4);
    assert_eq!(presenter.frames.borrow().observations(), [None; 4]);
    assert!(presenter.frames.borrow().paint(|_, _| Err(())));
    assert!(controls(&presenter.paint_failed()).is_empty());
    assert_eq!(presenter.frames.borrow().retirement_bytes(), 0);
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
}
