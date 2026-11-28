use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{DataToShell, RunId, Step};
use kumochi_scene::{StopCut, text::Bounded};

use super::*;
use crate::link::{LinkEvent, credit_fixture::counted_legacy_fixture, scene_naming};
use crate::presenter::fixture::{RUN, commit_frame, header, rect, snapshot, zero_cut};

const A: SceneId = SceneId(1);
const B: SceneId = SceneId(2);
const C: SceneId = SceneId(3);
const D: SceneId = SceneId(4);

fn credited(credit: u32) -> Disposal {
    Disposal {
        credit,
        refuse: None,
    }
}

fn refused(credit: u32, scene: SceneId) -> Disposal {
    Disposal {
        credit,
        refuse: Some(scene),
    }
}

fn open(generation: u64) -> Refuser {
    let mut refuser = Refuser::default();
    refuser.connected(generation);
    refuser
}

fn draw(refuser: &mut Refuser, scene: SceneId) -> Option<Disposal> {
    refuser.data(1, Naming::Draws(scene), 10)
}

fn retire(refuser: &mut Refuser, scenes: &[SceneId]) -> Option<Disposal> {
    refuser.data(1, Naming::Retires(scenes), 10)
}

#[test]
fn every_frame_of_the_open_connection_is_credited_once() {
    let mut refuser = open(1);
    let victims = [A, B, C, D];
    assert_eq!(
        refuser.data(1, Naming::Draws(A), 101),
        Some(refused(101, A))
    );
    assert_eq!(refuser.data(1, Naming::Draws(A), 102), Some(credited(102)));
    assert_eq!(
        refuser.data(1, Naming::Retained(B), 103),
        Some(credited(103))
    );
    assert_eq!(
        refuser.data(1, Naming::Retires(&[C]), 104),
        Some(credited(104))
    );
    assert_eq!(
        refuser.data(1, Naming::Retires(&victims), 105),
        Some(credited(105))
    );
    assert_eq!(refuser.data(1, Naming::Other, 106), Some(credited(106)));
}

#[test]
fn a_stale_generation_or_no_connection_gets_nothing() {
    let mut idle = Refuser::default();
    assert_eq!(idle.data(0, Naming::Draws(A), 10), None);
    let mut refuser = open(2);
    assert_eq!(refuser.data(1, Naming::Draws(A), 10), None);
    assert_eq!(refuser.data(1, Naming::Other, 10), None);
    assert_eq!(refuser.data(3, Naming::Retained(A), 10), None);
    assert_eq!(
        refuser.data(2, Naming::Draws(A), 10),
        Some(refused(10, A)),
        "the stale frames refused nothing for the open connection"
    );
}

#[test]
fn a_closed_connection_stays_closed_for_its_frames() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    refuser.disconnected();
    assert_eq!(draw(&mut refuser, B), None);
    assert_eq!(refuser.data(1, Naming::Other, 10), None);
    assert_eq!(retire(&mut refuser, &[A]), None);
}

#[test]
fn a_scene_drawn_on_twice_is_refused_once() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    assert_eq!(draw(&mut refuser, A), Some(credited(10)));
    assert_eq!(draw(&mut refuser, B), Some(refused(10, B)));
}

#[test]
fn a_retired_scene_drawn_on_again_is_refused_again() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    assert_eq!(retire(&mut refuser, &[A]), Some(credited(10)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
}

#[test]
fn retiring_one_scene_keeps_the_others_refused() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    assert_eq!(draw(&mut refuser, B), Some(refused(10, B)));
    assert_eq!(retire(&mut refuser, &[A]), Some(credited(10)));
    assert_eq!(draw(&mut refuser, B), Some(credited(10)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
}

#[test]
fn a_new_connection_refuses_its_scenes_again() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    refuser.connected(2);
    assert_eq!(refuser.data(2, Naming::Draws(A), 10), Some(refused(10, A)));
}

#[test]
fn a_retained_chunk_is_credited_and_a_later_draw_on_it_is_refused() {
    let mut refuser = open(1);
    assert_eq!(refuser.data(1, Naming::Retained(A), 30), Some(credited(30)));
    assert_eq!(refuser.data(1, Naming::Retained(A), 31), Some(credited(31)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
}

#[test]
fn a_retained_chunk_after_its_scene_retired_gets_credit_only() {
    let mut refuser = open(1);
    assert_eq!(refuser.data(1, Naming::Retained(A), 30), Some(credited(30)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    assert_eq!(draw(&mut refuser, A), Some(credited(10)));
    assert_eq!(retire(&mut refuser, &[A]), Some(credited(10)));
    assert_eq!(refuser.data(1, Naming::Retained(A), 30), Some(credited(30)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
}

#[test]
fn repeated_replacements_forget_every_victim() {
    let mut refuser = open(1);
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
    assert_eq!(draw(&mut refuser, B), Some(refused(10, B)));
    assert_eq!(retire(&mut refuser, &[A, B]), Some(credited(10)));
    assert_eq!(draw(&mut refuser, C), Some(refused(10, C)));
    assert_eq!(retire(&mut refuser, &[C]), Some(credited(10)));
    assert_eq!(draw(&mut refuser, D), Some(refused(10, D)));
    assert_eq!(draw(&mut refuser, D), Some(credited(10)));
    assert_eq!(draw(&mut refuser, B), Some(refused(10, B)));
    assert_eq!(draw(&mut refuser, A), Some(refused(10, A)));
}

fn refused_through(count: u64) -> Refuser {
    let mut refuser = open(1);
    for id in 1..=count {
        assert_eq!(
            draw(&mut refuser, SceneId(id)),
            Some(refused(10, SceneId(id)))
        );
    }
    refuser
}

#[test]
fn the_exact_bound_keeps_every_refusal() {
    let kept = u64::try_from(CLEARED_KEPT).expect("the bound fits an id");
    assert_eq!(kept, 64);
    let mut refuser = refused_through(kept);
    assert_eq!(draw(&mut refuser, SceneId(1)), Some(credited(10)));
    assert_eq!(draw(&mut refuser, SceneId(kept)), Some(credited(10)));
}

#[test]
fn one_past_the_bound_forgets_the_oldest() {
    let kept = u64::try_from(CLEARED_KEPT).expect("the bound fits an id");
    let mut refuser = refused_through(kept + 1);
    assert_eq!(draw(&mut refuser, SceneId(2)), Some(credited(10)));
    assert_eq!(draw(&mut refuser, SceneId(kept + 1)), Some(credited(10)));
    assert_eq!(
        draw(&mut refuser, SceneId(1)),
        Some(refused(10, SceneId(1)))
    );
}

/// One frame of each scene kind, a zero and a nonzero cut, and frames that aren't scene traffic.
fn table() -> Vec<(DataToShell, Option<Naming<'static>>)> {
    let legacy = || Step {
        label: "s".to_owned(),
    };
    vec![
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
                cut: StopCut {
                    activated: 1,
                    revealed: Bounded::default(),
                },
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
                step: legacy(),
            },
            None,
        ),
        (
            DataToShell::SceneSnapshot {
                scene: A,
                revision: 1,
                chunk: vec![legacy()],
                last: true,
            },
            None,
        ),
        (DataToShell::RunEnd { run: RUN }, None),
    ]
}

#[test]
fn the_link_classifies_every_scene_kind() {
    for (frame, naming) in &table() {
        assert_eq!(scene_naming(frame), *naming);
    }
}

#[test]
fn the_reader_tags_scene_data_exactly_when_the_classifier_names_it() {
    let table = table();
    let mut wire = Vec::new();
    for (frame, _) in &table {
        frame::write(&mut wire, frame, Pipe::Data).expect("frame written");
    }
    let events = counted_legacy_fixture(wire.as_slice());
    assert_eq!(events.len(), table.len());
    for ((frame, _), event) in table.iter().zip(&events) {
        let scene_data = match event {
            LinkEvent::SceneData { .. } => true,
            LinkEvent::Data { .. } => false,
            other => panic!("a data frame became {other:?}"),
        };
        assert_eq!(
            scene_data,
            scene_naming(frame).is_some(),
            "{:?}",
            std::mem::discriminant(frame)
        );
    }
}
