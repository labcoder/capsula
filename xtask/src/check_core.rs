//! `cargo xtask check-core`: holds the line between `kumochi-shell-core` and the shell. It fails
//! when the core depends on a crate that is not on its list, when a UI framework enters its
//! tree, when its test support reaches a binary that ships, or when a test gate in the core is
//! written in a form the shell's tests could miss: they build the core without `cfg(test)`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};

/// The crates the core may depend on directly, dev-dependencies included. A change that gives
/// the core a dependency adds it here, so the new line is reviewed.
const DEPENDENCIES: [&str; 17] = [
    "async-channel",
    "cpal",
    "fluent-bundle",
    "keyring",
    "kumochi-audio",
    "kumochi-brain",
    "kumochi-protocol",
    "kumochi-scene",
    "kumochi-storage",
    "libc",
    "thiserror",
    "tracing",
    "tracing-appender",
    "tracing-subscriber",
    "unic-langid",
    "windows",
    "zeroize",
];

/// Crate-name prefixes of the UI frameworks a front end may sit on. The whole tree is searched
/// for them, because a listed dependency could bring one in.
const FRAMEWORKS: [&str; 2] = ["gpui", "nsh"];

/// The gate that is on in the core's own tests and in the shell's.
const SHARED: &str = "any(test, feature = \"test-support\")";
const NOT_SHARED: &str = "not(any(test, feature = \"test-support\"))";

const SWITCH: &str = "the shell's tests build this crate without cfg(test); gate on \
                      any(test, feature = \"test-support\") or its negation";
const STATEMENT: &str = "a bare test gate may only sit on an item; on a statement, field, arm or \
                         parameter write #[cfg(any(test, feature = \"test-support\"))]";
const MIXED: &str = "this file has test-support gates, so the shell's tests use its hooks; \
                     write #[cfg(any(test, feature = \"test-support\"))]";
const LINT: &str = "cfg_attr on a test gate may only carry expect(...) or allow(...)";
const UNCLOSED: &str = "this cfg has no closing parenthesis";

pub(crate) fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let direct = tree(
        root,
        &[
            "--package",
            "kumochi-shell-core",
            "--depth",
            "1",
            "--edges",
            "normal,build,dev",
        ],
    )?;
    let unlisted: Vec<&str> = direct
        .lines()
        .skip(1)
        .filter_map(crate_name)
        .filter(|name| !DEPENDENCIES.contains(name))
        .collect();
    ensure!(
        unlisted.is_empty(),
        "kumochi-shell-core depends on crates that are not on the list in \
         xtask/src/check_core.rs: {}",
        unlisted.join(", ")
    );
    let whole = tree(root, &["--package", "kumochi-shell-core"])?;
    let found: Vec<&str> = whole.lines().filter(|line| names_framework(line)).collect();
    ensure!(
        found.is_empty(),
        "kumochi-shell-core must build without a UI framework, but its tree holds: {}",
        found.join(", ")
    );
    let shipped = tree(root, &["--workspace", "--edges", "features,normal,build"])?;
    ensure!(
        !shipped.lines().any(carries_test_support),
        "kumochi-shell-core's test-support may only be a dev-dependency's feature"
    );
    let mut files = Vec::new();
    sources(&root.join("crates/shell-core/src"), &mut files)?;
    let mut faults = Vec::new();
    for file in &files {
        let source =
            fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        for (line, why) in gate_faults(&source) {
            faults.push(format!("{}:{line}: {why}", file.display()));
        }
    }
    ensure!(
        faults.is_empty(),
        "test gates in kumochi-shell-core that the shell's tests could miss:\n{}",
        faults.join("\n")
    );
    println!(
        "kumochi-shell-core: only listed dependencies, no UI framework in its tree, no test \
         support in a shipped binary, and no test gate the shell's tests could miss"
    );
    Ok(())
}

/// `cargo tree`'s lines for a selection on every target, one crate per line.
fn tree(root: &Path, selection: &[&str]) -> Result<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["tree", "--locked", "--target", "all", "--prefix", "none"])
        .args(selection)
        .current_dir(root)
        .output()
        .context("running cargo tree")?;
    ensure!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("cargo tree printed text")
}

/// The crate a `cargo tree` line names, if it names one.
fn crate_name(line: &str) -> Option<&str> {
    line.split_whitespace()
        .next()
        .filter(|name| !name.starts_with('['))
}

fn names_framework(line: &str) -> bool {
    FRAMEWORKS.iter().any(|prefix| line.starts_with(prefix))
}

fn carries_test_support(line: &str) -> bool {
    line.starts_with("kumochi-shell-core feature \"test-support\"")
}

/// Every Rust file under a folder.
fn sources(folder: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", folder.display()))?
            .path();
        if path.is_dir() {
            sources(&path, found)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `cfg!(...)`, a value in an expression.
    Switch,
    /// `#[cfg(...)]`, on whatever follows it.
    Outer,
    /// `#![cfg(...)]`, on the module it opens.
    Inner,
    /// `#[cfg_attr(...)]` or `#![cfg_attr(...)]`.
    Lint,
}

const OPENERS: [(&str, Kind); 5] = [
    ("cfg!(", Kind::Switch),
    ("#[cfg(", Kind::Outer),
    ("#![cfg(", Kind::Inner),
    ("#[cfg_attr(", Kind::Lint),
    ("#![cfg_attr(", Kind::Lint),
];

/// The lines of one core file, counted from 1, whose `cfg` names `test` in a form that is not
/// one of the allowed ones:
///
/// - `any(test, feature = "test-support")` or its negation, anywhere;
/// - `test` or `all(test, ...)` on an item, which is then absent from the shell's tests, and
///   only on a module once the file shares a hook through `test-support`;
/// - `cfg_attr` on any test gate, carrying only `expect` or `allow`.
fn gate_faults(source: &str) -> Vec<(usize, &'static str)> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let beside_shared = source
        .contains("feature = \"test-support\"")
        .then_some(MIXED);
    let mut faults = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("//") {
            continue;
        }
        for (opener, kind) in OPENERS {
            for (at, _) in line.match_indices(opener) {
                let why = match closed(&lines[index..], at + opener.len()) {
                    Some((body, rest)) => fault(kind, &body, &rest, beside_shared),
                    None => Some(UNCLOSED),
                };
                faults.extend(why.map(|why| (index + 1, why)));
            }
        }
    }
    faults.sort_unstable();
    faults
}

/// The text inside a `cfg`'s parentheses, which may run over several lines, with the lines
/// that follow its closing one; the first of them is what is left of that line.
fn closed<'a>(lines: &[&'a str], from: usize) -> Option<(String, Vec<&'a str>)> {
    let mut body = String::new();
    let mut depth = 1_u32;
    let mut quoted = false;
    for (index, line) in lines.iter().enumerate() {
        let start = if index == 0 { from } else { 0 };
        for (at, character) in line.char_indices().skip_while(|(at, _)| *at < start) {
            match character {
                '"' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                let mut rest = vec![line[at + 1..].trim_start_matches(']').trim()];
                rest.extend(&lines[index + 1..]);
                return Some((body, rest));
            }
            body.push(character);
        }
        body.push(' ');
    }
    None
}

/// Why one `cfg` is a fault, if it is. `beside_shared` is what a bare gate on an item other
/// than a module earns: a fault only in a file that shares a hook through `test-support`.
fn fault(
    kind: Kind,
    body: &str,
    rest: &[&str],
    beside_shared: Option<&'static str>,
) -> Option<&'static str> {
    let parts = parts(body);
    if !parts.iter().any(|part| names_test(part)) {
        return None;
    }
    let predicate = parts.first().map_or("", String::as_str);
    let is_shared = predicate == SHARED || predicate == NOT_SHARED;
    match kind {
        Kind::Switch => (!is_shared).then_some(SWITCH),
        Kind::Lint => {
            let lints = parts.len() > 1
                && parts[1..]
                    .iter()
                    .all(|part| part.starts_with("expect(") || part.starts_with("allow("));
            (!lints).then_some(LINT)
        }
        Kind::Outer | Kind::Inner if is_shared => None,
        Kind::Outer | Kind::Inner if !only_in_tests(predicate) => Some(SWITCH),
        Kind::Inner => None,
        Kind::Outer => match gated(rest).and_then(item) {
            None => Some(STATEMENT),
            Some("mod") => None,
            Some(_) => beside_shared,
        },
    }
}

/// The comma-separated parts of a `cfg`'s text at its top level, each on one line with single
/// spaces.
fn parts(body: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut depth = 0_u32;
    let mut quoted = false;
    for character in body.chars() {
        match character {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            _ => {}
        }
        if character == ',' && depth == 0 && !quoted {
            parts.push(String::new());
        } else if let Some(part) = parts.last_mut() {
            part.push(character);
        }
    }
    parts
        .iter()
        .map(|part| {
            part.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace("( ", "(")
                .replace(", )", ")")
                .replace(" )", ")")
        })
        .filter(|part| !part.is_empty())
        .collect()
}

/// Whether a predicate names `test` outside a string, or the `test-support` feature.
fn names_test(predicate: &str) -> bool {
    if predicate.contains("feature = \"test-support\"") {
        return true;
    }
    let mut quoted = false;
    let code: String = predicate
        .chars()
        .filter(|character| {
            if *character == '"' {
                quoted = !quoted;
            }
            !quoted && *character != '"'
        })
        .collect();
    words(&code).any(|word| word == "test")
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
}

/// Whether a predicate is true only in this crate's own tests.
fn only_in_tests(predicate: &str) -> bool {
    predicate == "test"
        || predicate
            .strip_prefix("all(test, ")
            .and_then(|others| others.strip_suffix(')'))
            .is_some_and(|others| !names_test(others))
}

/// What an attribute is on: the rest of its line, or the first line under it that is not a
/// comment or another attribute.
fn gated<'a>(rest: &[&'a str]) -> Option<&'a str> {
    let mut open = 0_i32;
    for line in rest {
        if open == 0 && (line.is_empty() || line.starts_with("//")) {
            continue;
        }
        if open == 0 && !line.starts_with("#[") {
            return Some(line);
        }
        let mut quoted = false;
        for character in line.chars() {
            match character {
                '"' => quoted = !quoted,
                '(' | '[' if !quoted => open += 1,
                ')' | ']' if !quoted => open -= 1,
                _ => {}
            }
        }
    }
    None
}

/// The keyword of the item a line declares, if it declares one.
fn item(line: &str) -> Option<&str> {
    const ITEMS: [&str; 11] = [
        "mod",
        "fn",
        "impl",
        "use",
        "static",
        "struct",
        "enum",
        "type",
        "trait",
        "union",
        "macro_rules",
    ];
    let line = match line.strip_prefix("pub(") {
        Some(scope) => scope.split_once(") ")?.1,
        None => line.strip_prefix("pub ").unwrap_or(line),
    };
    let mut words = words(line);
    let first = words.next()?;
    match first {
        "const" => Some(words.next().filter(|word| *word == "fn").unwrap_or(first)),
        "extern" if line.starts_with("extern crate ") => Some("crate"),
        "async" | "unsafe" | "extern" => words.take(3).find(|word| ITEMS.contains(word)),
        _ => ITEMS.contains(&first).then_some(first),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LINT, MIXED, STATEMENT, SWITCH, UNCLOSED, carries_test_support, crate_name, gate_faults,
        names_framework,
    };

    #[test]
    fn a_tree_line_names_its_crate_and_a_heading_names_none() {
        assert_eq!(crate_name("keyring v3.6.3"), Some("keyring"));
        assert_eq!(
            crate_name("kumochi-protocol v0.1.2 (/work/kumochi/crates/protocol)"),
            Some("kumochi-protocol")
        );
        assert_eq!(crate_name("tracing v0.1.41 (*)"), Some("tracing"));
        assert_eq!(crate_name("[dev-dependencies]"), None);
        assert_eq!(crate_name(""), None);
    }

    #[test]
    fn a_framework_line_is_found_and_a_lookalike_is_not() {
        assert!(names_framework("gpui-pre v0.3.10"));
        assert!(names_framework(
            "gpui-kit v0.8.1 (https://github.com/longbridge/gpui-kit)"
        ));
        assert!(names_framework("nsh-core v0.1.0"));
        assert!(!names_framework(
            "kumochi-shell-core v0.1.2 (/work/kumochi/crates/shell-core)"
        ));
        assert!(!names_framework("windows v0.62.2"));
        assert!(!names_framework(""));
    }

    #[test]
    fn only_the_cores_test_support_feature_counts() {
        assert!(carries_test_support(
            "kumochi-shell-core feature \"test-support\""
        ));
        assert!(!carries_test_support(
            "kumochi-shell-core feature \"default\""
        ));
        assert!(!carries_test_support(
            "kumochi-brain feature \"test-support\""
        ));
        assert!(!carries_test_support("kumochi-shell-core v0.1.2"));
    }

    #[test]
    fn every_allowed_form_passes() {
        let source = r#"
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the callers (two of them) arrive later"
    )
)]
#![cfg(all(test, windows))]
// cfg!(test) in a comment
/// and #[cfg(not(test))] in a doc comment
fn start() {
    if cfg!(any(test, feature = "test-support")) {
        return;
    }
    if cfg!(not(any(test, feature = "test-support"))) && cfg!(debug_assertions) {
        open();
    }
    #[cfg(not(any(test, feature = "test-support")))]
    open();
    #[cfg(windows)]
    open();
}
#[cfg_attr(test, allow(unused))]
#[cfg_attr(all(test, windows), expect(dead_code, reason = "only Windows tests use it"))]
#[cfg_attr(
    not(any(test, feature = "test-support")),
    expect(dead_code, reason = "only tests borrow it")
)]
fn kept() {}
#[cfg(any(test, feature = "test-support"))]
pub fn hook() {}
"#;
        assert_eq!(gate_faults(source), []);
        let items = r#"
#[cfg(test)]
pub(crate) fn probe() {}
#[cfg(test)]
#[expect(
    dead_code,
    reason = "shared helpers [some unused] serve several test binaries"
)]
mod allocation;
#[cfg(all(test, windows))]
mod native;
#[cfg(test)]
impl PartialEq for Effect {}
#[cfg(test)]
const fn limit() -> usize { 4 }
#[cfg(test)]
const LIMIT: usize = 4;
#[cfg(test)]
pub(in crate::presenter) use fixture::step;
#[cfg(test)]
unsafe extern "system" fn callback() {}
#[cfg(test)]
extern crate alloc;
#[cfg(test)]
mod tests;
"#;
        assert_eq!(gate_faults(items), []);
        assert_eq!(gate_faults(""), []);
    }

    #[test]
    fn a_switch_the_shells_tests_would_miss_is_a_fault_in_every_spelling() {
        for switch in [
            "if cfg!(test) {",
            "if cfg!(not(test)) {",
            "if cfg!(all(test, windows)) {",
            "if cfg!(any(test, debug_assertions)) {",
            "if cfg!(any(test, feature = \"test-support\", windows)) {",
            "#[cfg(not(test))]",
            "#[cfg(all(not(test), windows))]",
            "#[cfg(any(not(test), feature = \"live\"))]",
            "#[cfg(any(test, debug_assertions))]",
            "#[cfg(any(test, feature = \"test-support\", windows))]",
            "#[cfg(all(test, not(test)))]",
            "#![cfg(not(test))]",
            "#![cfg(any(test, windows))]",
            "#[cfg(feature = \"test-support\")]",
            "#[cfg(not(feature = \"test-support\"))]",
            "if cfg!(feature = \"test-support\") {",
        ] {
            let source = format!("fn start() {{\n    {switch}\n    open();\n}}\n");
            assert_eq!(gate_faults(&source), [(2, SWITCH)], "{switch}");
        }
    }

    #[test]
    fn a_bare_gate_on_anything_but_an_item_is_a_fault() {
        for (gate, under) in [
            ("#[cfg(test)]", "return Self::stub();"),
            ("#[cfg(test)]", "record(&frame);"),
            ("#[cfg(test)]", "crate::record(&frame);"),
            ("#[cfg(test)]", "{"),
            ("#[cfg(test)]", "unsafe { record(&frame) }"),
            ("#[cfg(test)]", "unsafe { crate::record(&frame) }"),
            ("#[cfg(test)]", "let recorded = Vec::new();"),
            ("#[cfg(test)]", "recorded: Vec<Frame>,"),
            ("#[cfg(test)]", "StopReason::Release => {}"),
            ("#[cfg(all(test, windows))]", "record(&frame);"),
            ("#[cfg(test)] after_authorized: impl FnOnce(),", ""),
            ("#[cfg(test)]", ""),
        ] {
            let source = format!("fn start() {{\n    {gate}\n    {under}\n");
            assert_eq!(gate_faults(&source), [(2, STATEMENT)], "{gate} {under}");
        }
    }

    #[test]
    fn a_cfg_attr_on_a_test_gate_carries_only_lints() {
        for attribute in [
            "#[cfg_attr(test, path = \"stub.rs\")]",
            "#[cfg_attr(test, derive(Clone))]",
            "#[cfg_attr(not(test), expect(dead_code), path = \"real.rs\")]",
            "#[cfg_attr(windows, cfg(test))]",
            "#![cfg_attr(test, no_std)]",
            "#[cfg_attr(test)]",
        ] {
            let source = format!("{attribute}\nmod capture;\n");
            assert_eq!(gate_faults(&source), [(1, LINT)], "{attribute}");
        }
    }

    #[test]
    fn a_cfg_that_never_closes_is_a_fault() {
        assert_eq!(gate_faults("#[cfg(test\nmod tests;\n"), [(1, UNCLOSED)]);
        assert_eq!(
            gate_faults("fn start() {\n    if cfg!(any(test, \"unclosed) {\n"),
            [(2, UNCLOSED)]
        );
    }

    #[test]
    fn a_bare_gate_is_a_fault_beside_a_shared_one_except_on_a_module() {
        let mixed = "#[cfg(any(test, feature = \"test-support\"))]\npub fn recording() {}\n\
                     #[cfg(test)]\nfn probe() {}\n#[cfg(test)]\n\
                     #[path = \"link_tests.rs\"]\npub(crate) mod tests;\n#[cfg(test)]\nmod more {}\n";
        assert_eq!(gate_faults(mixed), [(3, MIXED)]);
    }
}
