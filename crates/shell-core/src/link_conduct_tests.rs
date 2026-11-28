//! Several promised steps of one conducted run, through an attended pair and its scripted peer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToShell, SceneDescriptorV1, SceneId, SegmentId,
    SegmentStart, SpeechDecision, StepRef,
};
use kumochi_scene::text::Bounded;

use super::LinkEvent;
use super::conduct_fixture::{Pair, allow, key};
use super::fixture::PATIENCE;
use super::narrated::BEFORE_ALLOW;
use super::output::hold_input_installs;
use crate::presenter::fixture;
use crate::speaker::conductor::{Assignment, View};
use crate::speaker::queued::hold_fake_opens;

impl Pair {
    fn steps(&self) -> Vec<(Assignment, bool, bool)> {
        self.session.io.narrated.as_ref().unwrap().steps()
    }
}

/// Both commits reach the presenter before it advances, and scene 1 holds the later step B, so
/// the presenter meets B before A.
fn two_scenes(label: &str) -> (Pair, Assignment, Assignment) {
    let mut pair = Pair::new(label);
    let (a, b) = (
        pair.assignment(SceneId(2), 0),
        pair.assignment(SceneId(1), 1),
    );
    pair.begin(b.step.scene);
    pair.begin(a.step.scene);
    let commits = [Pair::commit(a), Pair::commit(b)];
    for (commit, step) in commits.iter().zip([a, b]) {
        pair.send(commit);
        pair.announce(step);
    }
    let mut effects = Vec::new();
    for commit in &commits {
        let LinkEvent::SceneData { frame, bytes, .. } = pair.delivered(commit) else {
            unreachable!()
        };
        effects.extend(
            pair.presenter
                .data(pair.session.generation(), frame, bytes, Instant::now())
                .unwrap(),
        );
    }
    pair.settle(effects);
    pair.wake();
    (pair, a, b)
}

#[test]
fn two_scenes_bind_only_the_lowest_announced_step() {
    let (mut pair, a, b) = two_scenes("conduct-two-scenes");
    assert_eq!(pair.bound().map(|(bound, _)| bound), Some(a));
    allow(&mut pair, a);
    assert_eq!(pair.bound(), Some((a, View::Eligible)));
    assert_ne!(pair.bound().map(|(bound, _)| bound), Some(b));
    assert!(pair.decisions().is_empty(), "B waits for A's receipt");
    assert!(pair.session.open());
}

#[test]
fn a_removed_unnarrated_step_is_discarded_in_turn_without_fencing_the_head() {
    let (mut pair, a, b) = two_scenes("conduct-removed-second");
    allow(&mut pair, a);
    pair.scene(&DataToShell::ScenesRemoved {
        scenes: vec![b.step.scene],
    });
    pair.update();
    assert!(
        pair.decisions().is_empty(),
        "B's Discard waits for A's receipt"
    );
    pair.taken(a);
    assert_eq!(pair.decided(), [Pair::decision(b, SpeechDecision::Discard)]);
    assert!(pair.decisions().is_empty(), "exactly one Discard");
    assert_eq!(pair.bound(), Some((a, View::Eligible)));
    assert_eq!(
        pair.steps(),
        [(a, true, false), (b, false, false)],
        "B was never narrated and A was never fenced"
    );
    assert!(pair.session.open());
}

#[test]
fn esc_with_withheld_credit_drains_both_commits_and_releases_without_a_frame() {
    let mut pair = Pair::new("conduct-esc-withheld");
    let (a, b) = (
        pair.assignment(SceneId(1), 0),
        pair.assignment(SceneId(2), 1),
    );
    let first = Pair::commit(a);
    pair.send(&first);
    pair.delivered(&first);
    pair.esc();
    let second = Pair::commit(b);
    pair.send(&second);
    pair.delivered(&second);
    pair.run_end();
    assert!(
        pair.hold_ready(),
        "the exact RunEnd released the stopped run with no UI pass"
    );
    assert!(pair.session.open());
}

#[test]
fn a_run_wide_tts_failure_settles_the_unbound_step_and_shows_it_once() {
    let (mut pair, a, b) = two_scenes("conduct-tts-failure");
    allow(&mut pair, a);
    pair.taken(a);
    pair.pcm(0, 0);
    pair.pcm(1, 0);
    pair.run_end();
    let deadline = Instant::now() + PATIENCE;
    let mut shown = Vec::new();
    while !shown.contains(&key(b)) {
        assert!(Instant::now() < deadline, "B shows on its authored timing");
        shown.extend(pair.paint());
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..3 {
        shown.extend(pair.paint());
    }
    assert_eq!(shown.iter().filter(|&&seen| seen == key(b)).count(), 1);
    assert!(pair.decisions().is_empty(), "B's decision was never sent");
    assert!(pair.hold_ready(), "a fresh speaking Hold is admitted");
    assert!(pair.session.open());
}

#[test]
fn two_interrupted_runs_keep_one_receipt_until_its_late_exact_ack() {
    let mut pair = Pair::new("conduct-late-receipt");
    let a = pair.assignment(SceneId(1), 0);
    pair.begin(a.step.scene);
    pair.promise(a);
    pair.wake();
    allow(&mut pair, a);
    pair.esc();
    pair.run_end();
    pair.next_run(2);
    let c = pair.assignment(SceneId(2), 0);
    pair.begin(c.step.scene);
    pair.promise(c);
    pair.wake();
    let deadline = Instant::now() + PATIENCE;
    while pair.bound() != Some((c, View::Eligible)) || !pair.applied(c) {
        assert!(
            Instant::now() < deadline,
            "C binds and the worker applies it"
        );
        pair.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        pair.decisions().is_empty(),
        "A's receipt is outstanding, so C's Allow waits"
    );
    pair.esc();
    pair.run_end();
    pair.next_run(3);
    let d = pair.assignment(SceneId(3), 0);
    pair.begin(d.step.scene);
    pair.promise(d);
    pair.taken(a);
    assert!(pair.decisions().is_empty());
    assert!(pair.session.open(), "the late exact receipt is accepted");
    pair.taken(a);
    pair.closed();
}

#[test]
fn only_allowed_speech_takes_sound_while_empty_finals_plain_and_draining_pass() {
    for case in ["unallowed", "empty", "plain", "draining"] {
        let mut pair = Pair::new(&format!("conduct-pcm-{case}"));
        let a = pair.assignment(SceneId(1), 0);
        pair.begin(a.step.scene);
        match case {
            "plain" => pair.send(&DataToShell::SegmentQueued {
                run: pair.run,
                segment: a.segment,
                step: None,
                start: SegmentStart::Plain,
                output_generation: pair.output,
            }),
            _ => pair.promise(a),
        }
        if case == "draining" {
            pair.esc();
        }
        pair.pcm(0, usize::from(case != "empty") * 480);
        if case == "unallowed" {
            pair.closed();
            continue;
        }
        let marker = DataToShell::SceneBegin {
            scene: SceneId(9),
            header: fixture::header(),
        };
        pair.send(&marker);
        pair.delivered(&marker);
        assert!(pair.decisions().is_empty());
        assert!(pair.session.open(), "{case} PCM keeps the pair");
    }
}

/// A line whose first sample, 0.5, is the only one at that level.
fn marked_line(samples: usize) -> Vec<i16> {
    let mut line = vec![9; samples];
    line[0] = 16_384;
    line
}

#[test]
fn a_slow_first_open_keeps_a_plain_reply_s_first_sample() {
    let pair = Pair::new("conduct-slow-open-plain");
    let opening = hold_fake_opens();
    pair.send(&DataToShell::SegmentQueued {
        run: pair.run,
        segment: SegmentId(0),
        step: None,
        start: SegmentStart::Plain,
        output_generation: pair.output,
    });
    pair.sound(0, &marked_line(4_800));
    std::thread::sleep(Duration::from_millis(300));
    drop(opening);
    let (_device, first) = pair.first_sound();
    assert_eq!(
        first.to_bits(),
        0.5f32.to_bits(),
        "the reply starts at its first sample"
    );
    assert!(pair.session.open());
}

/// Chunk zero of a recovered snapshot begins its scene for the bridge, so A's later promise
/// binds and paints; B's scene never began, so its promise is retired and only discarded.
#[test]
fn a_promise_after_snapshot_chunk_zero_paints_and_an_unknown_scene_s_is_retired() {
    let mut pair = Pair::new("conduct-recovered-chunk-zero");
    let a = Assignment {
        step: StepRef {
            scene: SceneId(1),
            index: 1,
        },
        ..pair.assignment(SceneId(1), 0)
    };
    let b = pair.assignment(SceneId(2), 1);
    let scenes =
        Bounded::new(vec![SceneDescriptorV1::new(a.step.scene, 1, 1, 1).unwrap()]).unwrap();
    let generation = pair.session.generation();
    pair.presenter = fixture::presenter();
    pair.presenter.connected(generation);
    let effects = pair
        .presenter
        .described(generation, &scenes, Instant::now())
        .unwrap();
    pair.settle(effects);
    pair.session.attach_narration(&mut pair.presenter);
    pair.scene(&fixture::snapshot(a.step.scene, fixture::rect("kept")));
    pair.promise(a);
    let commit = Pair::commit(b);
    pair.send(&commit);
    pair.delivered(&commit);
    pair.announce(b);
    pair.wake();
    allow(&mut pair, a);
    pair.taken(a);
    assert_eq!(
        pair.decided(),
        [Pair::decision(b, SpeechDecision::Discard)],
        "B's unknown scene retired its promise"
    );
    pair.sound(0, &marked_line(4_800));
    let (mut device, _) = pair.first_sound();
    let deadline = Instant::now() + PATIENCE;
    let mut shown = Vec::new();
    while !shown.contains(&key(a)) {
        assert!(Instant::now() < deadline, "A shows on its keyed onset");
        device.render(&mut [0.0; 240]);
        shown.extend(pair.paint());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(shown, [key(a)], "only A paints");
    assert!(pair.session.open());
}

/// The marker, the presenter consuming it, a late promise, its empty final and `RunEnd`: the
/// late step never paints and leaves no fallback in the table.
#[test]
fn a_late_promise_after_a_consumed_removal_never_paints_or_reserves_a_fallback() {
    let mut pair = Pair::new("conduct-late-promise");
    let late = pair.assignment(SceneId(1), 0);
    pair.begin(late.step.scene);
    pair.scene(&DataToShell::ScenesRemoved {
        scenes: vec![late.step.scene],
    });
    let commit = Pair::commit(late);
    pair.send(&commit);
    pair.delivered(&commit);
    pair.announce(late);
    pair.sound(late.segment.0, &[]);
    pair.run_end();
    for _ in 0..3 {
        assert!(pair.paint().is_empty(), "the late step never paints");
    }
    assert_eq!(
        pair.session.io.narrated.as_ref().unwrap().reserved(),
        0,
        "the late step reserves no fallback"
    );
    assert!(pair.session.open());
}

#[test]
fn an_eligible_step_keeps_its_first_sample_through_a_slow_first_open() {
    let mut pair = Pair::new("conduct-slow-open-step");
    let a = pair.assignment(SceneId(1), 0);
    pair.begin(a.step.scene);
    pair.promise(a);
    pair.wake();
    allow(&mut pair, a);
    pair.taken(a);
    let opening = hold_fake_opens();
    pair.sound(0, &marked_line(4_800));
    let held = Instant::now() + Duration::from_millis(300);
    while Instant::now() < held {
        assert!(
            pair.paint().is_empty(),
            "A waits for its keyed onset, not silent timing, while the device opens"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(opening);
    let (mut device, first) = pair.first_sound();
    assert_eq!(
        first.to_bits(),
        0.5f32.to_bits(),
        "the step's line starts at its first sample"
    );
    let deadline = Instant::now() + PATIENCE;
    let mut shown = Vec::new();
    while !shown.contains(&key(a)) {
        assert!(Instant::now() < deadline, "A shows on its keyed onset");
        device.render(&mut [0.0; 240]);
        shown.extend(pair.paint());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(shown, [key(a)], "A's first paint is accepted once");
    assert!(pair.session.open());
}

#[test]
fn a_stop_or_run_end_that_lands_before_the_gate_drops_the_allow_and_keeps_the_pair() {
    for case in ["revoked", "ended"] {
        let mut pair = Pair::new(&format!("conduct-allow-race-{case}"));
        let a = pair.assignment(SceneId(1), 0);
        pair.begin(a.step.scene);
        pair.promise(a);
        let raced = Arc::new(AtomicBool::new(false));
        let (shared, session, ran) = (
            Arc::clone(&pair.link.handle.shared),
            pair.session.generation(),
            Arc::clone(&raced),
        );
        BEFORE_ALLOW.set(Some(Box::new(move || {
            let landed = if case == "revoked" {
                shared.output.revoke_queued(session, a.run)
            } else {
                shared.output.end_queued_run(session, a.run)
            };
            assert!(landed, "{case} lands between the pass and the gate");
            ran.store(true, Ordering::Release);
        })));
        pair.wake();
        let deadline = Instant::now() + PATIENCE;
        while !raced.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "A's pass reaches the gate");
            pair.update();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            pair.decisions().is_empty(),
            "the raced Allow publishes no decision"
        );
        pair.run_end();
        let deadline = Instant::now() + PATIENCE;
        while !pair
            .link
            .handle
            .shared
            .output
            .queued_hold_ready(pair.session.generation())
        {
            assert!(
                Instant::now() < deadline,
                "the exact RunEnd admits a fresh speaking Hold"
            );
            pair.update();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(pair.session.open(), "{case} before the gate keeps the pair");
    }
}

#[test]
fn a_brain_stopped_run_drains_its_retained_commits_behind_a_typed_successor() {
    let mut pair = Pair::new("conduct-revoked-predecessor");
    let (a, retained) = (
        pair.assignment(SceneId(1), 0),
        pair.assignment(SceneId(2), 1),
    );
    pair.begin(a.step.scene);
    pair.begin(retained.step.scene);
    pair.promise(a);
    pair.reply(&ControlToShell::SpeechRevoked { run: a.run });
    loop {
        match pair.link.next_control() {
            ControlToBrain::SpeechStopped { run, .. } if run == a.run => break,
            ControlToBrain::SpeechDecision(..) => panic!("a revoked run sends no decision"),
            _ => {}
        }
    }
    pair.next_run(2);
    let commit = Pair::commit(retained);
    pair.send(&commit);
    pair.delivered(&commit);
    let end = DataToShell::RunEnd { run: a.run };
    pair.send(&end);
    pair.delivered(&end);
    assert!(pair.decisions().is_empty());
    assert!(
        pair.session.open(),
        "the retained commit drains behind the successor without a disconnect"
    );
}

#[test]
fn the_input_install_wakes_a_narration_its_only_earlier_pass_deferred() {
    let mut pair = Pair::new("conduct-install-wake");
    pair.esc();
    pair.run_end();
    let installs = hold_input_installs();
    pair.name_run(2);
    let a = pair.assignment(SceneId(1), 0);
    pair.begin(a.step.scene);
    pair.promise(a);
    pair.wake();
    assert_eq!(
        pair.steps(),
        [(a, false, false)],
        "the only pass before the install finds no input and narrates nothing"
    );
    drop(installs);
    pair.named();
    allow(&mut pair, a);
    assert!(pair.session.open());
}
