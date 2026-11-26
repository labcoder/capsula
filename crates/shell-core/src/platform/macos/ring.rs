//! The capture ring: preallocated cycle slots that the Core Audio IO block fills in place and the
//! capture worker drains, with no allocation, lock or wait on the IO thread.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use zeroize::{Zeroize, Zeroizing};

use crate::platform::microphone::MicError;

const OPEN: u8 = 0;
const IN_CYCLE: u8 = 1;
const CLOSED: u8 = 2;

const FREE: u8 = 0;
const WRITING: u8 = 1;
const FULL: u8 = 2;
const READING: u8 = 3;

/// The 100 ms capture buffer plus two 20 ms drain polls.
const HOLD_MS: u64 = 140;
const MAX_SLOTS: u64 = 256;
const MAX_CYCLE_FRAMES: u32 = 4_096;
/// Above the largest ring an admitted pin sizes (192 kHz stereo, 3,839-frame cycles: 245,696 B).
const MAX_STORAGE: u64 = 256 * 1_024;

/// Where one cycle sits on the device's clocks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Stamp {
    pub(super) frames: u32,
    pub(super) host_time: u64,
    pub(super) sample_time: f64,
}

struct Cycle {
    stamp: Stamp,
    bytes: Zeroizing<Box<[u8]>>,
}

struct Slot {
    state: AtomicU8,
    cycle: UnsafeCell<Cycle>,
}

pub(super) struct Ring {
    admission: AtomicU8,
    slots: Box<[Slot]>,
    write: AtomicUsize,
    read: AtomicUsize,
}

// SAFETY: a slot's cycle is touched only by the thread whose compare-exchange moved that slot out
// of FREE or FULL, until the same thread stores FREE or FULL again, so no cycle is shared.
unsafe impl Sync for Ring {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Produced {
    Published,
    /// The fill copied less than the whole cycle; the slot was scrubbed and stays free.
    Revoked,
    /// The next slot is still unread; nothing was written.
    Full,
    /// The producer is closed or another cycle holds the claim; nothing was touched.
    Refused,
}

impl Ring {
    /// Sized from the pin: enough `cycle_frames` cycles of float32 input for 140 ms.
    pub(super) fn new(rate: u32, channels: u16, cycle_frames: u32) -> Result<Self, MicError> {
        if !(1..=2).contains(&channels) || !(1..=MAX_CYCLE_FRAMES).contains(&cycle_frames) {
            return Err(MicError::Format);
        }
        let slots = (u64::from(rate) * HOLD_MS).div_ceil(1_000 * u64::from(cycle_frames));
        let bytes = u64::from(cycle_frames) * 4 * u64::from(channels);
        if slots == 0 || slots > MAX_SLOTS || slots * bytes > MAX_STORAGE {
            return Err(MicError::Format);
        }
        let bytes = usize::try_from(bytes).map_err(|_| MicError::Format)?;
        let slots = (0..slots)
            .map(|_| Slot {
                state: AtomicU8::new(FREE),
                cycle: UnsafeCell::new(Cycle {
                    stamp: Stamp::default(),
                    bytes: Zeroizing::new(vec![0; bytes].into_boxed_slice()),
                }),
            })
            .collect();
        Ok(Self {
            admission: AtomicU8::new(OPEN),
            slots,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        })
    }

    /// The IO block's whole ring step: claims the producer, lets `fill` copy into the next free
    /// slot in place, then publishes the slot or scrubs it.
    pub(super) fn produce(&self, stamp: Stamp, fill: impl FnOnce(&mut [u8]) -> usize) -> Produced {
        if self
            .admission
            .compare_exchange(OPEN, IN_CYCLE, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Produced::Refused;
        }
        let at = self.write.load(Ordering::Relaxed);
        let produced = match self.slots.get(at) {
            Some(slot)
                if slot
                    .state
                    .compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok() =>
            {
                // SAFETY: this thread took WRITING above and alone touches the cycle until it
                // stores FULL or FREE below.
                let cycle = unsafe { &mut *slot.cycle.get() };
                if fill(&mut cycle.bytes) == cycle.bytes.len() {
                    cycle.stamp = stamp;
                    slot.state.store(FULL, Ordering::Release);
                    self.write
                        .store((at + 1) % self.slots.len(), Ordering::Relaxed);
                    Produced::Published
                } else {
                    cycle.bytes.zeroize();
                    slot.state.store(FREE, Ordering::Release);
                    Produced::Revoked
                }
            }
            _ => Produced::Full,
        };
        self.admission.store(OPEN, Ordering::Release);
        produced
    }

    /// The worker's drain step: hands the oldest published cycle to `read` in place, then
    /// zeroizes and frees its slot. None when nothing is published.
    pub(super) fn pop<T>(&self, read: impl FnOnce(Stamp, &[u8]) -> T) -> Option<T> {
        let at = self.read.load(Ordering::Relaxed);
        let slot = self.slots.get(at)?;
        slot.state
            .compare_exchange(FULL, READING, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // SAFETY: this thread took READING above and alone touches the cycle until it stores
        // FREE below.
        let cycle = unsafe { &mut *slot.cycle.get() };
        let value = read(cycle.stamp, &cycle.bytes);
        cycle.bytes.zeroize();
        slot.state.store(FREE, Ordering::Release);
        self.read
            .store((at + 1) % self.slots.len(), Ordering::Relaxed);
        Some(value)
    }

    /// Closes the producer for good. False while a cycle holds the claim, so teardown retries at
    /// its next poll and never scrubs a slot that cycle may be writing.
    pub(super) fn close(&self) -> bool {
        matches!(
            self.admission
                .compare_exchange(OPEN, CLOSED, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(CLOSED)
        )
    }

    /// Zeroizes every slot, published ones included, once the producer is closed. Before that it
    /// touches nothing and returns false.
    pub(super) fn scrub(&self) -> bool {
        if self.admission.load(Ordering::Acquire) != CLOSED {
            return false;
        }
        let mut scrubbed = true;
        for slot in &self.slots {
            let claimed = [FREE, FULL].into_iter().any(|from| {
                slot.state
                    .compare_exchange(from, READING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            });
            if !claimed {
                scrubbed = false;
                continue;
            }
            // SAFETY: this thread took READING above and alone touches the cycle until it
            // stores FREE below.
            unsafe { &mut *slot.cycle.get() }.bytes.zeroize();
            slot.state.store(FREE, Ordering::Release);
        }
        scrubbed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{FREE, FULL, MicError, Produced, READING, Ring, Stamp};
    use crate::platform::microphone::copy_batch;

    fn stamp(n: u32) -> Stamp {
        Stamp {
            frames: 4,
            host_time: u64::from(n) * 1_000,
            sample_time: f64::from(n) * 4.0,
        }
    }

    fn fill_with(byte: u8) -> impl FnOnce(&mut [u8]) -> usize {
        move |raw| {
            raw.fill(byte);
            raw.len()
        }
    }

    /// Every slot's bytes, read only while no producer or consumer runs.
    fn contents(ring: &Ring) -> Vec<u8> {
        let mut all = Vec::new();
        for slot in &ring.slots {
            let was = slot.state.load(Ordering::Acquire);
            assert!(was == FREE || was == FULL, "a slot is mid-cycle");
            slot.state.store(READING, Ordering::Release);
            // SAFETY: the test holds the only references and no cycle is running.
            all.extend_from_slice(&unsafe { &*slot.cycle.get() }.bytes);
            slot.state.store(was, Ordering::Release);
        }
        all
    }

    /// Two 16-byte slots: 50 Hz mono, four-frame cycles.
    fn small() -> Ring {
        Ring::new(50, 1, 4).unwrap()
    }

    #[test]
    fn the_ring_is_sized_for_140_ms_within_its_bounds() {
        let size = |rate, channels, frames| {
            Ring::new(rate, channels, frames).map(|ring| (ring.slots.len(), contents(&ring).len()))
        };
        assert_eq!(size(48_000, 1, 64), Ok((105, 26_880)));
        assert_eq!(size(48_000, 1, 27), Ok((249, 26_892)));
        assert_eq!(size(48_000, 1, 26), Err(MicError::Format));
        assert_eq!(size(192_000, 2, 3_839), Ok((8, 245_696)));
        assert_eq!(size(220_000, 2, 4_096), Ok((8, 262_144)));
        assert_eq!(size(240_000, 2, 4_096), Err(MicError::Format));
        assert_eq!(size(8_000, 1, 4_096), Ok((1, 16_384)));
        assert_eq!(size(0, 1, 64), Err(MicError::Format));
        for (channels, frames) in [(0, 64), (3, 64), (1, 0), (1, 4_097)] {
            assert_eq!(size(48_000, channels, frames), Err(MicError::Format));
        }
    }

    #[test]
    fn cycles_come_out_in_order_and_every_read_slot_is_zeroed() {
        let ring = small();
        assert_eq!(ring.produce(stamp(1), fill_with(0xa1)), Produced::Published);
        assert_eq!(ring.produce(stamp(2), fill_with(0xa2)), Produced::Published);
        assert_eq!(ring.produce(stamp(3), fill_with(0xa3)), Produced::Full);
        let read = |stamp: Stamp, bytes: &[u8]| (stamp, bytes.to_vec());
        assert_eq!(ring.pop(read), Some((stamp(1), vec![0xa1; 16])));
        assert_eq!(ring.produce(stamp(3), fill_with(0xa3)), Produced::Published);
        assert_eq!(ring.pop(read), Some((stamp(2), vec![0xa2; 16])));
        assert_eq!(ring.pop(read), Some((stamp(3), vec![0xa3; 16])));
        assert_eq!(ring.pop(read), None);
        assert_eq!(contents(&ring), vec![0; 32]);
    }

    #[test]
    fn a_short_fill_is_scrubbed_and_its_slot_reused() {
        let ring = small();
        let short = |raw: &mut [u8]| {
            raw.fill(0xa5);
            raw.len() - 1
        };
        assert_eq!(ring.produce(stamp(1), short), Produced::Revoked);
        assert_eq!(contents(&ring), vec![0; 32]);
        assert_eq!(ring.pop(|_, _| ()), None);
        assert_eq!(ring.produce(stamp(2), fill_with(0xa2)), Produced::Published);
        assert_eq!(ring.produce(stamp(3), fill_with(0xa3)), Produced::Published);
        assert_eq!(ring.pop(|stamp, _| stamp), Some(stamp(2)));
    }

    #[test]
    fn closing_scrubs_published_cycles_and_refuses_every_later_one() {
        let ring = small();
        assert!(!ring.scrub());
        assert_eq!(ring.produce(stamp(1), fill_with(0xa5)), Produced::Published);
        assert_eq!(ring.produce(stamp(2), fill_with(0xa5)), Produced::Published);
        assert!(ring.close());
        assert!(ring.close());
        assert!(ring.scrub());
        assert_eq!(contents(&ring), vec![0; 32]);
        assert_eq!(ring.produce(stamp(3), fill_with(0xa5)), Produced::Refused);
        assert_eq!(ring.pop(|_, _| ()), None);
    }

    #[test]
    fn teardown_waits_out_a_cycle_paused_inside_the_copy() {
        let ring = small();
        assert_eq!(ring.produce(stamp(1), fill_with(0xa1)), Produced::Published);
        let collecting = AtomicBool::new(true);
        let (paused, pause) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let (shared, flag) = (&ring, &collecting);
        thread::scope(|scope| {
            let producer = scope.spawn(move || {
                shared.produce(stamp(2), |raw| {
                    copy_batch(
                        raw,
                        Some(&[0xa5; 16]),
                        0,
                        || flag.load(Ordering::Acquire),
                        || {
                            paused.send(()).unwrap();
                            resumed.recv_timeout(Duration::from_secs(2)).unwrap();
                        },
                    )
                })
            });
            pause.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(ring.produce(stamp(3), fill_with(0xa3)), Produced::Refused);
            collecting.store(false, Ordering::Release);
            assert!(!ring.close());
            assert!(!ring.scrub());
            resume.send(()).unwrap();
            assert_eq!(producer.join().unwrap(), Produced::Revoked);
        });
        assert_eq!(
            ring.pop(|stamp, bytes| (stamp, bytes.to_vec())),
            Some((stamp(1), vec![0xa1; 16]))
        );
        assert_eq!(contents(&ring), vec![0; 32]);
        assert!(ring.close());
        assert!(ring.scrub());
        assert_eq!(ring.produce(stamp(4), fill_with(0xa4)), Produced::Refused);
    }
}
