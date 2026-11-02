use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn bump() {
    COUNT.with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value.saturating_add(1)));
        }
    });
}

#[expect(
    unsafe_code,
    reason = "approved test-only allocation instrumentation forwards unchanged to System"
)]
// SAFETY: all operations preserve System's contracts; the const thread-local counter cannot allocate or unwind.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the allocator forwards the caller's valid layout unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the allocator forwards the caller's valid layout unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: the pointer, layout and new size retain the caller's allocation contract.
        unsafe { System.realloc(ptr, layout, new_size) }
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
