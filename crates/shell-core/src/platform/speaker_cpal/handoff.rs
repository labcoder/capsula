use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::BLOCKS;

struct Slot<T> {
    published: AtomicBool,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: pair creates exactly one producer and one consumer. Each slot's release/acquire
// publication hands the value to the consumer; clearing publication hands it back.
unsafe impl<T: Send> Sync for Slot<T> {}

pub(super) struct Producer<T> {
    slots: Arc<[Slot<T>; BLOCKS]>,
    next: usize,
}

pub(super) struct Consumer<T> {
    slots: Arc<[Slot<T>; BLOCKS]>,
    next: usize,
}

pub(super) fn pair<T: Send>() -> (Producer<T>, Consumer<T>) {
    let slots = Arc::new(std::array::from_fn(|_| Slot {
        published: AtomicBool::new(false),
        value: UnsafeCell::new(None),
    }));
    (
        Producer {
            slots: Arc::clone(&slots),
            next: 0,
        },
        Consumer { slots, next: 0 },
    )
}

impl<T> Producer<T> {
    pub(super) fn push(&mut self, value: T) -> Result<(), T> {
        self.publish(value, || {})
    }

    #[cfg(test)]
    pub(super) fn push_before_publish(
        &mut self,
        value: T,
        before_publish: impl FnOnce(),
    ) -> Result<(), T> {
        self.publish(value, before_publish)
    }

    fn publish(&mut self, value: T, before_publish: impl FnOnce()) -> Result<(), T> {
        let slot = &self.slots[self.next];
        if slot.published.load(Ordering::Acquire) {
            return Err(value);
        }
        // SAFETY: this producer exclusively owns the unpublished slot. In normal use the
        // previous consumer took the value before releasing the slot.
        unsafe { *slot.value.get() = Some(value) };
        before_publish();
        slot.published.store(true, Ordering::Release);
        self.next = (self.next + 1) % BLOCKS;
        Ok(())
    }
}

impl<T> Consumer<T> {
    pub(super) fn pop(&mut self) -> Option<T> {
        let slot = &self.slots[self.next];
        if !slot.published.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: only this consumer can take a published value; the producer cannot write
        // again until this consumer releases the slot below.
        let value = unsafe { &mut *slot.value.get() }.take();
        slot.published.store(false, Ordering::Release);
        self.next = (self.next + 1) % BLOCKS;
        value
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.published.load(Ordering::Acquire))
            .count()
    }
}
