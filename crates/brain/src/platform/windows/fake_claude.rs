//! Tests only: this test binary is also the lane tests' fake `claude.exe`. Started with
//! [`FAKE_CLAUDE`] first, it runs the fake before libtest's `main`, so its stdout holds only what
//! the fake prints.

use std::ffi::OsStr;

/// The first argument that makes this test binary the fake.
pub(crate) const FAKE_CLAUDE: &str = "--kumochi-fake-claude";

// SAFETY: `.CRT$XCU` holds pointers the C runtime calls once before `main`, on the main thread;
// `enter` has the signature it calls and returns at once unless this process is the fake.
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static ENTER: extern "C" fn() = enter;

extern "C" fn enter() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(FAKE_CLAUDE)) {
        return;
    }
    let code = crate::engines::claude_code::lane::tests::fake(args.collect());
    std::process::exit(code);
}
