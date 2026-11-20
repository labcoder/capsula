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
    presenter.start_conduction(now, move |key| {
        Ok(offered
            .into_iter()
            .find(|a| a.run == key.run && a.step.scene == key.scene && a.step.index == key.index))
    });
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
    p.start_conduction(now, move |_| offered.get());
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
