//! `cargo xtask check-vendor`: fails when Cargo stops building a crate in `vendor/` from its copy
//! there. A dependency bump that asks for another version leaves the `[patch.crates-io]` entry
//! unused, and Cargo only warns, so the patched behavior would quietly disappear.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};

pub(crate) fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut checked = 0;
    for entry in fs::read_dir(root.join("vendor")).context("reading vendor/")? {
        let dir = entry?.path();
        let manifest_path = dir.join("Cargo.toml");
        let manifest = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let (name, version) = package_id(&manifest)
            .with_context(|| format!("{} names no package and version", manifest_path.display()))?;
        let output = Command::new(&cargo)
            .args(["pkgid", "--locked", name])
            .current_dir(root)
            .output()
            .context("running cargo pkgid")?;
        ensure!(
            output.status.success(),
            "Cargo.lock must hold exactly one {name}, built from vendor/: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let pkgid = String::from_utf8_lossy(&output.stdout);
        let (path, locked) = path_source(pkgid.trim()).with_context(|| {
            format!(
                "Cargo takes {name} from {}, not vendor/: re-vendor it at the version the \
                 dependency graph asks for (docs/development/gpui-patch.md)",
                pkgid.trim()
            )
        })?;
        ensure!(
            fs::canonicalize(&path).ok() == Some(fs::canonicalize(&dir)?),
            "Cargo builds {name} from {}, not {}",
            path.display(),
            dir.display()
        );
        ensure!(
            locked == version,
            "Cargo builds {name} {locked}, but vendor/ holds {version}"
        );
        println!("{name} {version}: built from {}", dir.display());
        checked += 1;
    }
    if checked == 0 {
        bail!("vendor/ holds no crates");
    }
    Ok(())
}

/// The directory and version of a `path+file://` package id, as `cargo pkgid` prints it.
fn path_source(pkgid: &str) -> Option<(PathBuf, &str)> {
    let (url, fragment) = pkgid.strip_prefix("path+file://")?.split_once('#')?;
    let version = fragment.rsplit('@').next()?;
    let path = match url.strip_prefix('/') {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => url,
    };
    Some((PathBuf::from(percent_decode(path)?), version))
}

/// Undoes a file URL's percent-encoding, as UTF-8.
fn percent_decode(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The name and version in a manifest's `[package]` table.
fn package_id(manifest: &str) -> Option<(&str, &str)> {
    let package = manifest
        .split("\n[")
        .find_map(|table| table.strip_prefix("package]"))?;
    Some((value(package, "name")?, value(package, "version")?))
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
    use std::path::PathBuf;

    use super::{package_id, path_source};

    #[test]
    fn a_path_package_id_gives_its_directory_and_version() {
        assert_eq!(
            path_source("path+file:///C:/dev/capsula/vendor/gpui-pre-windows#0.3.10"),
            Some((
                PathBuf::from("C:/dev/capsula/vendor/gpui-pre-windows"),
                "0.3.10"
            ))
        );
        assert_eq!(
            path_source("path+file:///home/k/capsula/vendor/gpui#gpui-pre-windows@0.3.10"),
            Some((PathBuf::from("/home/k/capsula/vendor/gpui"), "0.3.10"))
        );
    }

    #[test]
    fn an_encoded_path_is_decoded() {
        assert_eq!(
            path_source("path+file:///C:/Kumo%20Work/100%25/caf%C3%A9/vendor/gpui#0.3.10"),
            Some((
                PathBuf::from("C:/Kumo Work/100%/café/vendor/gpui"),
                "0.3.10"
            ))
        );
        assert_eq!(
            path_source("path+file:///C:/bad%2/vendor/gpui#0.3.10"),
            None
        );
    }

    #[test]
    fn a_registry_or_git_package_id_is_not_vendored() {
        assert_eq!(
            path_source(
                "registry+https://github.com/rust-lang/crates.io-index#gpui-pre-windows@0.3.10"
            ),
            None
        );
        assert_eq!(
            path_source(
                "git+https://github.com/zed-industries/zed?rev=08cbb88#gpui-pre-windows@0.3.10"
            ),
            None
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
