use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static COUNT: Cell<Option<[usize; 4]>> = const { Cell::new(None) };
    static LARGEST_RELEASE: Cell<usize> = const { Cell::new(0) };
    static MATCHING_RELEASES: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
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
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(1, layout.size());
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
