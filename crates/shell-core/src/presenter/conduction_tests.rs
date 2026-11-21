use std::time::{Duration, Instant};
use std::{cell::Cell, rc::Rc};

use kumochi_protocol::message::{DataToShell, OutputGeneration, SceneId, SegmentId, StepRef};
use kumochi_scene::{Step, text::Bounded};

use super::{
    GENERATION, LIVE, PrepareJob, Presenter, RUN, Shaper, begin, commit_as, data, live, ms,
    only_job, paint_shown, step,
};
use crate::speaker::conductor::{
    Assignment, Binding, Motion, Picture, PictureLevel, PictureState, RouteStamp, View, ViewLevel,
};

fn assignment(scene: SceneId, index: u32) -> Assignment {
    Assignment {
        session: GENERATION,
        output: OutputGeneration(2),
        run: RUN,
        segment: SegmentId(index),
        step: StepRef { scene, index },
    }
}

fn drawing() -> Step {
    step(
        r#"{"step":{"id":"s","start":"speech","say":"words","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
    )
}

fn fixture<const N: usize>(offered: [Assignment; N]) -> (Presenter, Instant) {
    let (mut presenter, now) = live();
    presenter.start_conduction(
        now,
        move |key| {
            Ok(offered.into_iter().find(|a| {
                a.run == key.run && a.step.scene == key.scene && a.step.index == key.index
            }))
        },
        |_| {},
    );
    (presenter, now)
}

fn submit(presenter: &mut Presenter, scene: SceneId, now: Instant) -> PrepareJob {
    begin(presenter, scene, now);
    only_job(commit_as(presenter, RUN, scene, 0, drawing(), now).expect("commit"))
}

fn prepared(presenter: &mut Presenter, job: PrepareJob, at: Instant) {
    assert!(
        presenter
            .completed(job.run(&mut Shaper), at)
            .expect("prepare")
            .is_empty()
    );
}

fn report(
    presenter: &mut Presenter,
    binding: Binding,
    route: RouteStamp,
    state: PictureState,
    at: Instant,
) {
    presenter.conductor_picture(
        Ok(Some(PictureLevel {
            binding,
            route,
            state,
        })),
        at,
    );
}

fn owned(
    presenter: &mut Presenter,
    binding: Binding,
    route: RouteStamp,
    picture: Picture,
    at: Instant,
) {
    report(presenter, binding, route, PictureState::Owned(picture), at);
}

fn bound(presenter: &Presenter, scene: SceneId) -> u32 {
    presenter.scenes[&scene].view.activated()
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn authenticated_silent_step_advances_then_narration_waits_for_its_source() {
    authenticated_narration(NarratedCase::Onset);
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn authenticated_removed_scene_discards_its_pending_unbound_decision() {
    authenticated_narration(NarratedCase::Removed);
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn authenticated_same_run_replacement_preserves_the_pair_with_and_without_credit() {
    for (credit, contend) in [
        (0, false),
        (kumochi_protocol::message::DATA_WINDOW, false),
        (0, true),
    ] {
        authenticated_narration(NarratedCase::Replaced { credit, contend });
    }
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn authenticated_replacement_chain_does_not_paint_the_earliest_victim_with_the_successor() {
    authenticated_narration(NarratedCase::Chain);
}

#[cfg(all(windows, debug_assertions))]
#[test]
fn authenticated_stopped_narration_keeps_the_painted_prefix_until_silent_replacement() {
    authenticated_narration(NarratedCase::StoppedReplacement);
}

#[cfg(all(windows, debug_assertions))]
#[derive(Clone, Copy)]
enum NarratedCase {
    Onset,
    Removed,
    Replaced { credit: u32, contend: bool },
    Chain,
    StoppedReplacement,
}

#[cfg(all(windows, debug_assertions))]
#[expect(
    clippy::too_many_lines,
    reason = "one authenticated pair through its real producer, fake callback and presenter"
)]
fn authenticated_narration(case: NarratedCase) {
    use crate::link::{LinkEvent, fixture};
    use crate::presenter::{Effect, fixture as presenter_fixture};
    use kumochi_protocol::message::{
        DataToBrain, RunId, ScreenChoice, SpeechDecision, SpeechDispatch, ThreadId,
    };
    use std::time::Duration;

    let _device = fixture::serialize_playback();
    let (id, hello, producer, brain) = match case {
        NarratedCase::Replaced { credit, .. } => {
            kumochi_brain::ipc::offline_narrated_replacement_peer(credit)
        }
        NarratedCase::Chain => kumochi_brain::ipc::offline_narrated_replacement_chain_peer(),
        NarratedCase::StoppedReplacement => {
            kumochi_brain::ipc::offline_narrated_stopped_replacement_peer()
        }
        NarratedCase::Onset | NarratedCase::Removed => {
            kumochi_brain::ipc::offline_narrated_peer(40)
        }
    };
    let pipe = fixture::authenticated_narrated(id, hello);
    let events = fixture::patient(pipe.events);
    let session = fixture::connected(fixture::next(&events));
    let generation = session.generation();
    let mut presenter = presenter_fixture::presenter();
    presenter.connected(generation);
    session.attach_narration(&mut presenter);
    let submit = |turn| {
        pipe.handle.send_data(
            &session,
            DataToBrain::TextSubmit {
                turn,
                thread: Some(ThreadId(1)),
                screen: ScreenChoice::None,
                text: "draw".into(),
            },
        )
    };
    let withheld = Cell::new(matches!(case, NarratedCase::Chain).then_some(0_u32));
    let grant = |bytes| {
        if let Some(previous) = withheld.get() {
            withheld.set(Some(previous + bytes));
        } else {
            session.grant(bytes);
        }
    };
    let settle = |presenter: &mut Presenter, mut effects: Vec<Effect>, now| {
        effects.reverse();
        while let Some(effect) = effects.pop() {
            let more = match effect {
                Effect::Credit(bytes) => {
                    grant(bytes);
                    continue;
                }
                Effect::Control(message) => {
                    assert!(pipe.handle.send_control(&session, message));
                    continue;
                }
                Effect::Admit(job) => presenter.completed(job.run(), now),
                Effect::Prepare(job) => presenter.completed(job.run(&mut Shaper), now),
                Effect::Recover(job) => presenter.completed(job.run(&mut Shaper), now),
                Effect::ConsumptionReady { scene, index } => {
                    presenter.consumption_ready(generation, scene, index, now)
                }
            }
            .expect("settle the presenter");
            effects.extend(more.into_iter().rev());
        }
    };
    let mut settings = false;
    while !settings || !presenter.membership_ready() {
        let event = fixture::next(&events);
        match event {
            LinkEvent::Settings { .. } => settings = true,
            LinkEvent::Scenes(_) => {
                let now = Instant::now();
                let effects = presenter
                    .described(generation, &Bounded::default(), now)
                    .unwrap();
                settle(&mut presenter, effects, now);
            }
            LinkEvent::ScenesV1 { scenes, .. } => {
                let now = Instant::now();
                let effects = presenter.described(generation, &scenes, now).unwrap();
                settle(&mut presenter, effects, now);
            }
            _ => {}
        }
    }
    assert!(!submit(1));
    assert!(pipe.playback.try_recv().is_err());
    if matches!(case, NarratedCase::StoppedReplacement) {
        producer.hold_after_scene_prefix();
    } else if !matches!(case, NarratedCase::Chain) {
        producer.allow_data_credit();
    }
    producer.release_run();
    producer.release_ack();
    let wait = |label: &str, check: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !check() {
            assert!(Instant::now() < deadline, "fixture did not reach {label}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait("exact registration", &mut || {
        pipe.handle.admits_text(Some(&session))
    });
    assert!(submit(1));
    if matches!(case, NarratedCase::StoppedReplacement) {
        use kumochi_protocol::message::{ControlToBrain, DATA_WINDOW, Phase, RunOrigin};
        use kumochi_scene::display::Prim;

        let now = Instant::now();
        let deliver = |presenter: &mut Presenter, event| {
            match event {
                LinkEvent::SceneData {
                    generation: incoming,
                    frame,
                    bytes,
                } => {
                    assert_eq!(incoming, generation);
                    let effects = presenter.data(generation, frame, bytes, now).unwrap();
                    settle(presenter, effects, now);
                }
                LinkEvent::Data { bytes, .. } => grant(bytes),
                _ => {}
            }
            let effects = presenter.advance(generation, now).unwrap();
            settle(presenter, effects, now);
        };
        let picture = |presenter: &Presenter| {
            let mut items = Vec::new();
            assert!(!presenter.frames.borrow().paint(|frame, _| {
                items.extend_from_slice(frame.items());
                Ok::<_, ()>(())
            }));
            items
        };
        let acknowledge = |presenter: &mut Presenter, run, scene| {
            presenter.begin_painted(now);
            let [receipt] = presenter
                .take_shown(generation)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .try_into()
                .expect("one accepted step paint");
            assert_eq!(
                (
                    receipt.generation,
                    receipt.run,
                    receipt.scene,
                    receipt.index
                ),
                (generation, run, scene, 0)
            );
            assert!(pipe.handle.send_control(
                &session,
                ControlToBrain::StepShown {
                    run,
                    scene,
                    index: 0
                }
            ));
        };
        let (stopped, old) = loop {
            let event = fixture::next(&events);
            let prefix = match &event {
                LinkEvent::SceneData { frame, .. } => match &**frame {
                    DataToShell::SceneStepCommit {
                        run,
                        scene,
                        index: 0,
                        speech: None,
                        ..
                    } => Some((*run, *scene)),
                    _ => None,
                },
                _ => None,
            };
            deliver(&mut presenter, event);
            if let Some(prefix) = prefix {
                break prefix;
            }
        };
        assert_eq!(bound(&presenter, old), 1);
        let (sample, effects) = presenter.sample_at(now).unwrap();
        settle(&mut presenter, effects, now);
        assert!(sample.visible);
        presenter.frames.borrow_mut().shown = true;
        let prefix = picture(&presenter);
        assert_eq!(prefix.len(), 1);
        acknowledge(&mut presenter, stopped, old);
        let prefix_bytes = producer.sent_bytes();
        wait(
            "written prefix and credit-held keyed narration",
            &mut || {
                producer.data_blocked()
                    && producer.held_narration().is_some()
                    && producer.data_credits() == (prefix_bytes, 0)
                    && producer.shown() == [(stopped, old, 0)]
            },
        );
        let (step, dispatch) = producer
            .held_narration()
            .expect("exact promised announcement");
        assert_eq!(
            (step, dispatch.run),
            (
                StepRef {
                    scene: old,
                    index: 1
                },
                stopped
            )
        );
        assert_eq!(producer.held_tail(), (0, 0));
        assert!(pipe.playback.try_recv().is_err());

        let effects = presenter.stop(stopped, now).unwrap();
        let [ControlToBrain::StopScenes { run, cuts }] = super::controls(&effects).as_slice()
        else {
            panic!("one real StopScenes for the painted prefix");
        };
        let [cut] = &cuts[..] else {
            panic!("one preserved scene cut")
        };
        assert_eq!((*run, cut.scene, cut.cut.activated), (stopped, old, 1));
        settle(&mut presenter, effects, now);
        wait(
            "local output Stop after the Brain Stop was sent",
            &mut || pipe.handle.output().stop(generation, stopped).is_ok(),
        );
        wait(
            "accepted empty quiet and one queued stopped RunEnd",
            &mut || {
                producer.accepted_quiet() == Some((stopped, None))
                    && producer.held_tail() == (1, 0)
                    && pipe.handle.admits_text(Some(&session))
            },
        );
        assert_eq!(producer.stop(), stopped);
        assert_eq!(producer.sent_bytes(), prefix_bytes);
        assert_eq!(producer.data_credits(), (prefix_bytes, 0));
        assert_eq!(picture(&presenter), prefix);

        assert!(submit(2));
        let next = loop {
            match fixture::next(&events) {
                LinkEvent::RunState {
                    run,
                    origin: Some(RunOrigin::Typed { turn: 2 }),
                    ..
                } => break run,
                LinkEvent::RunState {
                    phase: Phase::Failed,
                    ..
                } => panic!("the same pair must admit the successor"),
                LinkEvent::Data { .. } | LinkEvent::SceneData { .. } => {
                    panic!("writer credit must still be held")
                }
                _ => {}
            }
        };
        assert!(next > stopped);
        wait(
            "silent successor commit queued behind the same held credit",
            &mut || producer.held_successor().is_some(),
        );
        let (run, fresh) = producer.held_successor().unwrap();
        assert_eq!(run, next);
        assert_ne!(fresh, old);
        assert!(
            !pipe.handle.admits_text(Some(&session)),
            "the stopped tail still owns the predecessor slot"
        );
        assert_eq!(producer.sent_bytes(), prefix_bytes);
        assert_eq!(producer.data_credits(), (prefix_bytes, 0));
        assert_eq!(picture(&presenter), prefix);

        producer.allow_data_credit();
        session.grant(DATA_WINDOW);
        let mut replaced = false;
        let mut promised = false;
        let mut painted = false;
        let mut ended = Vec::new();
        while ended.last() != Some(&next) {
            let event = fixture::next(&events);
            let mut paint_successor = false;
            match &event {
                LinkEvent::SceneData { frame, .. } => match &**frame {
                    DataToShell::ScenesRemoved { .. } => {
                        panic!(
                            "standalone ScenesRemoved erases the painted prefix before replacement"
                        );
                    }
                    DataToShell::ScenesReplaced { scenes, successor } => {
                        assert!(!std::mem::replace(&mut replaced, true));
                        assert_eq!((scenes.as_slice(), *successor), ([old].as_slice(), fresh));
                        assert!(
                            !promised && ended.is_empty(),
                            "boundary precedes the stopped tail"
                        );
                    }
                    DataToShell::SceneStepCommit {
                        run,
                        scene,
                        index,
                        speech,
                        ..
                    } if *run == stopped => {
                        assert!(replaced && ended.is_empty());
                        assert!(!std::mem::replace(&mut promised, true));
                        let speech = speech.expect("the unwritten promise survives Stop");
                        assert_eq!(
                            (*scene, *index, speech.output_generation, speech.segment),
                            (old, step.index, dispatch.output, dispatch.segment)
                        );
                    }
                    DataToShell::SceneStepCommit {
                        run,
                        scene,
                        index,
                        speech,
                        ..
                    } => {
                        assert!(replaced);
                        assert_eq!((*run, *scene, *index, *speech), (next, fresh, 0, None));
                        assert!(!painted);
                        paint_successor = true;
                    }
                    _ => {}
                },
                LinkEvent::Data {
                    message: DataToShell::RunEnd { run },
                    ..
                } => {
                    assert!(promised, "RunEnd cannot strand the promise");
                    ended.push(*run);
                    assert!(ended.len() <= 2);
                }
                LinkEvent::RunState {
                    phase: Phase::Failed,
                    ..
                } => panic!("the replacement run must succeed"),
                _ => {}
            }
            deliver(&mut presenter, event);
            if !painted {
                assert_eq!(
                    picture(&presenter),
                    prefix,
                    "only fresh accepted paint replaces the prefix"
                );
                assert!(presenter.take_shown(generation).iter().all(Option::is_none));
            }
            if paint_successor {
                assert_eq!(bound(&presenter, fresh), 1);
                let (sample, effects) = presenter.sample_at(now).unwrap();
                settle(&mut presenter, effects, now);
                assert!(sample.visible);
                let items = picture(&presenter);
                let [item] = items.as_slice() else {
                    panic!("only the successor is painted")
                };
                assert!(
                    matches!(&item.primitives[..], [Prim::Rect { fill: Some(fill), .. }] if fill.rgb.get() == 0x00_eeee)
                );
                acknowledge(&mut presenter, next, fresh);
                painted = true;
            }
        }
        assert!(replaced && promised && painted);
        assert_eq!(ended, [stopped, next]);
        let seed = u64::from(DATA_WINDOW);
        wait(
            "both paint receipts and the exact returned credit",
            &mut || {
                let sent = producer.sent_bytes();
                producer.data_credits() == (sent + seed, sent - prefix_bytes + seed)
                    && producer.shown() == [(stopped, old, 0), (next, fresh, 0)]
                    && pipe.handle.admits_text(Some(&session))
            },
        );
        assert_eq!(
            producer.held_tail(),
            (1, 0),
            "Stop removed the queued announcement; one RunEnd settled the tail"
        );
        assert_eq!(producer.speech_decision(), None);
        assert!(producer.received_retirements(stopped).is_empty());
        assert_eq!(producer.heard(), 0);
        assert!(pipe.playback.try_recv().is_err());
        assert!(session.open());
        pipe.handle.end(&session);
        assert!(pipe.link.join().unwrap());
        assert!(brain.join().unwrap());
        return;
    }
    if matches!(case, NarratedCase::Chain) {
        use kumochi_scene::display::Prim;

        let paint = |presenter: &mut Presenter, now| {
            let (sample, effects) = presenter.sample_at(now).unwrap();
            settle(presenter, effects, now);
            assert!(sample.visible);
            let mut fills = Vec::new();
            presenter.frames.borrow_mut().shown = true;
            assert!(!presenter.frames.borrow().paint(|frame, _| {
                fills.extend(frame.items().iter().flat_map(|item| {
                    item.primitives
                        .iter()
                        .filter_map(|primitive| match primitive {
                            Prim::Rect {
                                fill: Some(fill), ..
                            } => Some(fill.rgb.get()),
                            _ => None,
                        })
                }));
                Ok::<_, ()>(())
            }));
            presenter.begin_painted(now);
            fills
        };
        let deliver = |presenter: &mut Presenter, event, now| {
            match event {
                LinkEvent::SceneData {
                    generation: incoming,
                    frame,
                    bytes,
                } => {
                    assert_eq!(incoming, generation);
                    let effects = presenter.data(generation, frame, bytes, now).unwrap();
                    settle(presenter, effects, now);
                }
                LinkEvent::Data { bytes, .. } => grant(bytes),
                _ => {}
            }
            let effects = presenter.advance(generation, now).unwrap();
            settle(presenter, effects, now);
        };
        let now = Instant::now();
        while presenter.scenes.len() != 3
            || !presenter
                .scenes
                .keys()
                .all(|&id| bound(&presenter, id) == 1)
        {
            deliver(&mut presenter, fixture::next(&events), now);
        }
        assert_eq!(
            paint(&mut presenter, now),
            [0xaa_0000, 0x00_bb00, 0x00_00cc]
        );
        let prefix = presenter.take_shown(generation);
        assert_eq!(prefix.iter().flatten().count(), 3);
        let earliest = prefix.into_iter().flatten().next().unwrap().scene;
        let prefix_bytes = withheld
            .get()
            .expect("the initial prefix is credited together");
        assert!(prefix_bytes > 0);
        session.grant(prefix_bytes);
        wait("credit-held replacement chain", &mut || {
            producer.replacement_queued() && producer.data_blocked()
        });
        assert_eq!(producer.data_credits(), (u64::from(prefix_bytes), 0));
        session.grant(kumochi_protocol::message::DATA_WINDOW);
        withheld.set(None);

        let now = Instant::now();
        let mut replacements = Vec::new();
        loop {
            let event = fixture::next(&events);
            if let LinkEvent::SceneData { frame, .. } = &event
                && let DataToShell::ScenesReplaced { scenes, successor } = &**frame
            {
                replacements.push((scenes.clone(), *successor));
            }
            let final_scene = match &event {
                LinkEvent::SceneData { frame, .. } => match &**frame {
                    DataToShell::SceneStepCommit {
                        run,
                        scene,
                        step,
                        speech: None,
                        ..
                    } if step.id.as_str() == "final" => Some((*run, *scene)),
                    _ => None,
                },
                _ => None,
            };
            deliver(&mut presenter, event, now);
            if let Some((run, scene)) = final_scene {
                assert_eq!(bound(&presenter, scene), 1);
                assert_eq!(
                    paint(&mut presenter, now),
                    [0x00_eeee],
                    "the final successor's first paint must not include the earlier eviction victim"
                );
                let shown: Vec<_> = presenter
                    .take_shown(generation)
                    .into_iter()
                    .flatten()
                    .map(|shown| (shown.run, shown.scene, shown.index))
                    .collect();
                assert_eq!(shown, [(run, scene, 0)]);
                let [(evicted, intermediate), (cleared, final_successor)] = replacements.as_slice()
                else {
                    panic!("the two real replacement markers");
                };
                assert_eq!(evicted, &[earliest]);
                assert!(cleared.contains(intermediate));
                assert_eq!(*final_successor, scene);
                break;
            }
        }
        assert!(session.open());
        assert!(pipe.playback.try_recv().is_err());
        pipe.handle.end(&session);
        assert!(pipe.link.join().unwrap());
        assert!(brain.join().unwrap());
        return;
    }
    if let NarratedCase::Replaced {
        credit: initial,
        contend,
    } = case
    {
        wait("real replacement queued", &mut || {
            producer.replacement_queued()
        });
        if initial == 0 {
            wait("real data credit blocked", &mut || producer.data_blocked());
            assert_eq!(producer.data_credits(), (0, 0));
            session.grant(kumochi_protocol::message::DATA_WINDOW);
        }
        let mut boundary = None;
        let mut promised = None;
        let mut painted = None;
        let ended = loop {
            let now = Instant::now();
            match fixture::next(&events) {
                LinkEvent::SceneData {
                    generation: incoming,
                    frame,
                    bytes,
                } => {
                    assert_eq!(incoming, generation);
                    if let DataToShell::ScenesReplaced { scenes, successor } = &*frame {
                        let [old] = scenes.as_slice() else {
                            panic!("one replaced scene")
                        };
                        if !contend {
                            assert!(session.update_narration(&pipe.handle, &mut presenter, now));
                        }
                        let effects = presenter.advance(generation, now).unwrap();
                        settle(&mut presenter, effects, now);
                        if presenter.scenes.contains_key(old) {
                            assert_eq!(
                                bound(&presenter, *old),
                                0,
                                "wire fence beats delayed presenter removal"
                            );
                        }
                        boundary = Some((*old, *successor));
                    }
                    if let DataToShell::SceneStepCommit {
                        run,
                        scene,
                        speech: Some(speech),
                        ..
                    } = &*frame
                    {
                        promised = Some((*run, *scene, *speech));
                    }
                    let effects = presenter.data(generation, frame, bytes, now).unwrap();
                    settle(&mut presenter, effects, now);
                }
                LinkEvent::Data {
                    message: DataToShell::RunEnd { run },
                    bytes,
                    ..
                } => {
                    session.grant(bytes);
                    break run;
                }
                LinkEvent::Data { bytes, .. } => session.grant(bytes),
                _ => {}
            }
            if let Some((old, successor)) = boundary {
                if !contend {
                    assert!(session.update_narration(&pipe.handle, &mut presenter, now));
                }
                let effects = presenter.advance(generation, now).unwrap();
                settle(&mut presenter, effects, now);
                assert!(
                    !presenter.scenes.contains_key(&old),
                    "the protected promise cannot restore its scene"
                );
                if painted.is_none()
                    && presenter.scenes.contains_key(&successor)
                    && bound(&presenter, successor) == 1
                {
                    let (sample, effects) = presenter.sample_at(now).unwrap();
                    settle(&mut presenter, effects, now);
                    assert!(sample.visible);
                    paint_shown(&mut presenter, now);
                    painted = presenter
                        .take_shown(generation)
                        .into_iter()
                        .flatten()
                        .next();
                }
            }
        };
        let (old, successor) = boundary.expect("real replacement frame");
        let (run, promised_scene, promise) = promised.expect("preserved exact old promise");
        assert_eq!((ended, promised_scene), (run, old));
        assert_eq!(
            painted.map(|r| (r.run, r.scene, r.index)),
            Some((run, successor, 0))
        );
        let dispatch = SpeechDispatch {
            run,
            output: promise.output_generation,
            segment: promise.segment,
        };
        if contend {
            fixture::with_outbox_and_output_held(&pipe.handle, &session, || {
                assert!(
                    session.update_narration(&pipe.handle, &mut presenter, Instant::now()),
                    "a busy output lock must defer the unbound Discard, not close the pair"
                );
                assert!(session.open());
                assert_eq!(producer.speech_decision(), None);
            });
            wait("exact Discard after releasing the output lock", &mut || {
                assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
                producer.speech_decision() == Some((dispatch, SpeechDecision::Discard))
            });
        }
        assert_ne!(
            producer.speech_decision(),
            Some((dispatch, SpeechDecision::Allow))
        );
        assert_eq!(producer.heard(), 0);
        assert!(
            pipe.playback.try_recv().is_err(),
            "no fake device opens for revoked empty source"
        );
        let seed = if initial == 0 {
            u64::from(kumochi_protocol::message::DATA_WINDOW)
        } else {
            0
        };
        wait("all exact data credit returned", &mut || {
            let expected = producer.sent_bytes() + seed;
            producer.data_credits() == (expected, expected)
        });
        assert!(session.open(), "replacement must not tear down the pair");
        pipe.handle.end(&session);
        assert!(pipe.link.join().unwrap());
        assert!(brain.join().unwrap());
        return;
    }
    let mut commit = None;
    let (run, scene, promise) = loop {
        let now = Instant::now();
        match fixture::next(&events) {
            LinkEvent::SceneData {
                generation: incoming,
                frame,
                bytes,
            } => {
                assert_eq!(incoming, generation);
                let narrated = match &*frame {
                    DataToShell::SceneStepCommit {
                        run,
                        scene,
                        index: 1,
                        speech: Some(promise),
                        ..
                    } => Some((*run, *scene, *promise)),
                    _ => None,
                };
                let effects = presenter.data(generation, frame, bytes, now).unwrap();
                settle(&mut presenter, effects, now);
                commit = narrated.or(commit);
            }
            LinkEvent::Data { bytes, .. } => session.grant(bytes),
            _ => {}
        }
        if let Some((run, scene, promise)) = commit
            && presenter.scenes.contains_key(&scene)
        {
            break (run, scene, promise);
        }
    };
    assert_eq!(bound(&presenter, scene), 0);
    assert_eq!(producer.heard(), 0);
    assert!(
        pipe.playback.try_recv().is_err(),
        "no callback before local apply and Allow"
    );
    if matches!(case, NarratedCase::Removed) {
        let key = SpeechDispatch {
            run,
            output: promise.output_generation,
            segment: promise.segment,
        };
        let now = Instant::now();
        assert!(session.update_narration(&pipe.handle, &mut presenter, now));
        assert_eq!(
            producer.speech_decision(),
            None,
            "an unresolved view does not discard"
        );
        let effects = presenter
            .data(
                generation,
                kumochi_scene::deferred::Deferred::new(
                    DataToShell::ScenesRemoved {
                        scenes: vec![scene],
                    },
                    0,
                ),
                0,
                now,
            )
            .unwrap();
        settle(&mut presenter, effects, now);
        assert!(!presenter.scenes.contains_key(&scene));
        wait("keyed discard before first binding", &mut || {
            assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
            producer.speech_decision() == Some((key, SpeechDecision::Discard))
        });
        wait(
            "Brain run end after the pending speech decision",
            &mut || {
                assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
                match events.try_recv() {
                    Ok(LinkEvent::Data {
                        message: DataToShell::RunEnd { run: ended },
                        bytes,
                        ..
                    }) => {
                        assert_eq!(ended, run);
                        session.grant(bytes);
                        true
                    }
                    Ok(LinkEvent::Data { bytes, .. } | LinkEvent::SceneData { bytes, .. }) => {
                        session.grant(bytes);
                        false
                    }
                    _ => false,
                }
            },
        );
        assert_eq!(producer.heard(), 0);
        assert!(pipe.playback.try_recv().is_err());
        assert!(session.open());
        pipe.handle.end(&session);
        assert!(pipe.link.join().unwrap());
        assert!(brain.join().unwrap());
        return;
    }
    let now = Instant::now();
    let effects = presenter.advance(generation, now).unwrap();
    settle(&mut presenter, effects, now);
    assert_eq!(
        bound(&presenter, scene),
        1,
        "durable speech absence releases only the silent head"
    );
    assert_eq!(producer.speech_decision(), None);
    let _silent = presenter.sample_at(now).unwrap();
    paint_shown(&mut presenter, now);
    assert_eq!(
        presenter
            .take_shown(generation)
            .into_iter()
            .flatten()
            .next()
            .map(|r| r.index),
        Some(0)
    );
    let mut playback = None;
    wait("physical open after authorization", &mut || {
        assert!(session.update_narration(&pipe.handle, &mut presenter, Instant::now()));
        playback = pipe.playback.try_recv().ok();
        playback.is_some()
    });
    let mut playback = playback.unwrap();
    wait("physical staging", &mut || playback.ready());
    assert_eq!(bound(&presenter, scene), 1, "staging is not onset");
    let mut audible = [0.0; 8];
    playback.render(&mut audible);
    assert!(audible.iter().any(|sample| sample.abs() > 0.01));
    wait("matched scene onset", &mut || {
        let now = Instant::now();
        assert!(session.update_narration(&pipe.handle, &mut presenter, now));
        let effects = presenter.advance(generation, now).unwrap();
        settle(&mut presenter, effects, now);
        bound(&presenter, scene) == 2
    });
    let now = Instant::now();
    let _sample = presenter.sample_at(now).unwrap();
    paint_shown(&mut presenter, now);
    let shown = presenter.take_shown(generation);
    let [receipt] = shown
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    assert_eq!((receipt.run, receipt.scene, receipt.index), (run, scene, 1));
    let effects = presenter.stop_owned(run, now).unwrap();
    settle(&mut presenter, effects, now);
    wait("local Stop after ordered control", &mut || {
        pipe.handle.output().stop(generation, run).is_ok()
    });
    assert!(session.update_narration(&pipe.handle, &mut presenter, now));
    assert_eq!(producer.quiet(), RunId(0), "no original tail callback yet");
    wait("callback fence", &mut || playback.stopped());
    playback.render(&mut [0.0; 480]);
    wait("original callback quiet", &mut || producer.quiet() == run);
    presenter.connected(generation + 1);
    assert!(session.update_narration(&pipe.handle, &mut presenter, now));
    assert!(
        presenter
            .take_shown(generation + 1)
            .iter()
            .all(Option::is_none)
    );
    pipe.handle.end(&session);
    drop(playback);
    assert!(pipe.link.join().unwrap());
    assert!(brain.join().unwrap());
}

#[test]
fn onset_installs_once_and_only_accepted_paint_reports_shown() {
    let key = assignment(LIVE, 0);
    let (mut p, now) = fixture([key]);
    let job = submit(&mut p, LIVE, now);
    let view = p.conductor_view(now).expect("bound while preparing");
    assert_eq!(
        view,
        ViewLevel {
            binding: Binding {
                assignment: key,
                nonce: 1
            },
            speech: true,
            view: View::Preparing
        }
    );
    let binding = view.binding;
    prepared(&mut p, job, ms(now, 2));
    let view = p.conductor_view(ms(now, 2)).expect("eligible");
    assert_eq!(view.binding, binding);
    assert_eq!(view.view, View::Eligible);
    assert_eq!(
        p.next_wake(ms(now, 2)),
        None,
        "no fabricated wake while waiting"
    );

    let at = Duration::from_micros(50_500);
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::At(at),
        ms(now, 3),
    );
    let onset = now + at;
    assert_eq!(p.next_wake(ms(now, 3)), Some(onset));
    p.advance(GENERATION, ms(now, 50)).expect("before onset");
    assert_eq!(bound(&p, LIVE), 0);
    p.advance(GENERATION, onset).expect("onset");
    let epoch = p.frames.borrow().epoch(LIVE);
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(
        p.conductor_view(onset).expect("unpainted").view,
        View::Eligible
    );
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));

    assert!(p.sample_at(onset).expect("sample").0.visible);
    paint_shown(&mut p, onset);
    let acks = p.take_shown(GENERATION);
    let ack = acks
        .into_iter()
        .flatten()
        .next()
        .expect("one accepted paint");
    assert_eq!(
        (ack.generation, ack.run, ack.scene, ack.index),
        (GENERATION, RUN, LIVE, 0)
    );
    assert_eq!(acks.into_iter().flatten().count(), 1);
    assert_eq!(p.conductor_view(onset).expect("shown").view, View::Shown);
    owned(&mut p, binding, RouteStamp::Live(7), Picture::At(at), onset);
    p.advance(GENERATION, onset).expect("duplicate level");
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(p.frames.borrow().epoch(LIVE), epoch);
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
    assert_eq!(p.next_wake(onset), None);
}

#[test]
fn retirement_clears_onset_and_its_watermark_survives_empty_reads() {
    let (mut p, now) = fixture([assignment(LIVE, 0)]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let binding = p.conductor_view(now).expect("binding").binding;
    let at = Picture::At(Duration::from_millis(30));
    owned(&mut p, binding, RouteStamp::Live(7), at, now);
    owned(
        &mut p,
        binding,
        RouteStamp::Retired(7),
        Picture::Wait,
        ms(now, 10),
    );
    p.conductor_picture(Ok(None), ms(now, 11));
    for route in [
        RouteStamp::Unstamped,
        RouteStamp::Live(6),
        RouteStamp::Live(7),
    ] {
        owned(&mut p, binding, route, at, ms(now, 12));
    }
    assert_eq!(p.next_wake(ms(now, 30)), None);
    p.advance(GENERATION, ms(now, 30)).expect("retired onset");
    assert_eq!(bound(&p, LIVE), 0);
    owned(
        &mut p,
        binding,
        RouteStamp::Live(8),
        Picture::At(Duration::from_millis(40)),
        ms(now, 31),
    );
    owned(
        &mut p,
        binding,
        RouteStamp::Live(8),
        Picture::At(Duration::from_millis(41)),
        ms(now, 32),
    );
    assert_eq!(p.next_wake(ms(now, 32)), Some(ms(now, 41)));
}

#[test]
fn no_pcm_and_owner_loss_end_wait_on_the_keyed_terminal_not_a_presenter_timeout() {
    for (route, result) in [
        (RouteStamp::Live(7), PictureState::Owned(Picture::Silent)),
        (RouteStamp::Unstamped, PictureState::Unavailable),
        (RouteStamp::Live(7), PictureState::Unavailable),
    ] {
        let (mut p, now) = fixture([assignment(LIVE, 0)]);
        let job = submit(&mut p, LIVE, now);
        prepared(&mut p, job, now);
        let binding = p.conductor_view(now).expect("binding").binding;
        owned(&mut p, binding, route, Picture::Wait, now);
        let terminal = if route == RouteStamp::Unstamped {
            ms(now, 1)
        } else {
            ms(now, 6700)
        };
        p.advance(GENERATION, terminal).expect("still waiting");
        assert_eq!(bound(&p, LIVE), 0);
        assert_eq!(p.next_wake(terminal), None);
        report(&mut p, binding, RouteStamp::Unstamped, result, terminal);
        if matches!(result, PictureState::Owned(_)) {
            p.advance(GENERATION, terminal)
                .expect("unstamped evidence ignored");
            assert_eq!(bound(&p, LIVE), 0);
            report(&mut p, binding, route, result, terminal);
        }
        p.advance(GENERATION, terminal).expect("authored fallback");
        assert_eq!(bound(&p, LIVE), 1);
        assert_eq!(
            p.conductor_view(terminal).map(|level| level.view),
            matches!(result, PictureState::Owned(_)).then_some(View::Eligible)
        );
        assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
    }
}

#[test]
fn replacement_ignores_late_terminal_even_if_it_reuses_the_new_nonce() {
    let newer = SceneId(LIVE.0 + 1);
    let (mut p, now) = fixture([assignment(LIVE, 0), assignment(newer, 0)]);
    let old_job = submit(&mut p, LIVE, now);
    prepared(&mut p, old_job, now);
    let old = p.conductor_view(now).expect("old binding").binding;
    data(
        &mut p,
        DataToShell::ScenesReplaced {
            scenes: vec![LIVE],
            successor: newer,
        },
        ms(now, 10),
    )
    .expect("replacement");
    let next_job = submit(&mut p, newer, ms(now, 10));
    prepared(&mut p, next_job, ms(now, 11));
    let new = p
        .conductor_view(ms(now, 11))
        .expect("successor binding")
        .binding;
    assert_eq!(new.assignment.step.scene, newer);
    assert_eq!(new.nonce, old.nonce + 1);
    for stale in [
        old,
        Binding {
            nonce: new.nonce,
            ..old
        },
    ] {
        report(
            &mut p,
            stale,
            RouteStamp::Unstamped,
            PictureState::Ended(Picture::Wait),
            ms(now, 12),
        );
    }
    p.advance(GENERATION, ms(now, 12))
        .expect("stale terminal ignored");
    assert_eq!(bound(&p, newer), 0);
    assert_eq!(p.next_wake(ms(now, 12)), None);
    report(
        &mut p,
        new,
        RouteStamp::Unstamped,
        PictureState::Unavailable,
        ms(now, 13),
    );
    p.advance(GENERATION, ms(now, 13))
        .expect("matching terminal");
    assert_eq!(bound(&p, newer), 1);
}

#[test]
fn stable_heads_keep_their_binding_and_a_local_fence_wins_before_handoff() {
    let later = SceneId(LIVE.0 + 1);
    let first = assignment(LIVE, 0);
    let other = assignment(later, 0);
    let (mut p, now) = fixture([other, first]);
    let later_job = submit(&mut p, later, now);
    let earlier_job = submit(&mut p, LIVE, now);
    prepared(&mut p, later_job, now);
    let level = p.conductor_view(now).expect("head selection");
    assert_eq!(level.binding.assignment, first);
    assert_eq!(level.view, View::Preparing);
    prepared(&mut p, earlier_job, ms(now, 1));
    assert_eq!(
        p.conductor_view(ms(now, 1)).expect("still bound").binding,
        level.binding
    );
    p.stop(RUN, ms(now, 2)).expect("local fence");
    assert_eq!(
        p.conductor_view(ms(now, 2)).expect("fence retained").view,
        View::Fenced
    );
    report(
        &mut p,
        level.binding,
        RouteStamp::Retired(7),
        PictureState::Ended(Picture::Authored),
        ms(now, 3),
    );
    p.advance(GENERATION, ms(now, 3)).expect("fenced");
    assert_eq!((bound(&p, LIVE), bound(&p, later)), (0, 0));
    assert_eq!(p.next_wake(ms(now, 3)), None);
    p.disconnected();
    assert_eq!(p.conductor_view(ms(now, 4)), None);
}

#[test]
fn authored_fallback_does_not_replace_a_binding_before_audio_ends() {
    let later = SceneId(LIVE.0 + 1);
    let old = assignment(LIVE, 0);
    let other = assignment(later, 0);
    let (mut p, now) = fixture([old, other]);
    let old_job = submit(&mut p, LIVE, now);
    let other_job = submit(&mut p, later, now);
    prepared(&mut p, old_job, now);
    prepared(&mut p, other_job, now);
    let binding = p.conductor_view(now).expect("old head").binding;
    assert_eq!(binding.assignment, old);

    // The worker released its hold, but hasn't handed off a sample or ended the line.
    owned(&mut p, binding, RouteStamp::Live(7), Picture::Silent, now);
    p.advance(GENERATION, now).expect("authored start");
    assert!(p.sample_at(now).expect("sample").0.visible);
    paint_shown(&mut p, now);
    assert_eq!(p.take_shown(GENERATION).into_iter().flatten().count(), 1);
    assert_eq!(
        p.conductor_view(now).expect("reported paint").view,
        View::Shown
    );
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::Shown(crate::speaker::conductor::Motion::Authored),
        ms(now, 1),
    );
    let level = p.conductor_view(ms(now, 1)).expect("still playing");
    assert_eq!(
        level.binding, binding,
        "fallback cannot replace or drop unsent audio"
    );
    assert_eq!(level.view, View::Shown);
    assert_eq!(bound(&p, later), 0);
}

#[test]
fn natural_audio_handoff_keeps_the_installed_steps_unpainted_receipt() {
    let assigned = assignment(LIVE, 0);
    let (mut p, now) = fixture([assigned]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let binding = p.conductor_view(now).unwrap().binding;
    assert!(
        !p.conductor_handoff(assigned),
        "the waiting head still needs its onset"
    );
    owned(
        &mut p,
        binding,
        RouteStamp::Live(1),
        Picture::At(std::time::Duration::ZERO),
        now,
    );
    p.advance(GENERATION, now).unwrap();
    assert_eq!(bound(&p, LIVE), 1);
    assert!(p.conductor_handoff(assigned));
    assert_eq!(p.conductor_view(now), None);
    assert_eq!(p.take_shown(GENERATION), [None; 4]);
    assert!(p.sample_at(now).unwrap().0.visible);
    paint_shown(&mut p, now);
    assert_eq!(
        p.take_shown(GENERATION)
            .into_iter()
            .flatten()
            .map(|r| (r.run, r.scene, r.index))
            .collect::<Vec<_>>(),
        [(assigned.run, LIVE, 0)]
    );
    assert_eq!(p.take_shown(GENERATION), [None; 4]);
}

#[test]
fn the_next_narrated_step_binds_after_ended_without_waiting_for_paint() {
    let first = assignment(LIVE, 0);
    let second = assignment(LIVE, 1);
    let (mut p, now) = fixture([first, second]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let second_step = step(
        r#"{"step":{"id":"second","start":"speech","say":"next","put":[{"shape":{"id":"next","kind":"rect","box":[10,0,10,10],"enter":"none"}}]}}"#,
    );
    let second_job = only_job(commit_as(&mut p, RUN, LIVE, 1, second_step, now).expect("commit"));
    prepared(&mut p, second_job, now);
    let binding = p.conductor_view(now).expect("first binding").binding;
    assert_eq!(binding.assignment, first);
    report(
        &mut p,
        binding,
        RouteStamp::Live(7),
        PictureState::Ended(Picture::Silent),
        ms(now, 1),
    );
    p.advance(GENERATION, ms(now, 1))
        .expect("ended step may install authored");
    assert_eq!(bound(&p, LIVE), 1);
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
    let level = p
        .conductor_view(ms(now, 1))
        .expect("next head takes the slot");
    assert_eq!(
        level.binding,
        Binding {
            assignment: second,
            nonce: binding.nonce + 1
        }
    );
    assert_eq!(level.view, View::Eligible);
    assert_eq!(p.next_wake(ms(now, 1)), None, "new head has no onset yet");
}

#[test]
fn ended_survives_an_empty_read_before_a_coalesced_presenter_wake() {
    let later = SceneId(LIVE.0 + 1);
    let old = assignment(LIVE, 0);
    let next = assignment(later, 0);
    let (mut p, now) = fixture([old, next]);
    let old_job = submit(&mut p, LIVE, now);
    let later_job = submit(&mut p, later, now);
    prepared(&mut p, old_job, now);
    prepared(&mut p, later_job, now);
    let binding = p.conductor_view(now).expect("old binding").binding;
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::At(Duration::from_millis(80)),
        now,
    );
    report(
        &mut p,
        binding,
        RouteStamp::Live(7),
        PictureState::Ended(Picture::Silent),
        ms(now, 2),
    );
    p.conductor_picture(Ok(None), ms(now, 3));
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::Wait,
        ms(now, 4),
    );
    let level = p.conductor_view(ms(now, 5)).expect("one coalesced view");
    assert_eq!(
        level.binding,
        Binding {
            assignment: next,
            nonce: binding.nonce + 1
        }
    );
    p.advance(GENERATION, ms(now, 5))
        .expect("ended head still takes authored timing");
    assert_eq!((bound(&p, LIVE), bound(&p, later)), (1, 0));
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
    assert_eq!(p.next_wake(ms(now, 5)), None, "old onset didn't leak");
}

#[test]
fn ended_wait_releases_while_preparing_and_shows_on_local_readiness() {
    let key = assignment(LIVE, 0);
    let (mut p, now) = live();
    let lookup = Rc::new(Cell::new(Ok(Some(key))));
    let offered = Rc::clone(&lookup);
    p.start_conduction(now, move |_| offered.get(), |_| {});
    let job = submit(&mut p, LIVE, now);
    let level = p.conductor_view(now).expect("preparing");
    assert_eq!(level.view, View::Preparing);
    report(
        &mut p,
        level.binding,
        RouteStamp::Retired(7),
        PictureState::Ended(Picture::Wait),
        ms(now, 1),
    );
    lookup.set(Err(()));
    assert_eq!(p.conductor_view(ms(now, 1)), None);
    p.advance(GENERATION, ms(now, 1)).expect("still preparing");
    assert_eq!(bound(&p, LIVE), 0);
    prepared(&mut p, job, ms(now, 2));
    p.advance(GENERATION, ms(now, 2))
        .expect("authored start with no new worker message");
    assert_eq!(bound(&p, LIVE), 1);
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
    assert!(p.sample_at(ms(now, 2)).expect("sample").0.visible);
    paint_shown(&mut p, ms(now, 2));
    assert_eq!(p.take_shown(GENERATION).into_iter().flatten().count(), 1);
}

#[test]
fn invalid_ended_payloads_change_neither_the_deadline_nor_the_route() {
    use crate::speaker::conductor::SourceSpan;
    let (mut p, now) = fixture([assignment(LIVE, 0)]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let level = p.conductor_view(now).expect("binding");
    owned(
        &mut p,
        level.binding,
        RouteStamp::Live(7),
        Picture::At(Duration::from_millis(50)),
        now,
    );
    for picture in [
        Picture::At(Duration::from_millis(10)),
        Picture::Shown(Motion::Follow(SourceSpan {
            source: 0,
            at: Duration::from_millis(1),
            submitted_through: 24,
        })),
        Picture::Shown(Motion::Frozen),
        Picture::Fenced,
    ] {
        report(
            &mut p,
            level.binding,
            RouteStamp::Live(8),
            PictureState::Ended(picture),
            ms(now, 1),
        );
        assert_eq!(p.conductor_view(ms(now, 1)), Some(level));
        assert_eq!(p.next_wake(ms(now, 1)), Some(ms(now, 50)));
    }
    owned(
        &mut p,
        level.binding,
        RouteStamp::Live(7),
        Picture::At(Duration::from_millis(60)),
        ms(now, 2),
    );
    assert_eq!(
        p.next_wake(ms(now, 2)),
        Some(ms(now, 60)),
        "no invalid route watermark"
    );
}

#[test]
fn a_local_cut_fences_shown_even_if_the_scene_reopens_before_any_conductor_read() {
    use kumochi_protocol::message::RunId;
    use kumochi_scene::StopCut;

    let old = assignment(LIVE, 0);
    let next = Assignment {
        run: RunId(RUN.0 + 1),
        ..assignment(LIVE, 1)
    };
    let (mut p, now) = fixture([old, next]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let binding = p.conductor_view(now).expect("old head").binding;
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::At(Duration::ZERO),
        now,
    );
    p.advance(GENERATION, now).expect("old onset");
    assert!(p.sample_at(now).expect("sample").0.visible);
    paint_shown(&mut p, now);
    assert_eq!(p.take_shown(GENERATION).into_iter().flatten().count(), 1);
    assert_eq!(p.conductor_view(now).expect("shown").view, View::Shown);

    super::acknowledge(
        &mut p,
        LIVE,
        1,
        StopCut {
            activated: 1,
            revealed: Bounded::default(),
        },
        ms(now, 1),
    )
    .expect("kept cut");
    let next_step = step(
        r#"{"step":{"id":"next","start":"speech","say":"next","put":[{"shape":{"id":"next","kind":"rect","box":[10,0,10,10],"enter":"none"}}]}}"#,
    );
    let next_job =
        only_job(commit_as(&mut p, next.run, LIVE, 1, next_step, ms(now, 2)).expect("reopens"));
    prepared(&mut p, next_job, ms(now, 3));

    let level = p
        .conductor_view(ms(now, 3))
        .expect("new head owns the slot");
    assert_eq!(
        level.binding,
        Binding {
            assignment: next,
            nonce: binding.nonce + 1
        }
    );
    assert_eq!(level.view, View::Eligible);
    report(
        &mut p,
        binding,
        RouteStamp::Live(7),
        PictureState::Ended(Picture::Wait),
        ms(now, 4),
    );
    assert_eq!(p.conductor_view(ms(now, 4)), Some(level));
    p.advance(GENERATION, ms(now, 4)).expect("no new onset");
    assert_eq!(bound(&p, LIVE), 1);
    assert!(p.take_shown(GENERATION).iter().all(Option::is_none));
}

#[test]
fn older_stamped_owner_loss_cannot_detach_the_same_live_binding() {
    let (mut p, now) = fixture([assignment(LIVE, 0)]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let level = p.conductor_view(now).expect("binding");
    owned(
        &mut p,
        level.binding,
        RouteStamp::Live(6),
        Picture::Wait,
        now,
    );
    owned(
        &mut p,
        level.binding,
        RouteStamp::Live(7),
        Picture::At(Duration::from_millis(50)),
        ms(now, 1),
    );
    report(
        &mut p,
        level.binding,
        RouteStamp::Live(6),
        PictureState::Unavailable,
        ms(now, 2),
    );
    assert_eq!(p.conductor_view(ms(now, 2)), Some(level));
    assert_eq!(p.next_wake(ms(now, 2)), Some(ms(now, 50)));
    p.advance(GENERATION, ms(now, 2))
        .expect("stale loss doesn't bypass onset");
    assert_eq!(bound(&p, LIVE), 0);
    report(
        &mut p,
        level.binding,
        RouteStamp::Unstamped,
        PictureState::Unavailable,
        ms(now, 3),
    );
    p.advance(GENERATION, ms(now, 3))
        .expect("keyed unstamped loss permits fallback");
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(p.conductor_view(ms(now, 3)), None);
}

fn now_step(id: &str) -> Step {
    step(&format!(
        r#"{{"step":{{"id":"{id}","start":"now","say":"words","put":[{{"shape":{{"id":"{id}","kind":"rect","box":[0,0,10,10],"enter":"none"}}}}]}}}}"#
    ))
}

#[test]
fn owned_stop_fences_a_shown_run_after_a_newer_authored_install() {
    let (mut p, now) = fixture([assignment(LIVE, 0)]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let binding = p.conductor_view(now).expect("speech owner").binding;
    owned(
        &mut p,
        binding,
        RouteStamp::Live(7),
        Picture::At(Duration::ZERO),
        now,
    );
    p.advance(GENERATION, now).expect("speech install");
    p.sample_at(now).expect("sample");
    paint_shown(&mut p, now);
    assert_eq!(p.take_shown(GENERATION).into_iter().flatten().count(), 1);
    assert_eq!(p.conductor_view(now).expect("shown").view, View::Shown);
    let next = kumochi_protocol::message::RunId(RUN.0 + 1);
    let job = only_job(
        commit_as(&mut p, next, LIVE, 1, now_step("next"), ms(now, 1)).expect("new owned step"),
    );
    prepared(&mut p, job, ms(now, 1));
    p.advance(GENERATION, ms(now, 1))
        .expect("new authored install");
    assert_eq!(bound(&p, LIVE), 2);
    p.stop_owned(RUN, ms(now, 2)).expect("scoped stop");
    report(
        &mut p,
        binding,
        RouteStamp::Live(7),
        PictureState::Ended(Picture::Wait),
        ms(now, 3),
    );
    assert_eq!(
        p.conductor_view(ms(now, 3)),
        Some(ViewLevel {
            binding,
            speech: true,
            view: View::Fenced
        })
    );
    assert_eq!(
        bound(&p, LIVE),
        2,
        "the newer visual is not owned by the stopped run"
    );
}

#[test]
fn narrated_authored_starts_never_bind_even_with_a_free_slot() {
    let (mut p, now) = fixture([assignment(LIVE, 0), assignment(LIVE, 1)]);
    begin(&mut p, LIVE, now);
    let job = only_job(commit_as(&mut p, RUN, LIVE, 0, now_step("anchor"), now).expect("Now step"));
    prepared(&mut p, job, now);
    assert_eq!(
        p.conductor_view(now),
        None,
        "narrated Now isn't a binding candidate"
    );
    p.advance(GENERATION, now)
        .expect("Now installs immediately");
    assert_eq!(bound(&p, LIVE), 1);

    let after = step(r#"{"step":{"id":"after","start":{"after":"anchor","ms":30},"say":"words"}}"#);
    let job = only_job(commit_as(&mut p, RUN, LIVE, 1, after, now).expect("After step"));
    prepared(&mut p, job, now);
    assert_eq!(p.next_wake(now), Some(ms(now, 30)));
    p.advance(GENERATION, ms(now, 29)).expect("before delay");
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(
        p.conductor_view(ms(now, 30)),
        None,
        "narrated After isn't a binding candidate"
    );
    p.advance(GENERATION, ms(now, 30)).expect("after delay");
    assert_eq!(bound(&p, LIVE), 2);
    assert_eq!(p.conductor_view(ms(now, 30)), None);
}

#[test]
fn an_authored_successor_does_not_fence_unpainted_speech_or_credit_its_paint() {
    let (mut p, now) = fixture([assignment(LIVE, 0)]);
    let job = submit(&mut p, LIVE, now);
    prepared(&mut p, job, now);
    let level = p.conductor_view(now).expect("speech binding");
    owned(
        &mut p,
        level.binding,
        RouteStamp::Live(7),
        Picture::Silent,
        now,
    );
    p.advance(GENERATION, now)
        .expect("silent fallback installs without a PCM handoff");
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(p.take_shown(GENERATION).into_iter().flatten().count(), 0);

    let at = ms(now, 1);
    let job = only_job(
        commit_as(&mut p, RUN, LIVE, 1, now_step("next"), at).expect("authored successor"),
    );
    prepared(&mut p, job, at);
    assert_eq!(p.next_wake(at), Some(now));
    p.advance(GENERATION, at)
        .expect("authored successor installs without waiting for audio");
    assert_eq!(bound(&p, LIVE), 2);
    assert_eq!(
        p.conductor_view(at),
        Some(level),
        "the installed speech still owns audio"
    );
    assert!(p.sample_at(at).expect("authored sample").0.visible);
    paint_shown(&mut p, at);
    let mut acks = p.take_shown(GENERATION).into_iter().flatten();
    assert_eq!(
        acks.next().map(|ack| (ack.scene, ack.index)),
        Some((LIVE, 1))
    );
    assert_eq!(acks.next(), None);
    assert_eq!(
        p.conductor_view(at),
        Some(level),
        "the successor's paint isn't speech paint"
    );
    report(
        &mut p,
        level.binding,
        RouteStamp::Live(7),
        PictureState::Ended(Picture::Silent),
        ms(now, 2),
    );
    assert_eq!(
        p.conductor_view(ms(now, 2)),
        None,
        "only the keyed end releases speech"
    );
}

#[test]
fn a_ready_speech_step_becomes_bindable_only_after_its_authored_head_installs() {
    let (mut p, now) = fixture([assignment(LIVE, 0), assignment(LIVE, 1)]);
    begin(&mut p, LIVE, now);
    let job =
        only_job(commit_as(&mut p, RUN, LIVE, 0, now_step("anchor"), now).expect("authored head"));
    prepared(&mut p, job, now);
    let job = only_job(commit_as(&mut p, RUN, LIVE, 1, drawing(), now).expect("speech read-ahead"));
    prepared(&mut p, job, now);
    assert_eq!(bound(&p, LIVE), 0);
    assert_eq!(p.conductor_view(now), None);

    p.advance(GENERATION, now)
        .expect("installs the authored head only");
    assert_eq!(bound(&p, LIVE), 1);
    assert_eq!(
        p.conductor_view(now),
        Some(ViewLevel {
            binding: Binding {
                assignment: assignment(LIVE, 1),
                nonce: 1
            },
            speech: true,
            view: View::Eligible,
        })
    );
}
