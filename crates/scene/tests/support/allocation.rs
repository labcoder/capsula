use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
};

thread_local! {
    static COUNT: Cell<Option<[usize; 4]>> = const { Cell::new(None) };
    static LARGEST_RELEASE: Cell<usize> = const { Cell::new(0) };
    static MATCHING_RELEASES: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    static TAG: Cell<u8> = const { Cell::new(0) };
    static RECORD: RefCell<Option<Record>> = const { RefCell::new(None) };
    static RECORD_LOST: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub action: u8,
    pub pointer: usize,
    pub size: usize,
    pub align: usize,
    pub tag: u8,
}

struct Record {
    requests: Vec<Request>,
    overflow: bool,
}

pub(super) fn set_tag(tag: u8) -> u8 {
    TAG.with(|value| value.replace(tag))
}

fn record_request(action: u8, pointer: usize, layout: Layout) {
    let tag = TAG.try_with(Cell::get).unwrap_or(0);
    record_event(Request {
        action,
        pointer,
        size: layout.size(),
        align: layout.align(),
        tag,
    });
}

pub(super) fn checkpoint(action: u8, tag: u8) {
    record_event(Request {
        action,
        pointer: 0,
        size: 0,
        align: 0,
        tag,
    });
}

pub(super) fn handoff(pointer: usize, size: usize, align: usize) {
    let tag = TAG.with(Cell::get);
    record_event(Request {
        action: 4,
        pointer,
        size,
        align,
        tag,
    });
}

fn record_event(request: Request) {
    let _ = RECORD.try_with(|record| {
        let Ok(mut slot) = record.try_borrow_mut() else {
            let _ = RECORD_LOST.try_with(|lost| lost.set(true));
            return;
        };
        if let Some(record) = slot.as_mut() {
            if record.requests.len() == record.requests.capacity() {
                record.overflow = true;
                return;
            }
            record.requests.push(request);
        }
    });
}

struct Recording;

impl Drop for Recording {
    fn drop(&mut self) {
        RECORD.with(|record| {
            record.replace(None);
        });
    }
}

pub(super) fn requests<T>(capacity: usize, run: impl FnOnce() -> T) -> (T, Vec<Request>) {
    let record = Record {
        requests: Vec::with_capacity(capacity),
        overflow: false,
    };
    RECORD_LOST.with(|lost| lost.set(false));
    RECORD.with(|slot| {
        assert!(
            slot.replace(Some(record)).is_none(),
            "request scopes cannot nest"
        )
    });
    let guard = Recording;
    let value = run();
    let record = RECORD.with(|slot| slot.replace(None).expect("the request scope is active"));
    drop(guard);
    assert!(!record.overflow, "the request recorder overflowed");
    assert!(
        !RECORD_LOST.with(Cell::get),
        "the request recorder was reentered"
    );
    (value, record.requests)
}

struct CountingAllocator;

fn bump(index: usize, size: usize) {
    if index == 1 {
        let _ = MATCHING_RELEASES.try_with(|count| {
            if let Some((target, releases)) = count.get()
                && size == target
            {
                count.set(Some((target, releases.saturating_add(1))));
            }
        });
    }
    // A destroyed thread-local slot only disables counting during shutdown.
    let _ = COUNT.try_with(|count| {
        if let Some(mut value) = count.get() {
            value[index] = value[index].saturating_add(1);
            value[index + 2] = value[index + 2].saturating_add(size);
            if index == 1 {
                LARGEST_RELEASE.with(|largest| largest.set(largest.get().max(size)));
            }
            count.set(Some(value));
        }
    });
}

#[expect(
    unsafe_code,
    reason = "approved test-only allocation instrumentation forwards unchanged to System"
)]
// SAFETY: every returned pointer comes from System; forwarding preserves its contracts and try_with cannot unwind on TLS shutdown.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(0, layout.size());
        // SAFETY: the allocator forwards the caller's valid layout unchanged.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_request(0, pointer.addr(), layout);
        }
        pointer
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(1, layout.size());
        record_request(1, ptr.addr(), layout);
        // SAFETY: the caller's live pointer and matching layout are forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

pub(super) fn count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let (value, [allocations, _]) = activity(run);
    (value, allocations)
}

pub(super) fn activity<T>(run: impl FnOnce() -> T) -> (T, [usize; 2]) {
    let (value, counts, _) = usage(run);
    (value, counts)
}

/// Allocation and free counts, then the requested bytes they allocated and freed.
pub(super) fn usage<T>(run: impl FnOnce() -> T) -> (T, [usize; 2], [usize; 2]) {
    LARGEST_RELEASE.with(|largest| largest.set(0));
    COUNT.with(|count| {
        assert!(
            count.replace(Some([0; 4])).is_none(),
            "allocation scopes cannot nest"
        );
    });
    let value = run();
    let [allocations, deallocations, allocated, released] =
        COUNT.with(|count| count.replace(None).expect("counting scope is active"));
    assert!(
        largest_release() <= released,
        "the largest release is part of the total"
    );
    (value, [allocations, deallocations], [allocated, released])
}

pub(super) fn largest_release() -> usize {
    LARGEST_RELEASE.with(Cell::get)
}

pub(super) fn releases_of_size<T>(size: usize, run: impl FnOnce() -> T) -> (T, usize) {
    MATCHING_RELEASES.with(|count| {
        assert!(
            count.replace(Some((size, 0))).is_none(),
            "release scopes cannot nest"
        );
    });
    let value = run();
    let (_, releases) =
        MATCHING_RELEASES.with(|count| count.replace(None).expect("release scope is active"));
    (value, releases)
}

#[test]
fn release_count_only_matches_the_requested_size() {
    let control = "x".repeat(400).into_boxed_str();
    let unrelated = vec![0_u8; 1000].into_boxed_slice();
    let ((), freed) = releases_of_size(control.len(), || {
        drop(control);
        drop(unrelated);
    });
    assert_eq!(
        freed, 1,
        "calibration: only the text-sized caller drop counts"
    );
}

#[test]
fn request_recording_preserves_identity_phase_and_existing_usage_counts() {
    let before = set_tag(7);
    let (((), counts, bytes), events) = requests(10, || {
        usage(|| {
            let value = vec![0_u8; 123].into_boxed_slice();
            handoff(value.as_ptr().addr(), value.len(), 1);
            drop(value);
            checkpoint(2, 7);
        })
    });
    set_tag(before);
    assert_eq!(counts, [1, 1]);
    assert_eq!(bytes, [123, 123]);
    assert_eq!(events.len(), 4);
    assert_eq!(
        (
            events[0].action,
            events[0].size,
            events[0].align,
            events[0].tag
        ),
        (0, 123, 1, 7)
    );
    assert_eq!(
        (events[1].action, events[1].pointer),
        (4, events[0].pointer)
    );
    assert_eq!(
        (events[2].action, events[2].pointer, events[2].size),
        (1, events[0].pointer, 123)
    );
    assert_eq!((events[3].action, events[3].tag), (2, 7));
}

#[test]
#[should_panic(expected = "request recorder overflowed")]
fn a_full_request_log_fails_instead_of_dropping_events() {
    requests(0, || checkpoint(2, 1));
}

#[test]
fn a_reentered_request_recorder_fails_after_the_allocator_callback_returns() {
    let returned = Cell::new(false);
    let observed = Cell::new(None);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        requests(8, || {
            let ((value, lost), counts, bytes) = usage(|| {
                RECORD.with(|record| {
                    let held = record.borrow_mut();
                    let value = std::hint::black_box(
                        vec![0_u8; std::hint::black_box(123)].into_boxed_slice(),
                    );
                    let lost = RECORD_LOST.with(Cell::get);
                    drop(held);
                    (value, lost)
                })
            });
            observed.set(Some((counts, bytes, lost)));
            drop(value);
            returned.set(true);
        })
    }));
    assert!(
        returned.get(),
        "the allocation and measured callback returned normally"
    );
    assert_eq!(observed.get(), Some(([1, 0], [123, 0], true)));
    let panic = failure.expect_err("the request scope must reject the lost event");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("the request recorder was reentered"));
    assert!(RECORD.with(|record| record.borrow().is_none()));
    let ((), events) = requests(4, || {
        drop(std::hint::black_box(vec![0_u8; std::hint::black_box(17)]));
    });
    assert_eq!(events.len(), 2);
    assert_eq!((events[0].action, events[0].size), (0, 17));
    assert_eq!((events[1].action, events[1].size), (1, 17));
    assert_eq!(events[0].pointer, events[1].pointer);
}

#[path = "allocation_scopes.rs"]
pub(super) mod scopes;
