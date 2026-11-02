use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn bump() {
    // A destroyed thread-local slot only disables counting during shutdown.
    let _ = COUNT.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value.saturating_add(1)));
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
        bump();
        // SAFETY: the allocator forwards the caller's valid layout unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's live pointer and matching layout are forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

pub(super) fn count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|count| {
        assert!(
            count.replace(Some(0)).is_none(),
            "allocation scopes cannot nest"
        );
    });
    let value = run();
    let allocations = COUNT.with(|count| count.replace(None).expect("counting scope is active"));
    (value, allocations)
}
