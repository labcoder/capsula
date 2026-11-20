use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use spdx::detection::scan::Scanner;
use spdx::detection::{Store, TextData};
use spdx::{Expression, Licensee, ParseMode};

use super::sources;

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
        let licenses = policy.get("licenses").context("license policy missing")?;
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
        let allow = parse(
            licenses
                .get("allow")
                .context("license allow list missing")?,
        )?;
        let mut exceptions = BTreeMap::new();
        for entry in licenses
            .get("exceptions")
            .and_then(toml::Value::as_array)
            .context("exceptions missing")?
        {
            let name = entry
                .get("crate")
                .and_then(toml::Value::as_str)
                .context("exception crate missing")?;
            let allowed = parse(entry.get("allow").context("exception allow list missing")?)?;
            ensure!(
                exceptions.insert(name.to_owned(), allowed).is_none(),
                "duplicate license exception"
            );
        }
        let confidence: f32 = licenses
            .get("confidence-threshold")
            .context("license confidence threshold missing")?
            .to_string()
            .parse()
            .context("invalid license confidence threshold")?;
        ensure!(
            confidence.is_finite() && (0.0..=1.0).contains(&confidence),
            "invalid license confidence threshold"
        );
        let mut texts = Store::new();
        for allowed in allow.iter().chain(exceptions.values().flatten()) {
            let name = allowed.to_string();
            let text = spdx::license_id(&name)
                .with_context(|| format!("no canonical SPDX text for policy license {name}"))?
                .text();
            texts.add_license(name, TextData::new(text));
        }
        ensure!(!texts.is_empty(), "no SPDX licenses in policy");
        Ok(Self {
            root: root.to_owned(),
            allow,
            exceptions,
            confidence,
            texts,
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
        let version = package["version"].as_str().context("version missing")?;
        let dir = Path::new(
            package["manifest_path"]
                .as_str()
                .context("manifest missing")?,
        )
        .parent()
        .context("manifest has no directory")?;
        let (published, references) =
            self.license_paths(dir, &format!("{name}-{version}"), package)?;
        let mut identified = Vec::new();
        let mut supplements = Vec::new();
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
            let matches = self.identify(&text);
            if in_package && !supplementary && (has_copyright(&text) || matches.is_empty()) {
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
                let text = canonical(name, &license)?;
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
        for (kind, text) in
            sources::notices(dir, &format!("{name} {version}"), &package["targets"])?
        {
            if output.iter().all(|(_, kept)| *kept != text) {
                output.push((kind, text));
            }
        }
        if output.iter().any(|(license, _)| license == "MPL-2.0") {
            output.push((
                "MPL-2.0 source".to_owned(),
                mpl_source(name, version, &package["source"])?,
            ));
        }
        if !output.iter().any(|(_, text)| has_copyright(text)) {
            output.push((
                "manifest attribution".to_owned(),
                attribution(name, package)?,
            ));
        }
        Ok(output)
    }

    fn license_paths(
        &self,
        dir: &Path,
        package_dir: &str,
        package: &serde_json::Value,
    ) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
        let mut published = BTreeSet::new();
        collect(dir, &mut published)?;
        if let Some(path) = package["license_file"].as_str() {
            published.insert(dir.join(path));
        }
        let mut references = BTreeSet::new();
        let upstream = self.root.join("assets/licenses/upstream").join(package_dir);
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

pub(super) fn copyright_line(line: &str) -> Option<&str> {
    let line = line.trim();
    let line = line.strip_prefix("//").unwrap_or(line).trim_start();
    let owner = [
        "Copyright (c) ",
        "Copyright (C) ",
        "Copyright © ",
        "Copyright: ",
        "SPDX-FileCopyrightText: ",
    ]
    .into_iter()
    .find_map(|marker| line.split_once(marker).map(|(_, owner)| owner))
    .or_else(|| {
        ["©", "(c) ", "(C) "].into_iter().find_map(|marker| {
            line.strip_prefix(marker).filter(|owner| {
                let owner = owner.trim_start();
                marker == "©"
                    || owner.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit())
                    || owner.strip_suffix(" developers").is_some_and(|project| {
                        !project.is_empty() && !project.contains(char::is_whitespace)
                    })
            })
        })
    })
    .or_else(|| {
        let (prefix, owner) = line.split_once(char::is_whitespace)?;
        prefix.eq_ignore_ascii_case("copyright").then_some(owner)
    })?;
    let owner = owner.trim();
    let phrase = owner.to_ascii_lowercase();
    (!owner.is_empty()
        && !owner.contains(['[', ']'])
        && owner.split('<').skip(1).all(|part| {
            part.split_once('>')
                .is_some_and(|(contact, _)| contact.contains('@'))
        })
        && ![
            "and related rights",
            "holders",
            "holder ",
            "notice",
            "license",
            "protection under ",
            "owner of ",
            "owners of ",
            "law of ",
            "laws of ",
            "the copyright holders",
            "you must",
        ]
        .iter()
        .any(|prose| phrase.starts_with(prose)))
    .then_some(line)
}

fn has_copyright(text: &str) -> bool {
    text.lines().any(|line| copyright_line(line).is_some())
}

fn mpl_source(name: &str, version: &str, source: &serde_json::Value) -> Result<String> {
    ensure!(
        source.as_str() == Some("registry+https://github.com/rust-lang/crates.io-index"),
        "{name} {version} needs an exact source offer for MPL-2.0"
    );
    Ok(format!(
        "Source Code Form for {name} {version}: https://static.crates.io/crates/{name}/{name}-{version}.crate"
    ))
}

fn canonical(name: &str, license: &str) -> Result<String> {
    let text = spdx::license_id(license)
        .with_context(|| format!("{name} has no canonical SPDX text for {license}"))?
        .text();
    Ok(format!(
        "Selected {license} from the crate's declared license expression; canonical SPDX text (selected text not identified in package).\n\n{text}"
    ))
}

fn attribution(name: &str, package: &serde_json::Value) -> Result<String> {
    let repository = package["repository"]
        .as_str()
        .filter(|url| !url.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let locked = package["source"].as_str()?.strip_prefix("git+")?;
            let (url, _) = locked.rsplit_once('#')?;
            let url = url.split_once('?').map_or(url, |(url, _)| url);
            Some(format!("{url} (from the locked Cargo git source)"))
        });
    let authors = package["authors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|author| !author.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    ensure!(
        !authors.is_empty() || repository.is_some(),
        "{name} does not declare authors or a repository for attribution"
    );
    let authors = if authors.is_empty() {
        format!("The {name} authors (not named in published Cargo.toml)")
    } else {
        format!("{authors} (from published Cargo.toml)")
    };
    let repository = repository
        .as_deref()
        .unwrap_or("not declared in published Cargo.toml");
    Ok(format!(
        "No copyright notice found in the legal files or source comments inspected.\nAttribution: {authors}\nRepository: {repository}"
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
        if entry.file_type()?.is_dir() && sources::license_dir(&entry.file_name()) {
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
    fn holder_lines_include_bare_copyright_symbols_but_not_license_prose() {
        for line in [
            "Copyright The Foo Developers",
            "COPYRIGHT 2019 Fixture Foundation",
            "© Fixture Publisher",
            "(c) 2026 Fixture Publisher",
            "(C) zune-image developers",
            "SPDX-FileCopyrightText: 2026 Fixture Publisher",
        ] {
            assert_eq!(copyright_line(line), Some(line));
        }
        for line in [
            "Copyright and Related Rights. A Work made available under CC0",
            "copyright protection under copyright law of a country",
            "Copyright owner of any part of the Work",
            "(c) You must retain, in the Source form of any Derivative Works",
            "(c) compound (-1, n) is +0 for n > 0",
            "(c) send a request for early data",
            "Copyright (c) <year> <copyright holders>",
        ] {
            assert_eq!(copyright_line(line), None);
        }
    }

    #[test]
    fn copyright_line_strips_leading_comment_marker() {
        assert_eq!(
            copyright_line("  // Copyright (c) 2018 Fixture"),
            Some("Copyright (c) 2018 Fixture")
        );
    }

    #[test]
    fn mpl_source_offers_the_exact_crates_io_archive() {
        let source = serde_json::json!("registry+https://github.com/rust-lang/crates.io-index");
        assert_eq!(
            mpl_source("option-ext", "0.2.0", &source).unwrap(),
            "Source Code Form for option-ext 0.2.0: https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate"
        );
    }

    #[test]
    fn mpl_source_refuses_non_crates_io_sources() {
        for source in [
            serde_json::json!("git+https://example.invalid/fixture"),
            serde_json::Value::Null,
        ] {
            assert_eq!(
                mpl_source("fixture", "1.0.0", &source)
                    .unwrap_err()
                    .to_string(),
                "fixture 1.0.0 needs an exact source offer for MPL-2.0"
            );
        }
    }

    #[test]
    fn missing_policy_keys_return_errors() {
        let dir = crate::scratch::dir("notice-policy");
        let path = dir.join("deny.toml");
        for (document, message) in [
            ("", "license policy missing"),
            ("[licenses]\n", "license allow list missing"),
            ("[licenses]\nallow = []\n", "exceptions missing"),
            (
                "[licenses]\nallow = []\nexceptions = []\n",
                "license confidence threshold missing",
            ),
            (
                "[licenses]\nallow = []\nexceptions = [{}]\n",
                "exception crate missing",
            ),
            (
                "[licenses]\nallow = []\nexceptions = [{ crate = 'fixture' }]\n",
                "exception allow list missing",
            ),
        ] {
            fs::write(&path, document).unwrap();
            let error = Policy::read(&dir).err().expect("missing key must fail");
            assert_eq!(error.to_string(), message);
        }
    }

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
        let cc0 = spdx::license_id("CC0-1.0").unwrap().text();
        fs::write(dir.join("LICENSE"), cc0).unwrap();
        let other = |text: &str| ("other published legal text".to_owned(), text.to_owned());
        let expected = || {
            (
                "Apache-2.0".to_owned(),
                format!(
                    "Selected Apache-2.0 from the crate's declared license expression; canonical SPDX text (selected text not identified in package).\n\n{}",
                    spdx::license_id("Apache-2.0").unwrap().text()
                ),
            )
        };
        let manifest_credit = |name| {
            (
                "manifest attribution".to_owned(),
                format!(
                    "No copyright notice found in the legal files or source comments inspected.\nAttribution: {name}\nRepository: https://example.invalid/fixture"
                ),
            )
        };
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![
                expected(),
                manifest_credit(
                    "Ada Example <ada@example.invalid>, Ben Example (from published Cargo.toml)"
                )
            ]
        );
        package["authors"] = serde_json::json!([]);
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![
                expected(),
                manifest_credit("The fixture authors (not named in published Cargo.toml)")
            ]
        );
        package["repository"] = serde_json::Value::Null;
        assert_eq!(
            policy.package(&package).unwrap_err().to_string(),
            "fixture does not declare authors or a repository for attribution"
        );
        let copyright =
            "Changes are Copyright (c) 2026 Fixture Publisher <fixture@example.invalid>";
        fs::write(dir.join("COPYRIGHT"), format!("{copyright}\n")).unwrap();
        let extra = "This package includes material from the Original Fixture project.\n";
        fs::write(dir.join("LICENSE.txt"), extra).unwrap();
        assert_eq!(
            policy.package(&package).unwrap(),
            vec![
                expected(),
                ("copyright and notices".to_owned(), format!("{copyright}\n")),
                other(extra)
            ]
        );
    }

    #[test]
    fn package_keeps_the_unicode_component_and_both_isc_source_grants() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let policy = Policy::read(root).unwrap();
        let dir = crate::scratch::dir("scoped-license-inputs");
        let unicode = "Copyright © 1991-2018 Unicode, Inc. All rights reserved.\n";
        let permission = "Permission to use, copy, modify, and/or distribute this software for any\npurpose with or without fee is hereby granted, provided that the above\ncopyright notice and this permission notice appear in all copies.";
        let isc = [
            (
                "src/subject_name/mod.rs",
                "Copyright 2022 Rafael Fernández López.",
            ),
            ("src/crl/mod.rs", "Copyright 2023 Daniel McCarney."),
        ];
        for (file, credit) in isc {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let source = format!("{credit}\n\n{permission}");
            let header = source.replace('\n', "\n// ");
            fs::write(path, format!("// {header}\n\npub struct Fixture;")).unwrap();
        }
        fs::create_dir_all(dir.join("src/unicode_tables")).unwrap();
        fs::write(dir.join("src/unicode_tables/LICENSE-UNICODE"), unicode).unwrap();
        fs::create_dir(dir.join("tests")).unwrap();
        fs::write(dir.join("tests/LICENSE"), "Unrelated fixture license.").unwrap();
        let package = serde_json::json!({
            "name":"fixture", "version":"1.0.0", "manifest_path":dir.join("Cargo.toml"), "license":"ISC"
        });
        let output = policy.package(&package).unwrap();
        assert_eq!(
            output
                .iter()
                .filter(|(label, _)| label == "component license and notices")
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            vec![unicode]
        );
        assert_eq!(
            output
                .iter()
                .filter(|(label, _)| label == "published source notices")
                .map(|(_, text)| text.clone())
                .collect::<BTreeSet<_>>(),
            isc.into_iter()
                .map(|(_, credit)| format!("{credit}\n\n{permission}"))
                .collect::<BTreeSet<_>>()
        );
    }
}
