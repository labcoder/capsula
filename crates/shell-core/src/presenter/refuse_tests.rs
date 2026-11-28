use kumochi_protocol::message::{DataToShell, RunId, Step};
use kumochi_scene::{StopCut, text::Bounded};

use super::*;
use crate::link::scene_naming;
use crate::presenter::fixture::{RUN, commit_frame, header, rect, snapshot, zero_cut};

const A: SceneId = SceneId(1);
const B: SceneId = SceneId(2);
const C: SceneId = SceneId(3);
const D: SceneId = SceneId(4);

/// An effect as the brain would see it; anything else this shell must never send.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Credit(u32),
    Failed(SceneId),
}

fn sent(effects: Vec<Effect>) -> Vec<Sent> {
    effects
        .into_iter()
        .map(|effect| match effect {
            Effect::Credit(bytes) => Sent::Credit(bytes),
            Effect::Control(ControlToBrain::SceneFailed { scene }) => Sent::Failed(scene),
            other => panic!("a shell that draws nothing sent {other:?}"),
        })
        .collect()
}

fn open(generation: u64) -> Refuser {
    let mut refuser = Refuser::default();
    refuser.connected(generation);
    refuser
}

fn draw(refuser: &mut Refuser, scene: SceneId) -> Vec<Sent> {
    sent(refuser.data(1, Naming::Draws(scene), 10))
}

fn retire(refuser: &mut Refuser, scenes: &[SceneId]) -> Vec<Sent> {
    sent(refuser.data(1, Naming::Retires(scenes), 10))
}

#[test]
fn every_frame_of_the_open_connection_is_credited_once() {
    let mut refuser = open(1);
    let victims = [A, B, C, D];
    assert_eq!(
        sent(refuser.data(1, Naming::Draws(A), 101)),
        [Sent::Credit(101), Sent::Failed(A)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Draws(A), 102)),
        [Sent::Credit(102)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Retained(B), 103)),
        [Sent::Credit(103)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Retires(&[C]), 104)),
        [Sent::Credit(104)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Retires(&victims), 105)),
        [Sent::Credit(105)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Other, 106)),
        [Sent::Credit(106)]
    );
}

#[test]
fn a_stale_generation_or_no_connection_gets_nothing() {
    let mut idle = Refuser::default();
    assert_eq!(sent(idle.data(0, Naming::Draws(A), 10)), []);
    let mut refuser = open(2);
    assert_eq!(sent(refuser.data(1, Naming::Draws(A), 10)), []);
    assert_eq!(sent(refuser.data(1, Naming::Other, 10)), []);
    assert_eq!(sent(refuser.data(3, Naming::Retained(A), 10)), []);
    assert_eq!(
        sent(refuser.data(2, Naming::Draws(A), 10)),
        [Sent::Credit(10), Sent::Failed(A)],
        "the stale frames refused nothing for the open connection"
    );
}

#[test]
fn a_closed_connection_stays_closed_for_its_frames() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    refuser.disconnected();
    assert_eq!(draw(&mut refuser, B), []);
    assert_eq!(sent(refuser.data(1, Naming::Other, 10)), []);
    assert_eq!(sent(refuser.data(1, Naming::Retires(&[A]), 10)), []);
}

#[test]
fn a_scene_drawn_on_twice_is_refused_once() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, B), [Sent::Credit(10), Sent::Failed(B)]);
}

#[test]
fn a_retired_scene_drawn_on_again_is_refused_again() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    assert_eq!(retire(&mut refuser, &[A]), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
}

#[test]
fn a_new_connection_refuses_its_scenes_again() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    refuser.connected(2);
    assert_eq!(
        sent(refuser.data(2, Naming::Draws(A), 10)),
        [Sent::Credit(10), Sent::Failed(A)]
    );
}

#[test]
fn a_retained_chunk_is_credited_and_a_later_draw_on_it_is_refused() {
    let mut refuser = open(1);
    assert_eq!(
        sent(refuser.data(1, Naming::Retained(A), 30)),
        [Sent::Credit(30)]
    );
    assert_eq!(
        sent(refuser.data(1, Naming::Retained(A), 31)),
        [Sent::Credit(31)]
    );
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
}

#[test]
fn snapshot_refused_call_removal_then_a_delayed_descriptor() {
    let mut refuser = open(1);
    assert_eq!(
        sent(refuser.data(1, Naming::Retained(A), 30)),
        [Sent::Credit(30)]
    );
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10)]);
    assert_eq!(retire(&mut refuser, &[A]), [Sent::Credit(10)]);
    // The descriptor list on the control pipe is ignored; its delayed chunk only earns credit.
    assert_eq!(
        sent(refuser.data(1, Naming::Retained(A), 30)),
        [Sent::Credit(30)]
    );
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
}

#[test]
fn repeated_replacements_forget_every_victim() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
    assert_eq!(draw(&mut refuser, B), [Sent::Credit(10), Sent::Failed(B)]);
    assert_eq!(retire(&mut refuser, &[A, B]), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, C), [Sent::Credit(10), Sent::Failed(C)]);
    assert_eq!(retire(&mut refuser, &[C]), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, D), [Sent::Credit(10), Sent::Failed(D)]);
    assert_eq!(draw(&mut refuser, D), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, B), [Sent::Credit(10), Sent::Failed(B)]);
    assert_eq!(draw(&mut refuser, A), [Sent::Credit(10), Sent::Failed(A)]);
}

fn refused_through(count: u64) -> Refuser {
    let mut refuser = open(1);
    for id in 1..=count {
        assert_eq!(
            draw(&mut refuser, SceneId(id)),
            [Sent::Credit(10), Sent::Failed(SceneId(id))]
        );
    }
    refuser
}

#[test]
fn the_exact_bound_keeps_every_refusal() {
    let kept = u64::try_from(CLEARED_KEPT).expect("the bound fits an id");
    assert_eq!(kept, 64);
    let mut refuser = refused_through(kept);
    assert_eq!(draw(&mut refuser, SceneId(1)), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, SceneId(kept)), [Sent::Credit(10)]);
}

#[test]
fn one_past_the_bound_forgets_the_oldest() {
    let kept = u64::try_from(CLEARED_KEPT).expect("the bound fits an id");
    let mut refuser = refused_through(kept + 1);
    assert_eq!(draw(&mut refuser, SceneId(2)), [Sent::Credit(10)]);
    assert_eq!(draw(&mut refuser, SceneId(kept + 1)), [Sent::Credit(10)]);
    assert_eq!(
        draw(&mut refuser, SceneId(1)),
        [Sent::Credit(10), Sent::Failed(SceneId(1))]
    );
}

#[test]
fn the_link_classifies_every_scene_kind() {
    let cut = |activated| StopCut {
        activated,
        revealed: Bounded::default(),
    };
    let rows = [
        (
            DataToShell::SceneBegin {
                scene: A,
                header: header(),
            },
            Some(Naming::Draws(A)),
        ),
        (commit_frame(B, 0, rect("r")), Some(Naming::Draws(B))),
        (snapshot(C, rect("r")), Some(Naming::Retained(C))),
        (
            DataToShell::SceneCutApplied {
                scene: D,
                revision: 2,
                cut: zero_cut(),
            },
            Some(Naming::Retires(&[D])),
        ),
        (
            DataToShell::SceneCutApplied {
                scene: D,
                revision: 2,
                cut: cut(1),
            },
            Some(Naming::Other),
        ),
        (
            DataToShell::SceneEnd {
                run: RUN,
                scene: A,
                complete: true,
            },
            Some(Naming::Other),
        ),
        (
            DataToShell::ScenesRemoved { scenes: vec![A, B] },
            Some(Naming::Retires(&[A, B])),
        ),
        (
            DataToShell::ScenesReplaced {
                scenes: vec![B, C, D],
                successor: A,
            },
            Some(Naming::Retires(&[B, C, D])),
        ),
        (
            DataToShell::StepCommit {
                run: RunId(1),
                scene: A,
                index: 0,
                step: Step {
                    label: "s".to_owned(),
                },
            },
            None,
        ),
        (
            DataToShell::SceneSnapshot {
                scene: A,
                revision: 1,
                chunk: vec![Step {
                    label: "s".to_owned(),
                }],
                last: true,
            },
            None,
        ),
        (DataToShell::RunEnd { run: RUN }, None),
    ];
    for (frame, naming) in &rows {
        assert_eq!(scene_naming(frame), *naming);
    }
}
