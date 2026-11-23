use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use kumochi_protocol::message::{CaptureFor, GestureId, LeaseGeneration};

use super::fixture::{EXCLUDED, Fake, PRIOR, Surface, acquire};
use super::{Error, Exclusion, Key, LIFETIME, Slot};

#[test]
fn only_the_current_unexpired_lease_confirms_or_restores_its_receipts() {
    let at = Instant::now();
    let mut owner = Exclusion::default();
    let mut surfaces = Fake::new();
    let first = acquire(&mut owner, &mut surfaces, at);
    assert!(owner.confirms(first, at, &surfaces));
    assert!(!owner.confirms(first, at + LIFETIME, &surfaces));
    assert_eq!(owner.restore_matching(first, &mut surfaces), Ok(()));
    let next = acquire(&mut owner, &mut surfaces, at);
    assert!(!owner.confirms(first, at, &surfaces));
    assert_eq!(owner.restore_matching(first, &mut surfaces), Ok(()));
    assert_eq!(surfaces.affinities(), [EXCLUDED; 8]);
    assert!(owner.confirms(next, at, &surfaces));
    assert_eq!(owner.restore_matching(next, &mut surfaces), Ok(()));
    assert_eq!(surfaces.affinities(), PRIOR);
}

#[test]
fn all_eight_surfaces_restore_their_exact_independent_prior_affinity() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let key = acquire(&mut owner, &mut fake, Instant::now());
    assert_eq!(
        (key.session, key.input),
        (9, CaptureFor::Gesture(GestureId(7)))
    );
    assert!(owner.live(key.generation()), "the granted lease is live");
    assert_eq!(fake.affinities(), [EXCLUDED; 8]);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    assert!(!owner.live(key.generation()), "cleanup ends the grant");
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    let next = acquire(&mut owner, &mut fake, Instant::now());
    assert!(owner.live(next.generation()), "the slot is reusable");
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
}

#[test]
fn every_getter_and_partial_setter_failure_refuses_and_rolls_back() {
    for index in 0..8 {
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
            fake.get_failure = None;
            fake.set_failure = None;
            let next = acquire(&mut owner, &mut fake, Instant::now());
            assert!(
                owner.live(next.generation()),
                "no debt after a full rollback"
            );
            assert_eq!(owner.cleanup(&mut fake), Ok(()));
            assert_eq!(fake.affinities(), PRIOR);
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
            let key = acquire(&mut owner, &mut fake, Instant::now());
            assert_eq!(owner.cleanup(&mut fake), Err(Error::Surface));
            assert!(
                !owner.live(key.generation()),
                "failed cleanup revokes capture"
            );
        }
        assert_eq!(
            fake.affinities(),
            [0, EXCLUDED, EXCLUDED, 4, 8, 16, EXCLUDED, 64]
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
        let next = acquire(&mut owner, &mut fake, Instant::now());
        assert!(
            owner.live(next.generation()),
            "cleared debt permits a grant"
        );
        assert_eq!(owner.cleanup(&mut fake), Ok(()));
    }
}

#[test]
fn lazy_surface_attachment_preserves_priors_and_retains_failed_cleanup() {
    for slot in [
        Slot::Text,
        Slot::Threads,
        Slot::Settings,
        Slot::Drawing,
        Slot::Onboarding,
    ] {
        for fail in [false, true] {
            let mut owner = Exclusion::default();
            let mut fake = Fake::new();
            let hidden = fake.surfaces[slot as usize].take();
            let key = acquire(&mut owner, &mut fake, Instant::now());
            fake.surfaces[slot as usize] = hidden;
            if fail {
                fake.set_failure = Some(slot as usize);
                fake.restore_failure = 1 << slot as usize;
                assert_eq!(owner.attach(slot, &mut fake), Err(Error::Surface));
                assert!(
                    !owner.live(key.generation()),
                    "failed attachment revokes capture"
                );
                assert_eq!(
                    owner.acquire(10, CaptureFor::Turn(2), Instant::now(), &mut fake),
                    Err(Error::Busy)
                );
                assert_eq!(owner.cleanup(&mut fake), Err(Error::Surface));
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
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 91, 64]);
    let _next = acquire(&mut owner, &mut fake, Instant::now());
    fake.replace(Slot::Drawing, 92);
    assert_eq!(owner.attach(Slot::Drawing, &mut fake), Ok(()));
    assert_eq!(fake.affinities(), [EXCLUDED; 8]);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 92, 64]);
}

#[test]
fn an_unconfirmed_lost_instance_keeps_debt_without_touching_a_recycled_number() {
    let mut owner = Exclusion::default();
    let mut fake = Fake::new();
    let key = acquire(&mut owner, &mut fake, Instant::now());
    let old = fake.surfaces[Slot::Drawing as usize].take().unwrap();
    fake.surfaces[Slot::Drawing as usize] = Some(Surface {
        number: old.number,
        destroyed: Rc::new(Cell::new(false)),
        affinity: 93,
    });
    assert_eq!(owner.attach(Slot::Drawing, &mut fake), Err(Error::Surface));
    assert_eq!(owner.cleanup(&mut fake), Err(Error::Surface));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 93, 64]);
    assert!(
        !owner.live(key.generation()),
        "an uncertain surface revokes capture"
    );
    assert_eq!(
        owner.acquire(10, CaptureFor::Turn(9), Instant::now(), &mut fake),
        Err(Error::Busy)
    );
    old.destroyed.set(true);
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 93, 64]);
    let next = acquire(&mut owner, &mut fake, Instant::now());
    assert!(
        owner.live(next.generation()),
        "destruction proof releases the slot"
    );
    assert_eq!(owner.cleanup(&mut fake), Ok(()));
    assert_eq!(fake.affinities(), [0, 1, EXCLUDED, 4, 8, 16, 93, 64]);
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
            generation: LeaseGeneration(key.generation.0.wrapping_add(1).max(1)),
            ..key
        },
    ] {
        assert_eq!(owner.expire(stale, now + LIFETIME, &mut fake), Ok(()));
        assert_eq!(fake.affinities(), [EXCLUDED; 8]);
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
    assert_eq!(fake.affinities(), [EXCLUDED; 8]);
    assert_eq!(owner.expire(key, now + LIFETIME, &mut fake), Ok(()));
    assert_eq!(fake.affinities(), PRIOR);
    let next = owner
        .acquire(10, CaptureFor::Turn(8), now + LIFETIME, &mut fake)
        .unwrap();
    assert_eq!(owner.expire(key, now + LIFETIME * 2, &mut fake), Ok(()));
    assert!(
        !owner.live(key.generation()),
        "the expired grant stays dead"
    );
    assert!(
        owner.live(next.generation()),
        "the stale expiry cannot revoke the successor"
    );
    assert_eq!(fake.affinities(), [EXCLUDED; 8]);
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
    assert_eq!(
        owner.acquire(10, CaptureFor::Turn(2), Instant::now(), &mut fake),
        Err(Error::Exhausted)
    );
    assert_eq!(fake.affinities(), PRIOR);
}
