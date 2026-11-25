use std::cell::Cell;

use super::*;

fn admitted(major: Option<u32>, framework: fn() -> bool) -> (Result<(), Failure>, bool) {
    let asked = Cell::new(false);
    let result = admit(
        || major,
        || {
            asked.set(true);
            framework()
        },
    );
    (result, asked.get())
}

#[test]
fn an_older_or_unknown_system_refuses_unsupported_without_loading_the_framework() {
    for major in [Some(13), Some(10), None] {
        assert_eq!(
            admitted(major, || true),
            (Err(Failure::Refused(Refusal::Unsupported)), false)
        );
    }
}

#[test]
fn macos_14_loads_the_framework_and_opens_only_when_it_is_complete() {
    assert_eq!(admitted(Some(14), || true), (Ok(()), true));
    assert_eq!(admitted(Some(26), || true), (Ok(()), true));
    assert_eq!(
        admitted(Some(14), || false),
        (Err(Failure::Refused(Refusal::Unsupported)), true)
    );
}

#[test]
fn the_major_version_reads_from_a_nul_terminated_product_version() {
    assert_eq!(major(b"26.5.1\0"), Some(26));
    assert_eq!(major(b"14.0\0"), Some(14));
    assert_eq!(major(b"14\0"), Some(14));
    assert_eq!(major(b"10.15.7\0junk"), Some(10));
    assert_eq!(major(b"26.5.1"), None);
    assert_eq!(major(b"\0"), None);
    assert_eq!(major(b"x.1\0"), None);
}
