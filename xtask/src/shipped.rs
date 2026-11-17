//! `cargo xtask check-shipped`: what a packaged Kumochi is built from, and the features it must
//! never carry. The repository has no packaging task yet; one that is added builds from
//! [`SELECTION`], so the package and this check can't name different builds.

use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result, ensure};

/// The shipped selection, as cargo arguments: the two binaries' packages with their default
/// features and nothing added.
pub(crate) const SELECTION: [&str; 4] =
    ["--package", "kumochi-shell", "--package", "kumochi-brain"];

/// The shell's features that exist for evidence builds only.
const NEVER_SHIPPED: [&str; 1] = ["screen-evidence"];

pub(crate) fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // Depth 0 prints each selected package with the features this selection turns on in it.
    let output = Command::new(cargo)
        .args(["tree", "--locked", "--target", "all", "--prefix", "none"])
        .args(["--depth", "0", "--edges", "normal,build"])
        .args(["--format", "{p}|{f}"])
        .args(SELECTION)
        .current_dir(root)
        .output()
        .context("running cargo tree")?;
    ensure!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let report = String::from_utf8(output.stdout).context("cargo tree printed text")?;
    let found = never_shipped(&report)?;
    ensure!(
        found.is_empty(),
        "the shipped selection turns on kumochi-shell features that must never ship: {}",
        found.join(", ")
    );
    println!("the shipped selection turns on no evidence feature in kumochi-shell");
    Ok(())
}

/// The never-shipped features a `{p}|{f}` report says are on in `kumochi-shell` itself: its
/// own line's list, whether named directly or brought in by `default`. Another package's
/// feature of the same name is not the shell's. A report without the shell proves nothing.
fn never_shipped(report: &str) -> Result<Vec<&str>> {
    let mut shell = report
        .lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(package, _)| package.split_whitespace().next() == Some("kumochi-shell"));
    let (_, features) = shell
        .next()
        .context("cargo tree's report has no line for kumochi-shell")?;
    ensure!(
        shell.next().is_none(),
        "cargo tree's report names kumochi-shell twice"
    );
    Ok(features
        .split(',')
        .map(str::trim)
        .filter(|feature| NEVER_SHIPPED.contains(feature))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::never_shipped;

    const SHELL: &str = "kumochi-shell v0.1.2 (/work/kumochi/crates/shell)";
    const BRAIN: &str = "kumochi-brain v0.1.2 (/work/kumochi/crates/brain)";

    #[test]
    fn the_evidence_feature_is_found_named_directly_and_through_default() {
        let none: [&str; 0] = [];
        assert_eq!(
            never_shipped(&format!("{SHELL}|\n\n{BRAIN}|test-support\n")).unwrap(),
            none
        );
        assert_eq!(
            never_shipped(&format!("{SHELL}|screen-evidence\n")).unwrap(),
            ["screen-evidence"]
        );
        assert_eq!(
            never_shipped(&format!("{SHELL}|default,screen-evidence\n{BRAIN}|\n")).unwrap(),
            ["screen-evidence"]
        );
    }

    #[test]
    fn only_the_shell_s_own_line_counts_and_a_report_without_it_fails() {
        let none: [&str; 0] = [];
        assert_eq!(
            never_shipped(&format!("{BRAIN}|screen-evidence\n{SHELL}|default\n")).unwrap(),
            none
        );
        assert_eq!(
            never_shipped(&format!("{SHELL}|screen-evidence-fixture\n")).unwrap(),
            none
        );
        assert!(never_shipped(&format!("{BRAIN}|\n")).is_err());
        assert!(never_shipped("").is_err());
        assert!(never_shipped(&format!("{SHELL}|\n{SHELL}|screen-evidence\n")).is_err());
    }
}
