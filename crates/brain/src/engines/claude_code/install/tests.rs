use super::*;
use crate::test_support::Scratch;

const V: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 306,
};

#[test]
fn the_first_native_exe_present_is_found() {
    let scratch = Scratch::new("claude-locate");
    let profile = scratch.dir();
    let paths = [r".local\bin\claude.exe", r"other\claude.exe"];
    assert_eq!(locate_in(profile, &paths), Err(FindError::NotFound));
    std::fs::create_dir_all(profile.join("other")).expect("folder");
    std::fs::write(profile.join(paths[1]), b"").expect("file");
    assert_eq!(locate_in(profile, &paths), Ok(profile.join(paths[1])));
    std::fs::create_dir_all(profile.join(r".local\bin")).expect("folder");
    std::fs::write(profile.join(paths[0]), b"").expect("file");
    assert_eq!(locate_in(profile, &paths), Ok(profile.join(paths[0])));
}

#[test]
fn a_folder_named_like_the_exe_is_not_an_install() {
    let scratch = Scratch::new("claude-locate-dir");
    std::fs::create_dir_all(scratch.dir().join("claude.exe")).expect("folder");
    assert_eq!(
        locate_in(scratch.dir(), &["claude.exe"]),
        Err(FindError::NotFound)
    );
}

#[test]
fn shims_are_refused_before_anything_is_looked_at() {
    let scratch = Scratch::new("claude-locate-shim");
    for shim in [
        "claude.cmd",
        "claude",
        "claude.ps1",
        "npm/node_modules/claude/claude.exe",
        "npm/NODE_MODULES/claude.exe",
    ] {
        assert_eq!(
            locate_in(scratch.dir(), &[shim, "claude.exe"]),
            Err(FindError::Shim),
            "{shim}"
        );
    }
    assert_eq!(
        locate_in(scratch.dir(), &["CLAUDE.EXE"]),
        Err(FindError::NotFound),
        "the extension's case doesn't matter"
    );
}

#[test]
fn the_shipped_paths_are_native_exes() {
    let scratch = Scratch::new("claude-locate-shipped");
    assert_eq!(locate(scratch.dir()), Err(FindError::NotFound));
}

#[test]
fn the_version_is_the_first_word() {
    assert_eq!(parse_version(b"2.1.306 (Claude Code)\n"), Some(V));
    assert_eq!(parse_version(b"  2.1.306\r\n"), Some(V));
    assert_eq!(parse_version(b"2.1.306"), Some(V));
    for bad in [
        &b""[..],
        b"\n",
        b"2.1",
        b"2.1.",
        b"v2.1.306",
        b"2.1.306(Claude",
        b"Claude Code 2.1.306",
        b"2.1.70000",
        b"\xff2.1.306",
    ] {
        assert_eq!(parse_version(bad), None, "{bad:?}");
    }
}
