use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use spdx::detection::scan::Scanner;
use spdx::detection::{Store, TextData};
use spdx::{Expression, Licensee, ParseMode};

pub(super) struct Policy {
    root: PathBuf,
    allow: Vec<Licensee>,
    exceptions: BTreeMap<String, Vec<Licensee>>,
    confidence: f32,
    texts: Store,
}

impl Policy {
    pub(super) fn read(root: &Path) -> Result<Self> {
        let policy: toml::Value = toml::from_str(&fs::read_to_string(root.join("deny.toml"))?)?;
        let licenses = &policy["licenses"];
        let parse = |value: &toml::Value| -> Result<Vec<Licensee>> {
            value
                .as_array()
                .context("license list missing")?
                .iter()
                .map(|value| {
                    Licensee::parse(value.as_str().context("license must be a string")?)
                        .context("invalid policy license")
                })
                .collect()
        };
        let mut exceptions = BTreeMap::new();
        for entry in licenses["exceptions"]
            .as_array()
            .context("exceptions missing")?
        {
            let name = entry["crate"].as_str().context("exception crate missing")?;
            ensure!(
                exceptions
                    .insert(name.to_owned(), parse(&entry["allow"])?)
                    .is_none(),
                "duplicate license exception"
            );
        }
        let confidence: f32 = licenses["confidence-threshold"].to_string().parse()?;
        ensure!(
            confidence.is_finite() && (0.0..=1.0).contains(&confidence),
            "invalid license confidence threshold"
        );
        Ok(Self {
            root: root.to_owned(),
            allow: parse(&licenses["allow"])?,
            exceptions,
            confidence,
            texts: Store::load_inline().context("loading SPDX license texts")?,
        })
    }

    fn choose(
        &self,
        name: &str,
        expression: &str,
        available: Option<&[String]>,
    ) -> Result<Vec<String>> {
        let expression = Expression::parse_mode(expression, ParseMode::LAX)
            .context("invalid SPDX expression")?;
        let accepted = self
            .allow
            .iter()
            .chain(self.exceptions.get(name).into_iter().flatten())
            .filter(|license| available.is_none_or(|ids| ids.contains(&license.to_string())));
        Ok(expression
            .minimized_requirements(accepted)
            .context("license expression has no permitted branch")?
            .iter()
            .map(ToString::to_string)
            .collect())
    }

    pub(super) fn asset(&self, name: &str, expression: &str) -> Result<()> {
        let expression = Expression::parse(expression).context("invalid asset license")?;
        ensure!(
            expression.evaluate(|req| {
                self.allow.iter().any(|allowed| allowed.satisfies(req))
                    || matches!(
                        (name, req.to_string().as_str()),
                        ("parakeet", "CC-BY-4.0") | ("voice-native-runtime", "MPL-2.0" | "BSL-1.0")
                    )
            }),
            "asset license needs an explicit policy decision"
        );
        Ok(())
    }

    fn identify(&self, text: &str) -> BTreeSet<String> {
        let strategy = Scanner::new(&self.texts)
            .confidence_threshold(self.confidence)
            .optimize(true);
        let found = strategy.scan(&TextData::new(text));
        found
            .license
            .into_iter()
            .map(|license| license.name.to_owned())
            .chain(
                found
                    .containing
                    .iter()
                    .map(|part| part.license.name.to_owned()),
            )
            .collect()
    }

    pub(super) fn package(&self, package: &serde_json::Value) -> Result<Vec<(String, String)>> {
        let name = package["name"].as_str().context("package name missing")?;
        let (published, references) = self.license_paths(name, package)?;
        let mut identified = Vec::new();
        let mut supplements = Vec::new();
        let mut copyrights = BTreeSet::new();
        let mut other_legal_text = Vec::new();
        for path in published.iter().chain(references.difference(&published)) {
            let text = fs::read_to_string(path)
                .with_context(|| format!("reading license for {name}"))?
                .replace("\r\n", "\n");
            let basename = path
                .file_name()
                .context("license file name missing")?
                .to_string_lossy()
                .to_ascii_uppercase();
            let supplementary = basename.starts_with("NOTICE")
                || basename.starts_with("COPYRIGHT")
                || basename.starts_with("AUTHORS");
            if supplementary {
                supplements.push(text.clone());
            }
            let in_package = published.contains(path);
            let carries_notice = in_package && {
                let mut lines = text.lines().filter_map(copyright_line).peekable();
                let found = lines.peek().is_some();
                copyrights.extend(lines.map(str::to_owned));
                found
            };
            let matches = self.identify(&text);
            if in_package && !supplementary && (carries_notice || matches.is_empty()) {
                other_legal_text.push(text.clone());
            }
            for license in matches {
                identified.push((license, text.clone()));
            }
        }
        let expression = if let Some(expression) = package["license"].as_str() {
            expression.to_owned()
        } else {
            ensure!(
                identified.len() == 1,
                "license-file for {name} is missing or ambiguous"
            );
            identified[0].0.clone()
        };
        let available: Vec<_> = identified.iter().map(|(id, _)| id.clone()).collect();
        let chosen = self
            .choose(name, &expression, Some(&available))
            .or_else(|_| {
                self.choose(name, &expression, None)
                    .with_context(|| format!("{name} has no permitted license branch"))
            })?;
        let mut output = Vec::new();
        for license in chosen {
            if available.contains(&license) {
                output.extend(
                    identified
                        .iter()
                        .filter(|(id, _)| *id == license)
                        .map(|(_, text)| (license.clone(), text.clone())),
                );
            } else {
                let text = canonical(name, &license, &copyrights, package)?;
                output.push((license, text));
            }
        }
        for text in supplements {
            output.push(("copyright and notices".to_owned(), text));
        }
        for text in other_legal_text {
            if output.iter().all(|(_, kept)| *kept != text) {
                output.push(("other published legal text".to_owned(), text));
            }
        }
        Ok(output)
    }

    fn license_paths(
        &self,
        name: &str,
        package: &serde_json::Value,
    ) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
        let dir = Path::new(
            package["manifest_path"]
                .as_str()
                .context("manifest missing")?,
        )
        .parent()
        .context("manifest has no directory")?;
        let mut published = BTreeSet::new();
        collect(dir, &mut published)?;
        if let Some(path) = package["license_file"].as_str() {
            published.insert(dir.join(path));
        }
        let mut references = BTreeSet::new();
        let upstream = self.root.join("assets/licenses/upstream").join(format!(
            "{name}-{}",
            package["version"].as_str().context("version missing")?
        ));
        if upstream.is_dir() {
            collect(&upstream, &mut references)?;
        }
        if package["source"]
            .as_str()
            .is_some_and(|source| source.starts_with("git+"))
            && let Some(repository) = dir
                .ancestors()
                .skip(1)
                .find(|parent| parent.join(".git").exists())
        {
            collect(repository, &mut references)?;
        }
        Ok((published, references))
    }
}

fn copyright_line(line: &str) -> Option<&str> {
    let line = line.trim();
    let owner = [
        "Copyright (c) ",
        "Copyright (C) ",
        "Copyright © ",
        "Copyright: ",
    ]
    .into_iter()
    .find_map(|marker| line.split_once(marker).map(|(_, owner)| owner))
    .or_else(|| {
        line.strip_prefix("Copyright ")
            .filter(|owner| owner.starts_with(|c: char| c.is_ascii_digit()))
    })?;
    let owner = owner.trim();
    (!owner.is_empty()
        && !owner.contains(['[', ']'])
        && owner.split('<').skip(1).all(|part| {
            part.split_once('>')
                .is_some_and(|(contact, _)| contact.contains('@'))
        })
        && !owner.eq_ignore_ascii_case("the copyright holders"))
    .then_some(line)
}

fn canonical(
    name: &str,
    license: &str,
    copyrights: &BTreeSet<String>,
    package: &serde_json::Value,
) -> Result<String> {
    let text = spdx::license_id(license)
        .with_context(|| format!("{name} has no canonical SPDX text for {license}"))?
        .text();
    let attribution = if copyrights.is_empty() {
        let repository = package["repository"]
            .as_str()
            .filter(|url| !url.trim().is_empty())
            .with_context(|| format!("{name} does not declare a repository for attribution"))?;
        let authors = package["authors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|author| !author.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let authors = if authors.is_empty() {
            format!("The {name} authors (not named in published Cargo.toml)")
        } else {
            format!("{authors} (from published Cargo.toml)")
        };
        format!(
            "No copyright notice found in the published crate's license or notice files.\nAttribution: {authors}\nRepository: {repository}"
        )
    } else {
        copyrights
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(format!(
        "Selected {license} from the crate's declared license expression; canonical SPDX text (selected text not identified in package).\n{attribution}\n\n{text}"
    ))
}

fn collect(dir: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
        if entry.file_type()?.is_file()
            && [
                "LICENSE",
                "LICENCE",
                "COPYING",
                "NOTICE",
                "COPYRIGHT",
                "AUTHORS",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            files.insert(entry.path());
        }
        if entry.file_type()?.is_dir()
            && ["LICENSES", "LICENCES"].contains(&name.trim_start_matches('.'))
        {
            for child in fs::read_dir(entry.path())? {
                let child = child?;
                if child.file_type()?.is_file() {
                    files.insert(child.path());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_selection_obeys_or_and_named_exceptions_and_asset_decisions() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = Policy::read(root).unwrap();
        let available = ["MIT", "Apache-2.0", "BSD-2-Clause", "0BSD"].map(str::to_owned);
        let choose = |name, expression| policy.choose(name, expression, Some(&available));
        assert_eq!(choose("dual", "MIT OR Apache-2.0").unwrap(), vec!["MIT"]);
        assert_eq!(
            choose("both", "MIT AND BSD-2-Clause").unwrap(),
            vec!["MIT", "BSD-2-Clause"]
        );
        assert_eq!(choose("enum-iterator", "0BSD").unwrap(), vec!["0BSD"]);
        assert!(choose("other", "0BSD").is_err());
        assert!(choose("other", "MIT AND GPL-3.0-only").is_err());
        assert!(choose("other", "").is_err());
        assert!(choose("other", "MIT OR").is_err());
        assert!(policy.asset("parakeet", "CC-BY-4.0").is_ok());
        assert!(policy.asset("unapproved-model", "CC-BY-4.0").is_err());
        assert!(
            policy
                .asset("voice-native-runtime", "MIT AND BSL-1.0 AND MPL-2.0")
                .is_ok()
        );
        assert!(policy.asset("other", "MIT AND OFL-1.1").is_err());
    }

    #[test]
    fn a_license_file_only_package_preserves_its_text_and_notice_and_missing_text_fails() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = Policy::read(root).unwrap();
        let dir = crate::scratch::dir("license-file");
        let handwritten = fs::read_to_string(root.join("THIRD-PARTY-NOTICES.md"))
            .unwrap()
            .replace("\r\n", "\n");
        let start = handwritten.find("MIT License\n").unwrap();
        let end = start + handwritten[start..].find("SOFTWARE.\n").unwrap() + "SOFTWARE.\n".len();
        let text = &handwritten[start..end];
        fs::write(dir.join("terms.txt"), text).unwrap();
        fs::write(dir.join("NOTICE"), "Copyright fixture contributor\n").unwrap();
        let mut package = serde_json::json!({
            "name":"fixture", "version":"1.0.0", "manifest_path":dir.join("Cargo.toml"),
            "license":null, "license_file":"terms.txt"
        });
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![
                ("MIT".to_owned(), text.to_owned()),
                (
                    "copyright and notices".to_owned(),
                    "Copyright fixture contributor\n".to_owned()
                )
            ]
        );
        package["license"] = serde_json::json!("Apache-2.0 OR MIT");
        let bundled = dir.join(".licenses");
        fs::create_dir(&bundled).unwrap();
        let inherited = format!("Copyright (c) 2026 Inherited Publisher\n{text}");
        fs::write(bundled.join("Inherited-MIT"), &inherited).unwrap();
        assert_eq!(
            policy
                .package(&package)
                .unwrap()
                .into_iter()
                .filter_map(|(license, body)| (license == "MIT").then_some(body))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([text.to_owned(), inherited])
        );
        fs::remove_file(dir.join("terms.txt")).unwrap();
        assert!(policy.package(&package).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallback_uses_published_copyright_or_clearly_labeled_manifest_attribution() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = Policy::read(root).unwrap();
        let dir = crate::scratch::dir("canonical-license");
        let mut package = serde_json::json!({
            "name":"fixture", "version":"1.0.0", "manifest_path":dir.join("Cargo.toml"),
            "license":"CC0-1.0 OR MIT-0 OR Apache-2.0",
            "authors":["Ada Example <ada@example.invalid>", "Ben Example"],
            "repository":"https://example.invalid/fixture"
        });
        fs::write(
            dir.join("LICENSE"),
            spdx::license_id("CC0-1.0").unwrap().text(),
        )
        .unwrap();
        let expected = |attribution| {
            (
                "Apache-2.0".to_owned(),
                format!(
                    "Selected Apache-2.0 from the crate's declared license expression; canonical SPDX text (selected text not identified in package).\n{attribution}\n\n{}",
                    spdx::license_id("Apache-2.0").unwrap().text()
                ),
            )
        };
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![expected(concat!(
                "No copyright notice found in the published crate's license or notice files.\n",
                "Attribution: Ada Example <ada@example.invalid>, Ben Example (from published Cargo.toml)\n",
                "Repository: https://example.invalid/fixture",
            ))]
        );
        package["authors"] = serde_json::json!([]);
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![expected(concat!(
                "No copyright notice found in the published crate's license or notice files.\n",
                "Attribution: The fixture authors (not named in published Cargo.toml)\n",
                "Repository: https://example.invalid/fixture",
            ))]
        );
        package["repository"] = serde_json::Value::Null;
        assert_eq!(
            policy.package(&package).unwrap_err().to_string(),
            "fixture does not declare a repository for attribution"
        );
        let copyright =
            "Changes are Copyright (c) 2026 Fixture Publisher <fixture@example.invalid>";
        fs::write(dir.join("COPYRIGHT"), format!("{copyright}\n")).unwrap();
        let extra = "This package includes material from the Original Fixture project.\n";
        fs::write(dir.join("LICENSE.txt"), extra).unwrap();
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![
                expected(copyright),
                ("copyright and notices".to_owned(), format!("{copyright}\n")),
                ("other published legal text".to_owned(), extra.to_owned(),)
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
