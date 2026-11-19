use std::{cell::RefCell, rc::Rc, time::Duration};

use kumochi_protocol::message::GestureId;

use super::machine::Key;
use super::machine::fixture::{EXCLUDED, Fake, PRIOR};
use super::*;

fn presence_fixture(
    cx: &mut gpui_kit::TestAppContext,
) -> (gpui_kit::Entity<Presence>, Rc<RefCell<Fake>>) {
    let owner = super::super::test_fixture::presence(cx);
    let fixture = Rc::new(RefCell::new(Fake::new()));
    owner.update(cx, |this, _| {
        this.exclusion_fixture = Some(Rc::clone(&fixture));
        this.session = Some(crate::link::Session::detached(
            9,
            kumochi_protocol::message::Incarnation(1),
        ));
        let mut link = crate::link::LinkHandle::detached();
        fixture.borrow_mut().controls = Some(link.record_controls());
        this.link = link;
    });
    (owner, fixture)
}

fn acquire_presence(
    owner: &gpui_kit::Entity<Presence>,
    input: CaptureFor,
    cx: &mut gpui_kit::TestAppContext,
) -> Key {
    owner.update(cx, |this, cx| {
        let session = this.session.clone().unwrap();
        this.acquire_exclusion(&session, input, cx)
            .expect("actual Presence acquisition through the injected affinity adapter");
        this.exclusion.lease().unwrap().key()
    })
}

fn receive(
    owner: &gpui_kit::Entity<Presence>,
    event: crate::link::LinkEvent,
    cx: &mut gpui_kit::TestAppContext,
) {
    owner
        .update(cx, |this, cx| {
            this.link_events(crate::link::Batch::Plain(vec![event]), cx)
        })
        .expect("actual Presence receiver");
}

fn hidden_drawing(owner: &gpui_kit::Entity<Presence>, cx: &mut gpui_kit::TestAppContext) {
    owner.update(cx, |this, cx| {
        let generation = this.session.as_ref().expect("fixture session").generation();
        let stage = super::super::paint_cache::Stager::new(&this.paint_scratch);
        let (mut presenter, now, job) =
            crate::presenter::tests::preparing_for(generation, Box::new(stage));
        presenter
            .completed(job.run(&mut crate::presenter::tests::Shaper), now)
            .expect("existing prepared-frame fixture");
        let consumed = crate::presenter::tests::consume_ready(
            &mut presenter,
            crate::presenter::tests::LIVE,
            0,
            now,
            0,
            0,
        )
        .expect("activate the prepared drawing");
        assert!(consumed.effects.expect("current drawing").is_empty());
        this.presenter = presenter;
        let overlay = super::super::overlay::Overlay::detached(
            this.presenter.frames(),
            cx.weak_entity(),
            Rc::clone(&this.paint_scratch),
            Rc::clone(&this.paint_notice),
            Rc::clone(&this.ink),
            cx,
        );
        this.overlay = super::super::overlay_visibility::Visibility::Hidden(overlay);
        assert!(!this.presenter.frames().borrow().shown);
    });
}

#[gpui::test]
fn actual_presence_terminals_match_nonempty_owner_session_input_and_duplicates(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::link::LinkEvent;
    use kumochi_protocol::message::CaptureFailure;
    let (owner, fixture) = presence_fixture(cx);
    let key = acquire_presence(&owner, CaptureFor::Turn(7), cx);
    assert_eq!(
        fixture.borrow().affinities(),
        [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32, 64]
    );
    for (generation, capture_for) in [
        (8, CaptureFor::Turn(7)),
        (9, CaptureFor::Gesture(GestureId(7))),
        (9, CaptureFor::Turn(8)),
    ] {
        receive(
            &owner,
            LinkEvent::CaptureDone {
                generation,
                capture_for,
            },
            cx,
        );
        assert_eq!(
            owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().key()),
            key
        );
    }
    receive(
        &owner,
        LinkEvent::CaptureDone {
            generation: 9,
            capture_for: key.input(),
        },
        cx,
    );
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease().is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
    receive(
        &owner,
        LinkEvent::CaptureFailed {
            generation: 9,
            capture_for: key.input(),
            reason: CaptureFailure::Unavailable,
        },
        cx,
    );
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease().is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
    acquire_presence(&owner, CaptureFor::Turn(8), cx);
    receive(
        &owner,
        LinkEvent::CaptureFailed {
            generation: 9,
            capture_for: CaptureFor::Turn(8),
            reason: CaptureFailure::Cancelled,
        },
        cx,
    );
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease().is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
    assert!(fixture.borrow().controls.as_ref().unwrap().is_empty());
}

#[gpui::test]
fn detached_native_affinity_is_not_the_positive_presence_adapter(
    cx: &mut gpui_kit::TestAppContext,
) {
    let owner = super::super::test_fixture::presence(cx);
    owner.update(cx, |this, cx| {
        assert!(matches!(
            this.native.affinity(),
            Err(platform::PresenceError::NotWin32)
        ));
        let session = crate::link::Session::detached(9, kumochi_protocol::message::Incarnation(1));
        this.session = Some(session.clone());
        assert_eq!(
            this.acquire_exclusion(&session, CaptureFor::Turn(7), cx),
            Err(Error::Surface)
        );
        assert!(this.exclusion.lease().is_none());
        assert!(this.exclusion_timer.is_none());
    });
}

#[gpui::test]
fn actual_timer_uses_original_deadline_and_cannot_retire_a_successor(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::link::LinkEvent;
    let (owner, fixture) = presence_fixture(cx);
    let first = acquire_presence(&owner, CaptureFor::Turn(7), cx);
    let old_timer = owner.update(cx, |this, _| this.exclusion_timer.take().unwrap());
    receive(
        &owner,
        LinkEvent::CaptureDone {
            generation: 9,
            capture_for: first.input(),
        },
        cx,
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(1));
    let next = acquire_presence(&owner, first.input(), cx);
    assert_ne!(first.generation(), next.generation());
    cx.run_until_parked();
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(
        owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().key()),
        next
    );
    assert_eq!(
        fixture.borrow().affinities(),
        [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32, 64]
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(1) - Duration::from_nanos(1));
    cx.run_until_parked();
    assert_eq!(
        owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().key()),
        next
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_nanos(1));
    cx.run_until_parked();
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease().is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
    drop(old_timer);
}

#[gpui::test]
fn actual_timer_retains_restoration_failure_at_its_original_deadline(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (owner, fixture) = presence_fixture(cx);
    acquire_presence(&owner, CaptureFor::Turn(7), cx);
    fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(1));
    cx.run_until_parked();
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(2) - Duration::from_nanos(1));
    cx.run_until_parked();
    assert!(!owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().revoked()));
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_nanos(1));
    cx.run_until_parked();
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().revoked()));
    assert_eq!(
        fixture.borrow().affinities(),
        [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32, 64]
    );
}

#[gpui::test]
fn actual_replacement_and_state_keep_old_debt_without_matching_new_sessions(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::link::{LinkEvent, LinkState, Session};
    use kumochi_protocol::message::Incarnation;
    let (owner, fixture) = presence_fixture(cx);
    let key = acquire_presence(&owner, CaptureFor::Turn(7), cx);
    fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
    receive(
        &owner,
        LinkEvent::Connected(Session::detached(10, Incarnation(1))),
        cx,
    );
    owner.update(cx, |this, cx| {
        assert_eq!(this.session.as_ref().unwrap().generation(), 10);
        assert_eq!(this.exclusion.lease().unwrap().key(), key);
        assert!(this.exclusion.lease().unwrap().revoked());
        let session = this.session.clone().unwrap();
        assert_eq!(
            this.acquire_exclusion(&session, CaptureFor::Turn(8), cx),
            Err(Error::Busy)
        );
    });
    fixture.borrow_mut().restore_failure = 0;
    for generation in [9, 10] {
        receive(
            &owner,
            LinkEvent::CaptureDone {
                generation,
                capture_for: key.input(),
            },
            cx,
        );
        assert_eq!(
            fixture.borrow().affinities(),
            [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32, 64]
        );
    }
    receive(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert!(owner.read_with(cx, |this, _| this.session.is_none()
        && this.exclusion.lease().is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
}

#[gpui::test]
fn actual_shown_overlay_can_clear_or_disconnect_while_restoration_debt_blocks_show(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::link::{LinkEvent, LinkState};
    for disconnect in [false, true] {
        let (owner, fixture) = presence_fixture(cx);
        hidden_drawing(&owner, cx);
        owner.update(cx, super::super::Presence::draw_changed);
        assert!(owner.read_with(cx, |this, _| this.presenter.frames().borrow().shown));
        acquire_presence(&owner, CaptureFor::Turn(7), cx);
        fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
        owner.update(cx, super::super::Presence::revoke_exclusion);
        assert!(owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().revoked()));
        owner.update(cx, |this, cx| {
            let (sample, effects) = this
                .presenter
                .sample_at(std::time::Instant::now())
                .expect("a sample");
            assert!(sample.visible);
            assert!(effects.is_empty());
            assert!(matches!(
                this.sync_overlay(sample, cx),
                Err(super::super::OpenError::Exclusion(Error::Busy))
            ));
        });
        if disconnect {
            receive(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        } else {
            owner.update(cx, |this, cx| {
                let effects = this.presenter.clear(cx.background_executor().now());
                this.scene_effects(effects, cx);
            });
        }
        owner.update(cx, |this, cx| {
            assert!(!this.presenter.frames().borrow().shown);
            let (sample, effects) = crate::presenter::tests::sample(&mut this.presenter, 0);
            assert!(!sample.visible);
            assert!(effects.is_empty());
            assert!(this.exclusion.lease().unwrap().revoked());
            assert_eq!(
                fixture.borrow().affinities(),
                [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32, 64]
            );
            if let Some(session) = this.session.clone() {
                assert_eq!(
                    this.acquire_exclusion(&session, CaptureFor::Turn(8), cx),
                    Err(Error::Busy)
                );
            }
            assert!(!this.presenter.frames().borrow().shown);
        });
    }
}

#[gpui::test]
fn actual_quit_cleans_a_nonempty_owner_and_does_not_forgive_failed_restoration(
    cx: &mut gpui_kit::TestAppContext,
) {
    for fail in [false, true] {
        let (owner, fixture) = presence_fixture(cx);
        cx.update(|cx| {
            cx.set_global(super::super::PresenceApp {
                _owner: owner.clone(),
            });
        });
        acquire_presence(&owner, CaptureFor::Turn(7), cx);
        if fail {
            fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
        }
        owner.update(cx, |this, cx| {
            this.quit(cx);
            if fail {
                assert!(this.exclusion.lease().unwrap().revoked());
                assert_eq!(
                    fixture.borrow().affinities(),
                    [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32, 64]
                );
            } else {
                assert!(this.exclusion.lease().is_none());
                assert_eq!(fixture.borrow().affinities(), PRIOR);
            }
        });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn actual_presence_keeps_hold_and_listening_off_even_after_ready_and_deferred_effects(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::input::Effect;
    use crate::link::{Batch, LinkEvent, Session};
    use kumochi_protocol::message::{Incarnation, PartState};

    let owner = super::super::test_fixture::presence(cx);
    let (window, controls, requests) = owner.update(cx, |this, _| {
        let (mut link, requests) = crate::link::LinkHandle::recording();
        let controls = link.record_controls();
        this.link = link;
        this.session = Some(Session::detached(9, Incarnation(1)));
        (this.dock_window, controls, requests)
    });
    cx.update(|cx| {
        window
            .update(cx, |_, window, cx| {
                owner.update(cx, |this, cx| {
                    this.apply_effects(
                        [
                            Some(Effect::Listening),
                            Some(Effect::HoldStarted),
                            Some(Effect::HoldEnded),
                        ],
                        window,
                        cx,
                    );
                    this.link_events(
                        Batch::Plain(vec![LinkEvent::Voice {
                            generation: 9,
                            speech_input: PartState::Ready,
                        }]),
                        cx,
                    )
                    .expect("ready status");
                    this.refresh_dock(cx);
                    assert!(!this.dock.read(cx).listening);
                    assert!(this.exclusion.lease().is_none());
                    assert_eq!(this.exclusion.generation(), 0);
                    assert!(this.exclusion_timer.is_none());
                    assert!(controls.is_empty());
                    assert!(requests.is_empty());
                    for generation in [8, 9] {
                        this.link_events(
                            Batch::Plain(vec![
                                LinkEvent::CaptureDone {
                                    generation,
                                    capture_for: CaptureFor::Turn(7),
                                },
                                LinkEvent::CaptureFailed {
                                    generation,
                                    capture_for: CaptureFor::Gesture(GestureId(7)),
                                    reason: kumochi_protocol::message::CaptureFailure::Cancelled,
                                },
                            ]),
                            cx,
                        )
                        .expect("terminal events");
                    }
                    this.link_events(
                        Batch::Plain(vec![LinkEvent::Connected(Session::detached(
                            10,
                            Incarnation(1),
                        ))]),
                        cx,
                    )
                    .expect("direct replacement");
                    assert_eq!(this.session.as_ref().unwrap().generation(), 10);
                    assert!(this.exclusion.lease().is_none());
                    assert!(!this.dock.read(cx).listening);
                    assert!(controls.is_empty());
                    assert!(requests.is_empty());
                });
            })
            .expect("dock update");
    });
}

fn acquire_stamped(
    owner: &gpui_kit::Entity<Presence>,
    read: impl FnOnce() -> i64,
    cx: &mut gpui_kit::TestAppContext,
) -> Result<(kumochi_protocol::message::LeaseGeneration, u64), Error> {
    owner.update(cx, |this, cx| {
        let session = this.session.clone().unwrap();
        this.acquire_stamped(&session, CaptureFor::Gesture(GestureId(7)), read, cx)
    })
}

#[gpui::test]
fn a_stamped_lease_reads_the_counter_after_every_surface_is_excluded(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (owner, fixture) = presence_fixture(cx);
    let seen = Rc::clone(&fixture);
    let read = move || {
        assert_eq!(
            seen.borrow().affinities(),
            [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32, 64],
            "the counter is read after the last surface is excluded"
        );
        4_242
    };
    let stamped = acquire_stamped(&owner, read, cx);
    let key = owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().key());
    assert_eq!(key.input(), CaptureFor::Gesture(GestureId(7)));
    assert_eq!(stamped, Ok((key.generation(), 4_242)));
}

#[gpui::test]
fn a_counter_with_no_positive_reading_gives_the_lease_back_without_a_lapse(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::LeaseGeneration;
    let (owner, fixture) = presence_fixture(cx);
    for reading in [0, -1, i64::MIN] {
        assert_eq!(
            acquire_stamped(&owner, move || reading, cx),
            Err(Error::Exhausted),
            "a reading of {reading}"
        );
        assert!(owner.read_with(cx, |this, _| this.exclusion.lease().is_none()));
        assert_eq!(fixture.borrow().affinities(), PRIOR);
    }
    assert!(
        fixture
            .borrow()
            .controls
            .as_ref()
            .unwrap()
            .try_recv()
            .is_err(),
        "the brain never saw those leases, so none lapsed"
    );
    // Each refusal used its generation up; the least positive reading stamps the fourth.
    assert_eq!(
        acquire_stamped(&owner, || 1, cx),
        Ok((LeaseGeneration(4), 1))
    );
}

#[gpui::test]
fn a_refused_lease_reads_no_counter(cx: &mut gpui_kit::TestAppContext) {
    let (owner, _fixture) = presence_fixture(cx);
    let first = acquire_presence(&owner, CaptureFor::Turn(7), cx);
    assert_eq!(
        acquire_stamped(
            &owner,
            || -> i64 { panic!("a refused lease has nothing to stamp") },
            cx
        ),
        Err(Error::Busy)
    );
    assert_eq!(
        owner.read_with(cx, |this, _| this.exclusion.lease().unwrap().key()),
        first,
        "the lease in force is untouched"
    );
}
