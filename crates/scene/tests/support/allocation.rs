use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static COUNT: Cell<Option<[usize; 2]>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn bump(index: usize) {
    // A destroyed thread-local slot only disables counting during shutdown.
    let _ = COUNT.try_with(|count| {
        if let Some(mut value) = count.get() {
            value[index] = value[index].saturating_add(1);
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
        bump(0);
        // SAFETY: the allocator forwards the caller's valid layout unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(1);
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
    COUNT.with(|count| {
        assert!(
            count.replace(Some([0, 0])).is_none(),
            "allocation scopes cannot nest"
        );
    });
    let value = run();
    let counts = COUNT.with(|count| count.replace(None).expect("counting scope is active"));
    (value, counts)
}
