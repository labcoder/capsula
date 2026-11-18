use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use serde_json::Value;

const SHIPPED_ROOTS: [&str; 4] = ["-p", "kumochi-shell", "-p", "kumochi-brain"];
const WINDOWS: &str = "x86_64-pc-windows-msvc";

pub(super) fn packages(root: &Path) -> Result<Vec<Value>> {
    let metadata = cargo(
        root,
        [
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            WINDOWS,
        ],
    )?;
    let metadata: Value = serde_json::from_str(&metadata)?;
    let tree = cargo(
        root,
        ["tree", "--locked", "--target", WINDOWS]
            .into_iter()
            .chain(SHIPPED_ROOTS)
            .chain(["--edges", "normal", "--prefix", "none", "--format", "{p}"]),
    )?;
    select(&metadata, &tree)
}

fn cargo<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .env_remove("CARGO_MANIFEST_DIR")
        .current_dir(root)
        .output()
        .context("running Cargo for the shipped dependency closure")?;
    ensure!(
        output.status.success(),
        "Cargo dependency inspection failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn select(metadata: &Value, tree: &str) -> Result<Vec<Value>> {
    let packages = metadata["packages"]
        .as_array()
        .context("missing packages")?;
    let members = metadata["workspace_members"]
        .as_array()
        .context("missing workspace members")?;
    let mut indexed: BTreeMap<(&str, &str), Vec<&Value>> = BTreeMap::new();
    for package in packages {
        indexed
            .entry((
                package["name"].as_str().context("package name missing")?,
                package["version"].as_str().context("version missing")?,
            ))
            .or_default()
            .push(package);
    }
    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for line in tree.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let name = fields.next().context("empty dependency row")?;
        let version = fields
            .next()
            .and_then(|value| value.strip_prefix('v'))
            .context("invalid dependency version")?;
        let matches = indexed
            .get(&(name, version))
            .context("unknown dependency")?;
        ensure!(
            matches.len() == 1,
            "ambiguous package source for {name} {version}"
        );
        let package = matches[0];
        if !members.contains(&package["id"]) && seen.insert((name, version)) {
            selected.push(package.clone());
        }
    }
    ensure!(!selected.is_empty(), "empty shipped dependency closure");
    selected.sort_by_key(|package| {
        (
            package["name"].as_str().unwrap_or_default().to_owned(),
            package["version"].as_str().unwrap_or_default().to_owned(),
        )
    });
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tree_selection_excludes_members_deduplicates_and_rejects_invalid_rows() {
        let metadata = json!({"workspace_members":["own"], "packages":[
            {"id":"own","name":"kumochi-shell","version":"1.0.0"},
            {"id":"normal","name":"normal","version":"1.0.0"},
            {"id":"dev","name":"dev","version":"1.0.0"},
            {"id":"build","name":"build","version":"1.0.0"},
            {"id":"optional","name":"proptest","version":"1.0.0"}
        ]});
        assert_eq!(
            select(
                &metadata,
                "kumochi-shell v1.0.0\nnormal v1.0.0\nnormal v1.0.0 (*)\n"
            )
            .unwrap(),
            vec![json!({"id":"normal","name":"normal","version":"1.0.0"})]
        );
        assert!(select(&metadata, "unknown v1.0.0\n").is_err());
        assert!(select(&metadata, "normal\n").is_err());
        assert!(select(&metadata, "").is_err());
        let mut ambiguous = metadata;
        ambiguous["packages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"another-source","name":"normal","version":"1.0.0"}));
        assert!(select(&ambiguous, "normal v1.0.0\n").is_err());
    }
}
