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
];

pub(super) fn notices(dir: &Path, package: &str) -> Result<BTreeSet<(String, String)>> {
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
            if ty.is_dir() {
                if !matches!(
                    name.as_str(),
                    ".git" | "target" | "tests" | "testdata" | "fixtures" | "benches" | "examples"
                ) && !EXCLUDED.iter().any(|(id, excluded, reason)| {
                    *id == package && *excluded == relative && !reason.is_empty()
                }) {
                    pending.push(path);
                }
                continue;
            }
            let source = source_file(&path);
            let legal = parent != dir
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
    [
        "license",
        "licence",
        "copying",
        "notice",
        "authors",
        "copyright",
    ]
    .iter()
    .any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['-', '_', '.']))
    }) && path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_none_or(|ext| {
            !matches!(
                ext.to_ascii_lowercase().as_str(),
                "svg" | "png" | "jpg" | "gif" | "ico"
            )
        })
}

fn source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "c"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "h"
                    | "hpp"
                    | "inc"
                    | "inl"
                    | "m"
                    | "mm"
                    | "s"
                    | "asm"
            )
        })
}

fn source_header(path: &Path) -> Result<Vec<String>> {
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
        let body = if let Some(block) = source.strip_prefix("/*") {
            let (body, rest) = block
                .split_once("*/")
                .context("unterminated leading comment")?;
            source = rest;
            body.lines()
                .map(|line| line.trim_start().trim_start_matches('*').trim_start())
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(marker) = ["//!", "///", "//", "# ", "; ", "@ "]
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
                !bounded || !source.is_empty(),
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
                "governed by a BSD-style license",
            ]
            .iter()
            .any(|marker| body.contains(marker))
        {
            notices.push(body.trim().to_owned());
        }
    }
}
