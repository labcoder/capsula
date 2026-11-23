//! The fake surfaces the exclusion's tests drive: eight numbered surfaces, each with an affinity.

use std::{cell::Cell, rc::Rc, time::Instant};

use kumochi_protocol::message::{CaptureFor, GestureId};

use super::{Error, Exclusion, Key, Slot, Surfaces};

pub const EXCLUDED: u32 = 17;
pub const PRIOR: [u32; 8] = [0, 1, EXCLUDED, 4, 8, 16, 32, 64];

pub struct Surface {
    pub(crate) number: u32,
    pub(crate) destroyed: Rc<Cell<bool>>,
    pub affinity: u32,
}

pub struct Prior {
    number: u32,
    destroyed: Rc<Cell<bool>>,
    affinity: u32,
}

pub struct Fake {
    pub surfaces: [Option<Surface>; 8],
    pub(crate) get_failure: Option<usize>,
    pub set_failure: Option<usize>,
    pub restore_failure: u8,
    pub controls: Option<async_channel::Receiver<kumochi_protocol::message::ControlToBrain>>,
    pub(crate) proof_before_restore: bool,
}

impl Fake {
    pub fn assert_excluded(&self, slot: Slot) {
        assert_eq!(
            self.surfaces[slot as usize].as_ref().unwrap().affinity,
            EXCLUDED,
            "actual placement must observe exclusion already applied"
        );
    }

    pub fn new() -> Self {
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
            proof_before_restore: false,
        }
    }

    pub fn affinities(&self) -> [u32; 8] {
        std::array::from_fn(|index| self.surfaces[index].as_ref().unwrap().affinity)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only this crate's tests call it")
    )]
    pub(crate) fn replace(&mut self, slot: Slot, affinity: u32) {
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
        if self.proof_before_restore {
            assert_eq!(
                self.controls.as_ref().unwrap().len(),
                1,
                "proof precedes restoration"
            );
        }
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

    fn confirms_exclusion(&self, slot: Slot, prior: Option<&Prior>) -> bool {
        match (&self.surfaces[slot as usize], prior) {
            (None, None) => true,
            (Some(surface), Some(prior)) => {
                self.same(slot, prior) && !Self::gone(prior) && surface.affinity == EXCLUDED
            }
            _ => false,
        }
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only this crate's tests call it")
)]
pub(crate) fn acquire(owner: &mut Exclusion<Prior>, fake: &mut Fake, at: Instant) -> Key {
    owner
        .acquire(9, CaptureFor::Gesture(GestureId(7)), at, fake)
        .unwrap()
}
