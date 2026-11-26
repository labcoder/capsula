//! Ratchet the product's plain OS predicates outside platform modules.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use proc_macro2::{Delimiter, TokenStream, TokenTree};

const BASELINE: &str = "xtask/platform-cfg-baseline.txt";

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let (list, base) = match args.next().as_deref() {
        None => (false, None),
        Some("--list") => (true, None),
        Some("--base") => (false, Some(args.next().context("--base needs a commit")?)),
        Some(_) => bail!("expected check-platform-cfg [--list | --base <commit>]"),
    };
    ensure!(
        args.next().is_none(),
        "unexpected check-platform-cfg argument"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    let mut sites = BTreeMap::new();
    for dir in fs::read_dir(root.join("crates"))? {
        let source = dir?.path().join("src");
        if source.is_dir() {
            collect(root, &source, &mut sites)?;
        }
    }
    if list {
        for (site, lines) in &sites {
            for _ in lines {
                println!("{site}");
            }
        }
        return Ok(());
    }
    let baseline =
        fs::read_to_string(root.join(BASELINE)).context("reading platform cfg baseline")?;
    let expected = counts(&baseline);
    let actual: BTreeMap<_, _> = sites
        .iter()
        .map(|(site, lines)| (site.as_str(), lines.len()))
        .collect();
    let added = excess(&actual, &expected);
    ensure!(
        added.is_empty(),
        "new plain OS cfg outside platform modules:\n{}",
        added
            .iter()
            .map(|(site, extra)| format!("{site} (+{extra}; matching lines {:?})", sites[*site]))
            .collect::<Vec<_>>()
            .join("\n")
    );
    ensure!(
        excess(&expected, &actual).is_empty(),
        "remove stale entries from {BASELINE}; see cargo xtask check-platform-cfg --list"
    );
    if let Some(base) = base {
        let tree = git(root, &["ls-tree", &base, "--", BASELINE])?;
        if !tree.is_empty() {
            let previous = git(root, &["show", &format!("{base}:{BASELINE}")])?;
            let grown = excess(&expected, &counts(&previous));
            ensure!(grown.is_empty(), "{BASELINE} can only shrink: {grown:?}");
        }
    }
    let total: usize = actual.values().sum();
    println!("plain OS cfg: {total} existing sites outside platform modules");
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git output must be UTF-8")
}

fn counts(baseline: &str) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for line in baseline.lines().filter(|line| !line.is_empty()) {
        *counts.entry(line).or_default() += 1;
    }
    counts
}

fn excess<'a>(
    current: &BTreeMap<&'a str, usize>,
    previous: &BTreeMap<&str, usize>,
) -> Vec<(&'a str, usize)> {
    current
        .iter()
        .filter_map(|(&site, &count)| {
            let extra = count.saturating_sub(previous.get(site).copied().unwrap_or_default());
            (extra != 0).then_some((site, extra))
        })
        .collect()
}

fn collect(root: &Path, folder: &Path, sites: &mut BTreeMap<String, Vec<usize>>) -> Result<()> {
    for entry in fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "platform" | "platform.rs" | "tests" | "tests.rs" | "fixtures"
        ) || name.ends_with("_tests")
            || name.ends_with("_tests.rs")
            || name.ends_with("_fixture.rs")
        {
            continue;
        }
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, sites)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            let relative: PathBuf = path.strip_prefix(root)?.to_owned();
            let relative = relative.to_string_lossy().replace('\\', "/");
            let source =
                fs::read_to_string(&path).with_context(|| format!("reading {relative}"))?;
            let tokens: TokenStream = source
                .parse()
                .map_err(|error| anyhow::anyhow!("tokenizing {relative}: {error}"))?;
            scan(tokens, &mut |line, signature| {
                sites
                    .entry(format!("{relative}\t{signature}"))
                    .or_default()
                    .push(line);
            });
        }
    }
    Ok(())
}

fn scan(stream: TokenStream, found: &mut impl FnMut(usize, String)) {
    let tokens: Vec<_> = stream.into_iter().collect();
    let mut cursor = tokens.as_slice();
    while let [first, rest @ ..] = cursor {
        if let TokenTree::Punct(hash) = first
            && hash.as_char() == '#'
        {
            let (inner, tail) = match rest {
                [TokenTree::Punct(bang), tail @ ..] if bang.as_char() == '!' => (true, tail),
                _ => (false, rest),
            };
            if let [TokenTree::Group(group), tail @ ..] = tail
                && group.delimiter() == Delimiter::Bracket
            {
                let attribute: Vec<_> = group.stream().into_iter().collect();
                if let [TokenTree::Ident(name), TokenTree::Group(body)] = attribute.as_slice()
                    && body.delimiter() == Delimiter::Parenthesis
                {
                    let is_cfg = name == "cfg";
                    if inner && is_cfg && body.stream().to_string() == "test" {
                        return;
                    }
                    if !inner
                        && is_cfg
                        && body.stream().to_string() == "test"
                        && let Some(tail) = after_test_module(tail)
                    {
                        cursor = tail;
                        continue;
                    }
                    if is_cfg || name == "cfg_attr" {
                        let predicate: TokenStream = body
                            .stream()
                            .into_iter()
                            .take_while(
                                |token| !matches!(token, TokenTree::Punct(p) if p.as_char() == ','),
                            )
                            .collect();
                        if plain_os(&predicate) {
                            let prefix = if inner { "#!" } else { "#" };
                            found(
                                hash.span().start().line,
                                format!("{prefix}[{name}({predicate})]"),
                            );
                        }
                    }
                }
                cursor = tail;
                continue;
            }
        }
        if let [
            TokenTree::Ident(name),
            TokenTree::Punct(bang),
            TokenTree::Group(body),
            ..,
        ] = cursor
            && name == "cfg"
            && bang.as_char() == '!'
            && plain_os(&body.stream())
        {
            found(name.span().start().line, format!("cfg!({})", body.stream()));
        }
        if let TokenTree::Group(group) = first {
            scan(group.stream(), found);
        }
        cursor = rest;
    }
}

fn after_test_module(tokens: &[TokenTree]) -> Option<&[TokenTree]> {
    let [
        TokenTree::Ident(module),
        TokenTree::Ident(_),
        TokenTree::Group(body),
        tail @ ..,
    ] = tokens
    else {
        return None;
    };
    (module == "mod" && body.delimiter() == Delimiter::Brace).then_some(tail)
}

fn plain_os(predicate: &TokenStream) -> bool {
    let mut os = false;
    only_os(predicate.clone(), &mut os) && os
}

fn only_os(predicate: TokenStream, os: &mut bool) -> bool {
    predicate.into_iter().all(|token| match token {
        TokenTree::Ident(name) => match name.to_string().trim_start_matches("r#") {
            "all" | "any" | "not" => true,
            "windows" | "unix" | "target_os" | "target_family" => {
                *os = true;
                true
            }
            _ => false,
        },
        TokenTree::Group(group) => {
            group.delimiter() == Delimiter::Parenthesis && only_os(group.stream(), os)
        }
        TokenTree::Literal(_) => true,
        TokenTree::Punct(punct) => matches!(punct.as_char(), ',' | '='),
    })
}

#[cfg(test)]
mod tests;
