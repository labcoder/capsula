use super::{collect, counts, excess, scan};
use std::{collections::BTreeMap, fs};

#[test]
fn finds_real_attributes_and_macros_even_across_lines() {
    let source = r##"#![cfg(unix)]
// #[cfg(windows)]
/* #[cfg(unix)] /* nested #[cfg(windows)] */ */
const TEXT: &str = r#"#[cfg(windows)]"#;
#[cfg(
    any(windows, target_os = "macos")
)]
#[cfg_attr(target_family = "unix", doc = "fixture")]
fn real() {
    let _ = cfg!(not(windows));
    let _ = cfg!(all(windows, feature = "gate"));
    let _ = cfg!(feature = "windows");
}
#[cfg(test)]
mod fixtures {
    #[cfg(windows)]
    fn test_helper() {}
}
"##;
    let mut sites = Vec::new();
    scan(source.parse().unwrap(), &mut |line, signature| {
        sites.push((line, signature));
    });
    assert_eq!(
        sites,
        [
            (1, "#![cfg(unix)]".to_owned()),
            (
                5,
                "#[cfg(any (windows , target_os = \"macos\"))]".to_owned()
            ),
            (8, "#[cfg_attr(target_family = \"unix\")]".to_owned()),
            (10, "cfg!(not (windows))".to_owned()),
        ],
    );
}

#[test]
fn excludes_platform_and_test_paths_but_counts_duplicate_product_sites() {
    let root = crate::scratch::dir("platform-cfg");
    for path in [
        "platform.rs",
        "platform/unix.rs",
        "normal_tests.rs",
        "test_support.rs",
        "parts/tests/case.rs",
        "fixtures/case.rs",
        "normal.rs",
    ] {
        let file = root.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "#[cfg(windows)] fn a() {}\n#[cfg(windows)] fn b() {}").unwrap();
    }
    let mut sites = BTreeMap::new();
    collect(root.path(), root.path(), &mut sites).unwrap();
    assert_eq!(
        sites,
        BTreeMap::from([("normal.rs\t#[cfg(windows)]".to_owned(), vec![1, 2])]),
    );
}

#[test]
fn baseline_substitutions_and_duplicate_growth_fail_but_removals_pass() {
    let base = counts("a\r\na\r\nb\r\n");
    let smaller = counts("a\nb\n");
    assert_eq!(excess(&smaller, &base), []);
    assert_eq!(excess(&base, &smaller), [("a", 1)]);
    let substituted = counts("a\na\nc\n");
    assert_eq!(excess(&substituted, &base), [("c", 1)]);
    let duplicated = counts("a\na\na\nb\n");
    assert_eq!(excess(&duplicated, &base), [("a", 1)]);
}

#[test]
fn detects_cfg_select_literals_and_nested_applied_cfg() {
    let source = r#"#[cfg(all(windows, true))]
#[cfg_attr(all(), cfg(unix))]
fn select() {
    let _ = cfg_select! {
        target_os = "macos" => "apple",
        not(unix) => { "other" }
        windows => "win",
        _ => "fallback",
    };
}"#;
    let mut sites = Vec::new();
    scan(source.parse().unwrap(), &mut |line, signature| {
        sites.push((line, signature));
    });
    assert_eq!(
        sites,
        [
            (1, "#[cfg(all (windows , true))]".to_owned()),
            (2, "#[cfg_attr(all ()) -> cfg(unix)]".to_owned()),
            (5, "cfg_select!(target_os = \"macos\")".to_owned()),
            (6, "cfg_select!(not (unix))".to_owned()),
            (7, "cfg_select!(windows)".to_owned()),
        ]
    );
}

#[test]
fn skips_test_gated_siblings_and_macro_bodies_and_normalizes_commas() {
    let source = r"#[cfg(windows)]
#[cfg(all(test, unix))]
fn wrapped() { let _ = cfg!(windows); }
#[cfg(test)]
thread_local! { #[cfg(windows)] static N: usize = 0; }
#[test]
fn test_fn() { let _ = cfg!(windows); }
#[cfg(any(windows, unix,))]
fn trailing() {}
#[cfg(any(windows, unix))]
fn no_trailing() {}";
    let mut sites = Vec::new();
    scan(source.parse().unwrap(), &mut |line, signature| {
        sites.push((line, signature));
    });
    assert_eq!(
        sites,
        [
            (8, "#[cfg(any (windows , unix))]".to_owned()),
            (10, "#[cfg(any (windows , unix))]".to_owned()),
        ]
    );
}
