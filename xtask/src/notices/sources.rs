mod scoped;

use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};

use super::licenses::copyright_line;

const PREFIX_LIMIT: u64 = 64 * 1024;
const EXCLUDED: &[(&str, &str, &str)] = &[
    (
        "markup5ever_rcdom 0.3.0",
        "html5lib-tests",
        "HTML conformance fixtures",
    ),
    (
        "markup5ever_rcdom 0.3.0",
        "xml5lib-tests",
        "XML conformance fixtures",
    ),
    (
        "qoi 0.4.1",
        "ext/qoi",
        "standalone C tools; no native build target",
    ),
    (
        "xn 0.2.13",
        "third_party/kleidiai",
        "Arm kernels not built for x86-64 Windows",
    ),
    (
        "tracing-core 0.1.36",
        "src/spin",
        "vendored spin implementation is disabled with std",
    ),
    (
        "esaxx-rs 0.1.10",
        "src/esaxx.cpp",
        "use only its scoped grants, not the commented-out implementation",
    ),
];

fn shipped(packages: &[serde_json::Value], id: &str) -> bool {
    id.split_once(' ').is_some_and(|(name, version)| {
        packages
            .iter()
            .any(|package| package["name"] == name && package["version"] == version)
    })
}

pub(super) fn check_exclusions(packages: &[serde_json::Value]) -> Result<()> {
    for &(id, path, reason) in EXCLUDED {
        ensure!(
            !reason.is_empty(),
            "notice exclusion has no reason: {id} {path}"
        );
        ensure!(
            shipped(packages, id),
            "notice exclusion matches no shipped crate: {id} {path} ({reason})"
        );
    }
    for &(id, path, _, _) in scoped::SCOPED {
        ensure!(
            shipped(packages, id),
            "reviewed notice input matches no shipped crate: {id} {path}"
        );
    }
    Ok(())
}

pub(super) fn notices(
    dir: &Path,
    package: &str,
    targets: &serde_json::Value,
) -> Result<BTreeSet<(String, String)>> {
    let native = targets.as_array().is_some_and(|targets| {
        targets
            .iter()
            .any(|target| target["kind"][0] == "custom-build")
    });
    let mut output = BTreeSet::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(parent) = pending.pop() {
        for entry in fs::read_dir(&parent)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            let relative = path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_ascii_lowercase();
            if EXCLUDED
                .iter()
                .any(|(id, excluded, _)| *id == package && *excluded == relative)
            {
                continue;
            }
            if ty.is_dir() {
                if !matches!(
                    name.as_str(),
                    ".git" | "target" | "tests" | "testdata" | "fixtures" | "benches" | "examples"
                ) {
                    pending.push(path);
                }
                continue;
            }
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default();
            let source =
                extension.eq_ignore_ascii_case("rs") || (native && native_source(extension));
            let legal = !source
                && parent != dir
                && (legal_file(&path, &name) || parent.file_name().is_some_and(license_dir));
            if !source && !legal {
                continue;
            }
            ensure!(
                ty.is_file(),
                "unreadable legal input for {package}: {relative}"
            );
            if legal {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("reading legal input for {package}: {relative}"))?
                    .replace("\r\n", "\n");
                ensure!(
                    !text.trim().is_empty(),
                    "empty legal input for {package}: {relative}"
                );
                output.insert(("component license and notices".to_owned(), text));
            } else if relative != "build.rs" {
                for text in source_header(&path)
                    .with_context(|| format!("reading source notice for {package}: {relative}"))?
                {
                    output.insert(("published source notices".to_owned(), text));
                }
            }
        }
    }
    output.extend(
        scoped::read(dir, package)?
            .into_iter()
            .map(|text| ("reviewed source notices".to_owned(), text)),
    );
    Ok(output)
}

pub(super) fn license_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_string_lossy()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str(),
        "licenses" | "licences"
    )
}

fn legal_file(path: &Path, name: &str) -> bool {
    let boundary = |suffix: &str| suffix.is_empty() || suffix.starts_with(['-', '_', '.']);
    ([
        "license",
        "licence",
        "copying",
        "notice",
        "notices",
        "thirdpartynotices",
        "third-party-notices",
        "third_party_notices",
        "authors",
        "copyright",
        "credits",
        "patents",
    ]
    .iter()
    .any(|prefix| name.strip_prefix(prefix).is_some_and(boundary))
        || ["-license", "_license", "-licence", "_licence"]
            .iter()
            .any(|marker| {
                name.split_once(marker)
                    .is_some_and(|(prefix, suffix)| !prefix.is_empty() && boundary(suffix))
            }))
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_none_or(|ext| {
                !matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "svg" | "png" | "jpg" | "gif" | "ico"
                )
            })
}

fn native_source(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "inc" | "inl" | "m" | "mm" | "s" | "asm"
    )
}

fn source_header(path: &Path) -> Result<Vec<String>> {
    const BLOCK_COMMENT: &str = "/*";
    const LINE_COMMENTS: [&str; 6] = ["//!", "///", "//", "# ", "; ", "@ "];

    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(PREFIX_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    let bounded = u64::try_from(bytes.len())? > PREFIX_LIMIT;
    let mut source = std::str::from_utf8(&bytes)
        .or_else(|error| {
            if bounded && error.error_len().is_none() {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
            } else {
                Err(error)
            }
        })?
        .trim_start_matches('\u{feff}');
    let mut notices = Vec::new();
    loop {
        source = source.trim_start();
        let body = if let Some(block) = source.strip_prefix(BLOCK_COMMENT) {
            let (body, rest) = block
                .split_once("*/")
                .context("unterminated leading comment")?;
            source = rest;
            body.lines()
                .map(|line| line.trim_start().trim_start_matches('*').trim_start())
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(marker) = LINE_COMMENTS
            .into_iter()
            .find(|marker| source.starts_with(marker))
        {
            let mut body = String::new();
            while let Some(line) = source.strip_prefix(marker) {
                if marker == "//" && line.starts_with(['!', '/']) {
                    break;
                }
                let (line, rest) = line.split_once('\n').unwrap_or((line, ""));
                body.push_str(line.trim_start_matches(['/', '!']).trim());
                body.push('\n');
                source = rest;
            }
            body
        } else {
            ensure!(
                !bounded
                    || !LINE_COMMENTS
                        .into_iter()
                        .chain([BLOCK_COMMENT])
                        .any(|opener| opener.starts_with(source)),
                "source header exceeds scan limit"
            );
            return Ok(notices);
        };
        if body.lines().any(|line| copyright_line(line).is_some())
            || [
                "Permission is hereby granted",
                "Permission to use, copy",
                "SPDX-License-Identifier:",
                "Licensed under the",
                "Licensed to the Apache Software Foundation",
                "This Source Code Form is subject to the terms of the Mozilla Public",
                "governed by a BSD-style license",
            ]
            .iter()
            .any(|marker| body.contains(marker))
        {
            notices.push(body.trim().to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_comment_scan_keeps_legal_text_without_unrelated_comments() {
        let dir = crate::scratch::dir("notice-headers");
        let file = dir.join("library.rs");
        let apache = "Licensed to the Apache Software Foundation (ASF) under one or more contributor license agreements.";
        let mpl =
            "This Source Code Form is subject to the terms of the Mozilla Public\nLicense, v. 2.0.";
        let moxcms = "// Copyright 2024 (c) the Radzivon Bartoshyk. All rights reserved.\n//\n// Use of this source code is governed by a BSD-style\n// license that can be found in the LICENSE file.";
        let nested = moxcms.replace('\n', "\n * ");
        fs::write(&file, format!(
            "//! Unrelated docs.\n\n// Copyright 2026 Fixture Publisher\n\n/* Permission is hereby granted to copy this fixture. */\n\n// {apache}\n\n/* {mpl} */\n\n/*\n * {nested}\n */\n\nuse std::fmt;"
        )).unwrap();
        assert_eq!(
            source_header(&file).unwrap(),
            [
                "Copyright 2026 Fixture Publisher",
                "Permission is hereby granted to copy this fixture.",
                apache,
                mpl,
                moxcms,
            ]
        );
        fs::write(&file, "").unwrap();
        assert_eq!(source_header(&file).unwrap(), Vec::<String>::new());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_headers_require_a_build_target_and_reasons_exclude_standalone_tools() {
        let dir = crate::scratch::dir("native-notice-scope");
        fs::create_dir_all(dir.join("ext/qoi")).unwrap();
        fs::write(
            dir.join("ext/qoi/qoiconv.c"),
            "// Copyright 2026 Fixture Tool\n",
        )
        .unwrap();
        fs::write(dir.join("native.c"), "// Copyright 2026 Fixture Library\n").unwrap();
        let rust = serde_json::json!([{ "kind": ["lib"] }]);
        let native = serde_json::json!([{ "kind": ["custom-build"] }]);
        assert_eq!(
            notices(&dir, "fixture 1.0.0", &rust).unwrap(),
            BTreeSet::new()
        );
        assert_eq!(
            notices(&dir, "qoi 0.4.1", &native).unwrap(),
            BTreeSet::from([(
                "published source notices".to_owned(),
                "Copyright 2026 Fixture Library".to_owned()
            )])
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_header_accepts_64_kib_and_refuses_larger_or_truncated_headers() {
        let dir = crate::scratch::dir("notice-header-limit");
        let file = dir.join("library.rs");
        let prefix = "// Copyright 2026 Fixture Publisher\n// ";
        let padding = "x".repeat(65_536 - prefix.len());
        let exact = format!("{prefix}{padding}");
        fs::write(&file, &exact).unwrap();
        assert_eq!(
            source_header(&file).unwrap(),
            [format!("Copyright 2026 Fixture Publisher\n{padding}")]
        );
        for extra in ['x', 'é'] {
            fs::write(&file, format!("{exact}{extra}")).unwrap();
            assert_eq!(
                source_header(&file).unwrap_err().to_string(),
                "source header exceeds scan limit"
            );
        }
        let padding = "x".repeat(65_536 - "/* */\n".len());
        for notice in [
            "// Copyright 2026 Fixture Publisher",
            "/* Copyright 2026 Fixture Publisher */",
        ] {
            fs::write(&file, format!("/* {padding}*/\n{notice}")).unwrap();
            assert_eq!(
                source_header(&file).unwrap_err().to_string(),
                "source header exceeds scan limit"
            );
        }
        fs::write(&file, "/* Copyright 2026 Fixture Publisher").unwrap();
        assert_eq!(
            source_header(&file).unwrap_err().to_string(),
            "unterminated leading comment"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nested_legal_files_are_kept_and_exclusions_match_only_an_exact_release() {
        let dir = crate::scratch::dir("nested-notice-coverage");
        let nested = dir.join("src/component");
        fs::create_dir_all(&nested).unwrap();
        let mut expected = BTreeSet::new();
        for name in [
            "LICENSE-MIT",
            "nOtIcEs",
            "MIT-LICENSE.txt",
            "ThirdPartyNotices.txt",
            "credits",
            "PaTeNtS",
        ] {
            let text = format!("Terms from {name}\n");
            fs::write(nested.join(name), &text).unwrap();
            expected.insert(("component license and notices".to_owned(), text));
        }
        let excluded = dir.join("html5lib-tests");
        fs::create_dir(&excluded).unwrap();
        let excluded_text = "Fixture license in a version-specific excluded folder.";
        fs::write(excluded.join("LICENSE"), excluded_text).unwrap();
        let targets = serde_json::Value::Null;
        assert_eq!(
            notices(&dir, "markup5ever_rcdom 0.3.0", &targets).unwrap(),
            expected
        );
        expected.insert((
            "component license and notices".to_owned(),
            excluded_text.to_owned(),
        ));
        assert_eq!(
            notices(&dir, "markup5ever_rcdom 0.3.1", &targets).unwrap(),
            expected
        );
        fs::write(nested.join("NOTICE"), " \n").unwrap();
        assert_eq!(
            notices(&dir, "markup5ever_rcdom 0.3.0", &targets)
                .unwrap_err()
                .to_string(),
            "empty legal input for markup5ever_rcdom 0.3.0: src/component/NOTICE"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exclusion_without_a_shipped_package_is_rejected_with_its_reason() {
        let matched = EXCLUDED
            .iter()
            .map(|rule| rule.0)
            .chain(scoped::SCOPED.iter().map(|rule| rule.0))
            .map(|id| {
                let (name, version) = id.split_once(' ').unwrap();
                serde_json::json!({"name":name,"version":version})
            })
            .collect::<Vec<_>>();
        check_exclusions(&matched).unwrap();
        let bumped = serde_json::json!({"name":"markup5ever_rcdom","version":"0.3.1"});
        assert_eq!(
            check_exclusions(&[bumped]).unwrap_err().to_string(),
            "notice exclusion matches no shipped crate: markup5ever_rcdom 0.3.0 html5lib-tests (HTML conformance fixtures)"
        );
    }
}
