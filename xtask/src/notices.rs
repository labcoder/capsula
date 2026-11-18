//! Generates the shipped Windows dependency and asset notices (docs/development/release.md).

mod graph;
mod licenses;
mod sources;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::voice::{MODEL_FILES, NATIVE_ARCHIVE_URL, NATIVE_FILES};
use serde_json::Value;

use licenses::Policy;

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args: Vec<_> = args.collect();
    ensure!(
        args.is_empty() || args == ["--check"],
        "usage: cargo xtask notices [--check]"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask is in the workspace")?;
    let policy = Policy::read(root)?;
    let packages = graph::packages(root)?;
    sources::check_exclusions(&packages)?;
    let mut output = fs::read_to_string(root.join("THIRD-PARTY-NOTICES.md"))?.replace("\r\n", "\n");
    output.push_str("\n\n# Bundled assets\n");
    output.push_str(&assets(root, &policy, &packages)?);
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for package in &packages {
        let name = package["name"].as_str().context("package name missing")?;
        let version = package["version"]
            .as_str()
            .context("package version missing")?;
        for (license, text) in policy.package(package)? {
            groups
                .entry((license, text))
                .or_default()
                .push(format!("{name} {version}"));
        }
    }
    output.push_str("\n# Rust dependencies\n\nThese source-package notices are not an audit of the linked installer.\n");
    for ((license, text), mut names) in groups {
        names.sort();
        names.dedup();
        writeln!(
            output,
            "\n## {license}\n\n{}\n\n{}",
            names.join("\n"),
            text.trim_end()
        )?;
    }
    let path = root.join("assets/licenses/third-party.txt");
    if args.is_empty() {
        fs::write(path, output)?;
    } else {
        ensure!(
            fs::read_to_string(path)
                .context("generate third-party notices first")?
                .replace("\r\n", "\n")
                == output,
            "third-party notices are stale; run cargo xtask notices"
        );
    }
    println!(
        "{} shipped crates; third-party notices {}",
        packages.len(),
        if args.is_empty() {
            "generated"
        } else {
            "current"
        }
    );
    Ok(())
}

fn assets(root: &Path, policy: &Policy, packages: &[Value]) -> Result<String> {
    let inventory: Value = serde_json::from_str(&fs::read_to_string(
        root.join("assets/licenses/inventory.json"),
    )?)?;
    let mut seen = BTreeSet::new();
    let expected: BTreeSet<_> = MODEL_FILES
        .iter()
        .map(|file| format!("model:{}:{}", file.name, file.sha256))
        .chain(
            NATIVE_FILES
                .iter()
                .map(|file| format!("native:{}:{}", file.name, file.sha256)),
        )
        .chain(["icons:gpui-kit-assets".to_owned()])
        .collect();
    let mut output = String::new();
    for entry in inventory
        .as_array()
        .context("asset inventory must be an array")?
    {
        let field = |key| entry[key].as_str().context("asset inventory field missing");
        let name = field("name")?;
        let license = field("license")?;
        let source = field("source")?;
        policy.asset(name, license)?;
        ensure!(
            source.starts_with("https://"),
            "asset source must be an HTTPS provenance URL"
        );
        for file in entry["files"].as_array().context("asset files missing")? {
            let file = file.as_str().context("asset file must be a string")?;
            ensure!(
                expected.contains(file) && seen.insert(file.to_owned()),
                "unknown or duplicated inventory asset"
            );
            if file.starts_with("model:") {
                let model = MODEL_FILES
                    .iter()
                    .find(|model| format!("model:{}:{}", model.name, model.sha256) == file)
                    .context("unknown model")?;
                ensure!(
                    model.url.starts_with(&format!("{source}/")),
                    "model provenance differs from the inventory"
                );
            } else if file.starts_with("native:") {
                ensure!(
                    source == NATIVE_ARCHIVE_URL,
                    "native archive differs from the inventory"
                );
            } else {
                let package = packages
                    .iter()
                    .find(|package| package["name"] == "gpui-kit-assets")
                    .context("embedded asset package missing")?;
                let (repo, revision) = package["source"]
                    .as_str()
                    .and_then(|source| source.strip_prefix("git+"))
                    .and_then(|source| source.rsplit_once('#'))
                    .context("embedded asset source or revision missing")?;
                let repo = repo.split_once('?').map_or(repo, |(url, _)| url);
                ensure!(
                    revision == field("revision")?
                        && source == format!("{repo}/tree/{revision}/crates/assets"),
                    "embedded asset source or revision needs a new inventory audit"
                );
                let dir = Path::new(
                    package["manifest_path"]
                        .as_str()
                        .context("asset manifest missing")?,
                )
                .parent()
                .context("asset directory missing")?;
                check_icons(&dir.join("assets"))?;
            }
        }
        writeln!(
            output,
            "\n## {name}\n\nLicense: {license}\nSource: {source}\n"
        )?;
        let notices = entry["notices"]
            .as_array()
            .context("asset notices missing")?;
        ensure!(!notices.is_empty(), "asset notice text is missing");
        for notice in notices {
            let notice = notice.as_str().context("notice path must be a string")?;
            let path = Path::new(notice);
            ensure!(
                path.components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                "notice path must stay inside assets/licenses"
            );
            let text = fs::read_to_string(root.join("assets/licenses").join(path))?;
            ensure!(!text.trim().is_empty(), "empty asset notice");
            writeln!(output, "{}", text.replace("\r\n", "\n").trim_end())?;
        }
    }
    coverage(&expected, &seen)?;
    Ok(output)
}

fn coverage(expected: &BTreeSet<String>, seen: &BTreeSet<String>) -> Result<()> {
    ensure!(
        expected == seen,
        "a model, native runtime or embedded asset lacks an inventory entry"
    );
    Ok(())
}

fn check_icons(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            check_icons(&entry.path())?;
        } else {
            let path = entry.path();
            let extension = path.extension().and_then(|extension| extension.to_str());
            if extension.is_some_and(|extension| {
                ["ttf", "otf", "woff", "woff2", "svg"].contains(&extension)
            }) {
                ensure!(
                    extension == Some("svg")
                        && path.components().any(|part| part.as_os_str() == "icons"),
                    "new embedded font or icon needs inventory review"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradium_model_blocks_carry_the_publishers_attribution_not_gpuis() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = Policy::read(root).unwrap();
        let dir = crate::scratch::dir("embedded-icons");
        fs::create_dir_all(dir.join("assets/icons")).unwrap();
        let package = serde_json::json!({
            "name":"gpui-kit-assets",
            "source":"git+https://github.com/longbridge/gpui-kit#99576db58ec8103417c41a092e891b4022d24e7e",
            "manifest_path": dir.join("Cargo.toml")
        });
        let output = assets(root, &policy, &[package]).unwrap();
        for (name, credit) in [
            ("phonon", "Phonon 31M English by Gradium, Apache-2.0."),
            ("juniper", "Juniper voice by Gradium, Apache-2.0."),
        ] {
            let (_, rest) = output.split_once(&format!("\n## {name}\n")).unwrap();
            let (block, _) = rest.split_once("\n## ").unwrap();
            assert!(block.contains(credit));
            assert!(block.contains("Version 2.0, January 2004"));
            assert!(!block.contains("Zed Industries"));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_added_model_or_removed_inventory_row_fails_coverage() {
        let expected = BTreeSet::from(["model:existing:hash".to_owned()]);
        assert!(coverage(&expected, &expected).is_ok());
        let additional = BTreeSet::from([
            "model:existing:hash".to_owned(),
            "model:new:hash".to_owned(),
        ]);
        assert!(coverage(&additional, &expected).is_err());
        assert!(coverage(&expected, &BTreeSet::new()).is_err());
    }
}
