//! `cargo xtask check-core`: holds the line between `kumochi-shell-core` and the shell. It fails
//! when a UI framework enters the core's dependency tree, dev-dependencies included, when the
//! core's test support reaches a shipped shell, or when a test gate in the core is written so
//! that the shell's tests, which build the core without `cfg(test)`, would not see it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};

/// Crate-name prefixes of the UI frameworks a front end may sit on.
const FRAMEWORKS: [&str; 2] = ["gpui", "nsh"];

const SWITCH: &str =
    "cfg!(test) is false in the shell's tests; write cfg!(any(test, feature = \"test-support\"))";
const NEGATED: &str = "#[cfg(not(test))] runs in the shell's tests; write \
                       #[cfg(not(any(test, feature = \"test-support\")))]";
const MIXED: &str = "this file has test-support gates, so the shell's tests use its hooks; \
                     write #[cfg(any(test, feature = \"test-support\"))]";

pub(crate) fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let core = tree(root, &["--package", "kumochi-shell-core"])?;
    let found: Vec<&str> = core.lines().filter(|line| names_framework(line)).collect();
    ensure!(
        found.is_empty(),
        "kumochi-shell-core must build without a UI framework, but its tree holds: {}",
        found.join(", ")
    );
    let shipped = tree(
        root,
        &[
            "--package",
            "kumochi-shell",
            "--edges",
            "features,normal,build",
        ],
    )?;
    ensure!(
        !shipped.lines().any(carries_test_support),
        "kumochi-shell must take kumochi-shell-core's test-support only as a dev-dependency"
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
        "test gates in kumochi-shell-core that the shell's tests would not see:\n{}",
        faults.join("\n")
    );
    println!(
        "kumochi-shell-core: no UI framework in its tree, no test support in a shipped shell, \
         and no test gate the shell's tests would miss"
    );
    Ok(())
}

/// `cargo tree`'s lines for one package on every target, one crate per line.
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

/// The lines of one core file, counted from 1, whose test gate the shell's tests would miss.
fn gate_faults(source: &str) -> Vec<(usize, &'static str)> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let shared = source.contains("feature = \"test-support\"");
    let mut faults = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("//") {
            continue;
        }
        if line.contains("cfg!(test)") {
            faults.push((index + 1, SWITCH));
        } else if line.starts_with("#[cfg(not(test))]") {
            faults.push((index + 1, NEGATED));
        } else if shared
            && line.starts_with("#[cfg(test)]")
            && !declares_module(&lines[index + 1..])
        {
            faults.push((index + 1, MIXED));
        }
    }
    faults
}

/// Whether the item under an attribute, past any further attributes, declares a module.
fn declares_module(rest: &[&str]) -> bool {
    rest.iter()
        .find(|line| !line.starts_with("#[") && !line.starts_with("//"))
        .is_some_and(|line| {
            ["pub(crate) ", "pub(super) ", "pub "]
                .iter()
                .find_map(|visibility| line.strip_prefix(visibility))
                .unwrap_or(line)
                .starts_with("mod ")
        })
}

#[cfg(test)]
mod tests {
    use super::{MIXED, NEGATED, SWITCH, carries_test_support, gate_faults, names_framework};

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
    fn a_switch_the_shells_tests_would_miss_is_a_fault_wherever_it_is() {
        let source = "fn start() {\n    if cfg!(test) {\n        return;\n    }\n    \
                      #[cfg(not(test))]\n    open();\n}\n";
        assert_eq!(gate_faults(source), [(2, SWITCH), (5, NEGATED)]);
        let written = "fn start() {\n    if cfg!(any(test, feature = \"test-support\")) {\n        \
                       return;\n    }\n    #[cfg(not(any(test, feature = \"test-support\")))]\n    \
                       open();\n}\n// cfg!(test) in a comment\n#[cfg_attr(not(test), expect(dead_code))]\n\
                       fn kept() {}\n";
        assert_eq!(gate_faults(written), []);
    }

    #[test]
    fn a_bare_gate_is_a_fault_only_beside_a_shared_one_and_never_on_a_module() {
        let alone = "#[cfg(test)]\nfn probe() {}\n#[cfg(test)]\nmod tests;\n";
        assert_eq!(gate_faults(alone), []);
        let mixed = "#[cfg(any(test, feature = \"test-support\"))]\npub fn recording() {}\n\
                     fn send() {\n    #[cfg(test)]\n    record();\n}\n#[cfg(test)]\n\
                     #[path = \"link_tests.rs\"]\npub(crate) mod tests;\n#[cfg(test)]\nmod more {}\n";
        assert_eq!(gate_faults(mixed), [(4, MIXED)]);
    }
}
