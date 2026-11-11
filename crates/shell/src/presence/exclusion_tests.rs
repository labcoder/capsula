use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use kumochi_protocol::message::GestureId;

use super::*;

const EXCLUDED: u32 = 17;
const PRIOR: [u32; 7] = [0, 1, EXCLUDED, 4, 8, 16, 32];

struct Surface {
    number: u32,
    destroyed: Rc<Cell<bool>>,
    affinity: u32,
}

pub(crate) struct Prior {
    pub(super) window: Option<AnyWindowHandle>,
    number: u32,
    destroyed: Rc<Cell<bool>>,
    affinity: u32,
}

pub(crate) struct Fake {
    surfaces: [Option<Surface>; 7],
    get_failure: Option<usize>,
    set_failure: Option<usize>,
    restore_failure: u8,
    controls: Option<async_channel::Receiver<kumochi_protocol::message::ControlToBrain>>,
}

impl Fake {
    pub(super) fn assert_excluded(&self, slot: Slot) {
        assert_eq!(
            self.surfaces[slot as usize].as_ref().unwrap().affinity,
            EXCLUDED,
            "actual placement must observe exclusion already applied"
        );
    }

    fn new() -> Self {
        Self {
            surfaces: std::array::from_fn(|index| {
                Some(Surface {
                    number: u32::try_from(index).unwrap(),
                    destroyed: Rc::new(Cell::new(false)),
                    affinity: PRIOR[index],
                })
            }),
            get_failure: None,
            set_failure: None,
            restore_failure: 0,
            controls: None,
        }
    }

    fn affinities(&self) -> [u32; 7] {
        std::array::from_fn(|index| self.surfaces[index].as_ref().unwrap().affinity)
    }

    fn replace(&mut self, slot: Slot, affinity: u32) {
        let old = self.surfaces[slot as usize].as_ref().unwrap();
        old.destroyed.set(true);
        self.surfaces[slot as usize] = Some(Surface {
            number: old.number,
            destroyed: Rc::new(Cell::new(false)),
            affinity,
        });
    }
}

impl Surfaces for Fake {
    type Receipt = Prior;

    fn snapshot(&mut self, slot: Slot) -> Result<Option<Prior>, Error> {
        let index = slot as usize;
        if self.get_failure == Some(index) {
            return Err(Error::Surface);
        }
        Ok(self.surfaces[index].as_ref().map(|surface| Prior {
            window: None,
            number: surface.number,
            destroyed: Rc::clone(&surface.destroyed),
            affinity: surface.affinity,
        }))
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "asserts the fixture restores the same retained surface"
    )]
    fn exclude(&mut self, slot: Slot, prior: &Prior) -> Result<(), Error> {
        let surface = self.surfaces[slot as usize].as_mut().unwrap();
        assert!(Rc::ptr_eq(&surface.destroyed, &prior.destroyed));
        surface.affinity = EXCLUDED;
        if self.set_failure == Some(slot as usize) {
            return Err(Error::Surface);
        }
        Ok(())
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "asserts the fixture restores the same retained surface"
    )]
    fn restore(&mut self, slot: Slot, prior: &Prior) -> Result<(), Error> {
        if prior.destroyed.get() {
            return Ok(());
        }
        if self.restore_failure & (1 << slot as usize) != 0 {
            return Err(Error::Surface);
        }
        let surface = self.surfaces[slot as usize].as_mut().unwrap();
        assert_eq!(surface.number, prior.number);
        if !Rc::ptr_eq(&surface.destroyed, &prior.destroyed) {
            return Err(Error::Surface);
        }
        surface.affinity = prior.affinity;
        Ok(())
    }

    fn gone(prior: &Prior) -> bool {
        prior.destroyed.get()
    }

    fn same(&self, slot: Slot, prior: &Prior) -> bool {
        self.surfaces[slot as usize]
            .as_ref()
            .is_some_and(|surface| Rc::ptr_eq(&surface.destroyed, &prior.destroyed))
    }
}

fn acquire(owner: &mut Exclusion<Prior>, fake: &mut Fake, at: Instant) -> Key {
    owner
        .acquire(9, CaptureFor::Gesture(GestureId(7)), at, fake)
        .unwrap()
}

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
        this.exclusion.lease.as_ref().unwrap().key
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
        let consumed = presenter
            .consume_ready(crate::presenter::tests::LIVE, 0, now, 0, 0)
            .expect("activate the prepared drawing");
        assert!(consumed.effects.expect("current drawing").is_empty());
        this.presenter = presenter;
        let overlay = super::super::overlay::Overlay::detached(
            this.presenter.frames(),
            cx.weak_entity(),
            Rc::clone(&this.paint_scratch),
            Rc::clone(&this.paint_notice),
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
        [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32]
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
            owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().key),
            key
        );
    }
    receive(
        &owner,
        LinkEvent::CaptureDone {
            generation: 9,
            capture_for: key.input,
        },
        cx,
    );
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease.is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
    receive(
        &owner,
        LinkEvent::CaptureFailed {
            generation: 9,
            capture_for: key.input,
            reason: CaptureFailure::Unavailable,
        },
        cx,
    );
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease.is_none()));
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
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease.is_none()));
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
        assert!(this.exclusion.lease.is_none());
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
            capture_for: first.input,
        },
        cx,
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(1));
    let next = acquire_presence(&owner, first.input, cx);
    assert_ne!(first.generation, next.generation);
    cx.run_until_parked();
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(
        owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().key),
        next
    );
    assert_eq!(
        fixture.borrow().affinities(),
        [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32]
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(1) - Duration::from_nanos(1));
    cx.run_until_parked();
    assert_eq!(
        owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().key),
        next
    );
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_nanos(1));
    cx.run_until_parked();
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease.is_none()));
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
    assert!(!owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().revoked));
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_nanos(1));
    cx.run_until_parked();
    assert!(owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().revoked));
    assert_eq!(
        fixture.borrow().affinities(),
        [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32]
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
        assert_eq!(this.exclusion.lease.as_ref().unwrap().key, key);
        assert!(this.exclusion.lease.as_ref().unwrap().revoked);
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
                capture_for: key.input,
            },
            cx,
        );
        assert_eq!(
            fixture.borrow().affinities(),
            [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32]
        );
    }
    receive(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert!(owner.read_with(cx, |this, _| this.session.is_none()
        && this.exclusion.lease.is_none()));
    assert_eq!(fixture.borrow().affinities(), PRIOR);
}

#[gpui::test]
fn actual_lazy_overlay_protects_before_show_and_keeps_failed_surface_hidden(
    cx: &mut gpui_kit::TestAppContext,
) {
    for fail in [false, true] {
        let (owner, fixture) = presence_fixture(cx);
        acquire_presence(&owner, CaptureFor::Turn(7), cx);
        hidden_drawing(&owner, cx);
        if fail {
            fixture.borrow_mut().set_failure = Some(Slot::Drawing as usize);
            fixture.borrow_mut().restore_failure = 1 << Slot::Drawing as usize;
        }
        owner.update(cx, |this, cx| {
            let (sample, effects) = this.presenter.sample(0);
            assert!(effects.is_empty());
            assert!(sample.visible);
            let result = this.sync_overlay(sample, cx);
            if fail {
                assert!(matches!(
                    result,
                    Err(super::super::OpenError::Exclusion(Error::Surface))
                ));
                assert!(!this.presenter.frames().borrow().shown);
                assert!(this.overlay.window().is_some());
                assert!(this.exclusion.lease.as_ref().unwrap().revoked);
                assert!(
                    this.exclusion.lease.as_ref().unwrap().receipts[Slot::Drawing as usize]
                        .is_some()
                );
            } else {
                result.expect("actual overlay show");
                assert!(this.presenter.frames().borrow().shown);
                assert!(!this.exclusion.lease.as_ref().unwrap().revoked);
            }
            assert_eq!(
                fixture.borrow().surfaces[Slot::Drawing as usize]
                    .as_ref()
                    .unwrap()
                    .affinity,
                EXCLUDED
            );
        });
    }
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
        assert!(owner.read_with(cx, |this, _| this.exclusion.lease.as_ref().unwrap().revoked));
        owner.update(cx, |this, cx| {
            let (sample, effects) = this.presenter.sample_at(std::time::Instant::now());
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
            let (sample, effects) = this.presenter.sample(0);
            assert!(!sample.visible);
            assert!(effects.is_empty());
            assert!(this.exclusion.lease.as_ref().unwrap().revoked);
            assert_eq!(
                fixture.borrow().affinities(),
                [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32]
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
                assert!(this.exclusion.lease.as_ref().unwrap().revoked);
                assert_eq!(
                    fixture.borrow().affinities(),
                    [0, EXCLUDED, EXCLUDED, 4, 8, 16, 32]
                );
            } else {
                assert!(this.exclusion.lease.is_none());
                assert_eq!(fixture.borrow().affinities(), PRIOR);
            }
        });
        cx.run_until_parked();
    }
}

#[test]
fn all_seven_surfaces_restore_their_exact_independent_prior_affinity() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let key = acquire(&mut owner, &mut fake, Instant::now());
    assert_eq!(
        key,
        Key {
            session: 9,
            input: CaptureFor::Gesture(GestureId(7)),
            generation: LeaseGeneration(1)
        }
    );
    assert_eq!(fake.affinities(), [EXCLUDED; 7]);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    assert!(owner.lease.is_none());
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
}

#[test]
fn every_getter_and_partial_setter_failure_refuses_and_rolls_back() {
    for index in 0..7 {
        for setter in [false, true] {
            let mut owner = Exclusion::default();
            let mut fake = Fake::new();
            if setter {
                fake.set_failure = Some(index);
            } else {
                fake.get_failure = Some(index);
            }
            assert_eq!(
                owner.acquire(9, CaptureFor::Turn(1), Instant::now(), &mut fake),
                Err(Error::Surface)
            );
            assert_eq!(fake.affinities(), PRIOR);
            assert!(owner.lease.is_none());
        }
    }
}

#[test]
fn rollback_and_final_restore_failures_retain_debt_and_block_regrant() {
    for rollback in [false, true] {
        let mut owner = Exclusion::default();
        let mut fake = Fake::new();
        fake.restore_failure = (1 << Slot::Proxy as usize) | (1 << Slot::Drawing as usize);
        if rollback {
            fake.set_failure = Some(Slot::Drawing as usize);
            assert_eq!(
                owner.acquire(9, CaptureFor::Turn(1), Instant::now(), &mut fake),
                Err(Error::Surface)
            );
        } else {
            let _key = acquire(&mut owner, &mut fake, Instant::now());
            assert_eq!(owner.cleanup(&mut fake), Err(Error::Surface));
        }
        assert_eq!(
            fake.affinities(),
            [0, EXCLUDED, EXCLUDED, 4, 8, 16, EXCLUDED]
        );
        let lease = owner.lease.as_ref().unwrap();
        assert!(lease.revoked);
        assert_eq!(
            lease
                .receipts
                .iter()
                .filter(|prior| prior.is_some())
                .count(),
            2
        );
        fake.set_failure = None;
        assert_eq!(
            owner.acquire(10, CaptureFor::Turn(2), Instant::now(), &mut fake),
            Err(Error::Busy)
        );
        assert_eq!(owner.attach(Slot::Text, &mut fake), Err(Error::Busy));
        fake.restore_failure = 0;
        assert_eq!(owner.cleanup(&mut fake), Ok(()));
        assert_eq!(fake.affinities(), PRIOR);
        assert!(owner.lease.is_none());
    }
}

#[test]
fn lazy_surface_attachment_preserves_priors_and_retains_failed_cleanup() {
    for slot in [Slot::Text, Slot::Threads, Slot::Settings, Slot::Drawing] {
        for fail in [false, true] {
            let mut owner = Exclusion::default();
            let mut fake = Fake::new();
            let hidden = fake.surfaces[slot as usize].take();
            let _key = acquire(&mut owner, &mut fake, Instant::now());
            fake.surfaces[slot as usize] = hidden;
            if fail {
                fake.set_failure = Some(slot as usize);
                fake.restore_failure = 1 << slot as usize;
                assert_eq!(owner.attach(slot, &mut fake), Err(Error::Surface));
                assert!(owner.lease.as_ref().unwrap().revoked);
                assert!(owner.lease.as_ref().unwrap().receipts[slot as usize].is_some());
                fake.restore_failure = 0;
            } else {
                assert_eq!(owner.attach(slot, &mut fake), Ok(()));
                let surface = fake.surfaces[slot as usize].as_mut().unwrap();
                assert_eq!(surface.affinity, EXCLUDED);
                assert_eq!(owner.attach(slot, &mut fake), Ok(()));
                assert_eq!(
                    fake.surfaces[slot as usize].as_ref().unwrap().affinity,
                    EXCLUDED
                );
            }
            assert_eq!(owner.cleanup(&mut fake), Ok(()));
            assert_eq!(fake.affinities(), PRIOR);
        }
    }
}

#[test]
fn destruction_and_recycled_numbers_never_transfer_a_prior_affinity() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let _key = acquire(&mut owner, &mut fake, Instant::now());
    fake.replace(Slot::Drawing, 91);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 91]);
    let _next = acquire(&mut owner, &mut fake, Instant::now());
    fake.replace(Slot::Drawing, 92);
    assert_eq!(owner.attach(Slot::Drawing, &mut fake), Ok(()));
    assert_eq!(fake.affinities(), [EXCLUDED; 7]);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 92]);
}

#[test]
fn an_unconfirmed_lost_instance_keeps_debt_without_touching_a_recycled_number() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let _key = acquire(&mut owner, &mut fake, Instant::now());
    let old = fake.surfaces[Slot::Drawing as usize].take().unwrap();
    fake.surfaces[Slot::Drawing as usize] = Some(Surface {
        number: old.number,
        destroyed: Rc::new(Cell::new(false)),
        affinity: 93,
    });
    assert_eq!(owner.attach(Slot::Drawing, &mut fake), Err(Error::Surface));
    assert_eq!(owner.cleanup(&mut fake), Err(Error::Surface));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 93]);
    assert!(owner.lease.as_ref().unwrap().revoked);
    assert_eq!(
        owner.acquire(10, CaptureFor::Turn(9), Instant::now(), &mut fake),
        Err(Error::Busy)
    );
    old.destroyed.set(true);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 93]);
    assert!(owner.lease.is_none());
}

#[test]
fn full_keys_and_the_exact_deadline_fence_local_cleanup() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let now = Instant::now();
    let key = acquire(&mut owner, &mut fake, now);
    for stale in [
        Key { session: 10, ..key },
        Key {
            input: CaptureFor::Turn(7),
            ..key
        },
        Key {
            input: CaptureFor::Gesture(GestureId(8)),
            ..key
        },
        Key {
            generation: LeaseGeneration(2),
            ..key
        },
    ] {
        assert_eq!(owner.expire(stale, now + LIFETIME, &mut fake), Ok(()));
        assert_eq!(fake.affinities(), [EXCLUDED; 7]);
    }
    assert_eq!(
        owner.expire(
            key,
            (now + LIFETIME)
                .checked_sub(Duration::from_nanos(1))
                .expect("fixture clock supports the instant before expiry"),
            &mut fake
        ),
        Ok(())
    );
    assert_eq!(fake.affinities(), [EXCLUDED; 7]);
    assert_eq!(owner.expire(key, now + LIFETIME, &mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    let next = owner
        .acquire(10, CaptureFor::Turn(8), now + LIFETIME, &mut fake)
        .unwrap();
    assert_eq!(owner.expire(key, now + LIFETIME * 2, &mut fake), Ok(()));
    assert_eq!(owner.lease.as_ref().unwrap().key, next);
    assert_eq!(fake.affinities(), [EXCLUDED; 7]);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
}

#[test]
fn counter_exhaustion_refuses_instead_of_wrapping_and_off_is_a_noop() {
    let mut owner = Exclusion {
        generation: u32::MAX - 1,
        lease: None,
    };
    let mut fake = Fake::new();
    fake.get_failure = Some(0);
    assert_eq!(owner.attach(Slot::Dock, &mut fake), Ok(()));
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    fake.get_failure = None;
    let key = acquire(&mut owner, &mut fake, Instant::now());
    assert_eq!(key.generation, LeaseGeneration(u32::MAX));
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(
        owner.acquire(9, CaptureFor::Turn(1), Instant::now(), &mut fake),
        Err(Error::Exhausted)
    );
    assert_eq!(owner.generation, u32::MAX);
    assert_eq!(fake.affinities(), PRIOR);
}

#[gpui::test]
fn actual_presence_keeps_hold_and_listening_off_even_after_ready_and_deferred_effects(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::input::{Edge, Effect, Event, Gesture, Key as InputKey};
    use crate::link::{Batch, LinkEvent, Session};
    use crate::platform::EventTime;
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
                    let now = Instant::now();
                    let mut gesture = Gesture::legacy_hold_fixture();
                    gesture.handle(
                        Event::Key {
                            key: InputKey::LeftControl,
                            edge: Edge::Down,
                        },
                        now,
                    );
                    gesture.handle(
                        Event::Key {
                            key: InputKey::LeftAlt,
                            edge: Edge::Down,
                        },
                        now,
                    );
                    this.gesture.set(gesture);
                    assert!(this.gesture.get().listening());
                    this.apply_effects(
                        [
                            Some(Effect::Listening),
                            Some(Effect::HoldStarted),
                            Some(Effect::HoldEnded),
                        ],
                        EventTime { instant: now },
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
                    assert!(this.exclusion.lease.is_none());
                    assert_eq!(this.exclusion.generation, 0);
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
                    assert!(this.exclusion.lease.is_none());
                    assert!(!this.dock.read(cx).listening);
                    assert!(controls.is_empty());
                    assert!(requests.is_empty());
                });
            })
            .expect("dock update");
    });
}
