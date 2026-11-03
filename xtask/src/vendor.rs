//! `cargo xtask check-vendor`: fails when Cargo stops building a crate in `vendor/` from its copy
//! there. A dependency bump that asks for another version leaves the `[patch.crates-io]` entry
//! unused, and Cargo only warns, so the patched behavior would quietly disappear.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub(crate) fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let lock = fs::read_to_string(root.join("Cargo.lock")).context("reading Cargo.lock")?;
    let mut checked = 0;
    for entry in fs::read_dir(root.join("vendor")).context("reading vendor/")? {
        let manifest_path = entry?.path().join("Cargo.toml");
        let manifest = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let (name, version) = package_id(&manifest)
            .with_context(|| format!("{} names no package and version", manifest_path.display()))?;
        check_resolved(&lock, name, version)?;
        println!("{name} {version}: built from vendor/");
        checked += 1;
    }
    if checked == 0 {
        bail!("vendor/ holds no crates");
    }
    Ok(())
}

/// The name and version in a manifest's `[package]` table.
fn package_id(manifest: &str) -> Option<(&str, &str)> {
    let package = manifest
        .split("\n[")
        .find_map(|table| table.strip_prefix("package]"))?;
    Some((value(package, "name")?, value(package, "version")?))
}

/// Checks that Cargo.lock holds exactly one `name`, at `version`, with no source: a path package.
fn check_resolved(lock: &str, name: &str, version: &str) -> Result<()> {
    let packages: Vec<&str> = lock
        .split("[[package]]")
        .skip(1)
        .filter(|package| value(package, "name") == Some(name))
        .collect();
    let [package] = packages.as_slice() else {
        bail!(
            "Cargo.lock holds {} packages named {name}; it must hold only vendor/'s {version}",
            packages.len()
        );
    };
    if let Some(source) = value(package, "source") {
        bail!(
            "Cargo.lock takes {name} from {source}, not vendor/: re-vendor it at the version the \
             dependency graph asks for (docs/development/gpui-patch.md)"
        );
    }
    let locked = value(package, "version").unwrap_or("none");
    if locked != version {
        bail!("Cargo.lock holds {name} {locked}, but vendor/ holds {version}");
    }
    Ok(())
}

/// The string value of `key = "..."` in a TOML table, read line by line.
fn value<'a>(table: &'a str, key: &str) -> Option<&'a str> {
    table.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')
    })
}

#[cfg(test)]
mod tests {
    use super::{check_resolved, package_id};

    const VENDORED: &str = "[[package]]\nname = \"gpui-pre-windows\"\nversion = \"0.3.10\"\ndependencies = [\n \"windows 0.62.2\",\n]\n";
    const REGISTRY: &str = "[[package]]\nname = \"gpui-pre-windows\"\nversion = \"0.3.10\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    const OTHER: &str = "[[package]]\nname = \"gpui-pre-windows-extra\"\nversion = \"0.3.10\"\n";

    fn error(lock: &str, version: &str) -> String {
        check_resolved(lock, "gpui-pre-windows", version)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_single_path_package_at_the_vendored_version_passes() {
        let lock = format!("version = 4\n\n{OTHER}\n{VENDORED}");
        assert!(check_resolved(&lock, "gpui-pre-windows", "0.3.10").is_ok());
    }

    #[test]
    fn a_registry_copy_means_the_patch_stopped_applying() {
        assert_eq!(
            error(REGISTRY, "0.3.10"),
            "Cargo.lock takes gpui-pre-windows from registry+https://github.com/rust-lang/crates.io-index, \
             not vendor/: re-vendor it at the version the dependency graph asks for \
             (docs/development/gpui-patch.md)"
        );
    }

    #[test]
    fn a_missing_duplicate_or_mismatched_package_fails() {
        assert_eq!(
            error(OTHER, "0.3.10"),
            "Cargo.lock holds 0 packages named gpui-pre-windows; it must hold only vendor/'s 0.3.10"
        );
        assert_eq!(
            error(&format!("{VENDORED}{REGISTRY}"), "0.3.10"),
            "Cargo.lock holds 2 packages named gpui-pre-windows; it must hold only vendor/'s 0.3.10"
        );
        assert_eq!(
            error(VENDORED, "0.3.11"),
            "Cargo.lock holds gpui-pre-windows 0.3.10, but vendor/ holds 0.3.11"
        );
    }

    #[test]
    fn the_package_table_names_the_crate() {
        let manifest = "# generated\r\n\r\n[package]\r\nedition = \"2024\"\r\nname = \"gpui-pre-windows\"\r\nversion = \"0.3.10\"\r\n\r\n[lib]\r\nname = \"gpui_windows\"\r\n";
        assert_eq!(package_id(manifest), Some(("gpui-pre-windows", "0.3.10")));
        assert_eq!(package_id("[lib]\nname = \"gpui_windows\"\n"), None);
        assert_eq!(package_id(""), None);
    }
}
