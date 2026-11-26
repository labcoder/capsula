//! Ratchet the product's plain OS predicates outside platform modules.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};

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
            "platform" | "platform.rs" | "tests" | "tests.rs" | "test_support.rs" | "fixtures"
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
        if attribute(cursor).is_some() {
            let mut sites = Vec::new();
            let mut test_item = false;
            while let Some((inner, body, tail)) = attribute(cursor) {
                let fields: Vec<_> = body.stream().into_iter().collect();
                if test_attribute(&fields) {
                    if inner {
                        return;
                    }
                    test_item = true;
                }
                let prefix = if inner { "#!" } else { "#" };
                attribute_sites(&fields, "", &mut |signature| {
                    sites.push((
                        body.span_open().start().line,
                        format!("{prefix}[{signature}]"),
                    ));
                });
                cursor = tail;
            }
            if test_item {
                cursor = cursor
                    .iter()
                    .position(|token| punct(token, ';') || punct(token, ',')
                        || matches!(token, TokenTree::Group(body) if body.delimiter() == Delimiter::Brace))
                    .map_or(&[], |end| &cursor[end + 1..]);
            } else {
                for (line, signature) in sites {
                    found(line, signature);
                }
            }
            continue;
        }
        if let [
            TokenTree::Ident(name),
            TokenTree::Punct(bang),
            TokenTree::Group(body),
            ..,
        ] = cursor
            && bang.as_char() == '!'
        {
            if name == "cfg" {
                let predicate: Vec<_> = body.stream().into_iter().collect();
                if plain_os(&predicate) {
                    found(
                        name.span().start().line,
                        format!("cfg!({})", normalized(&predicate)),
                    );
                }
            } else if name == "cfg_select" {
                let arms: Vec<_> = body.stream().into_iter().collect();
                for end in 1..arms.len().saturating_sub(1) {
                    if punct(&arms[end], '=') && punct(&arms[end + 1], '>') {
                        let start = match &arms[..end] {
                            [.., TokenTree::Ident(_), TokenTree::Group(_)] => end - 2,
                            [.., TokenTree::Ident(_), equals, TokenTree::Literal(_)]
                                if punct(equals, '=') =>
                            {
                                end - 3
                            }
                            _ => end - 1,
                        };
                        let predicate = &arms[start..end];
                        if plain_os(predicate) {
                            found(
                                arms[start].span().start().line,
                                format!("cfg_select!({})", normalized(predicate)),
                            );
                        }
                    }
                }
            }
        }
        if let TokenTree::Group(group) = first {
            scan(group.stream(), found);
        }
        cursor = rest;
    }
}

fn punct(token: &TokenTree, value: char) -> bool {
    matches!(token, TokenTree::Punct(mark) if mark.as_char() == value)
}

fn attribute(tokens: &[TokenTree]) -> Option<(bool, &Group, &[TokenTree])> {
    let [hash, rest @ ..] = tokens else {
        return None;
    };
    if !punct(hash, '#') {
        return None;
    }
    let (inner, rest) = match rest {
        [bang, rest @ ..] if punct(bang, '!') => (true, rest),
        _ => (false, rest),
    };
    match rest {
        [TokenTree::Group(body), tail @ ..] if body.delimiter() == Delimiter::Bracket => {
            Some((inner, body, tail))
        }
        _ => None,
    }
}

fn test_attribute(fields: &[TokenTree]) -> bool {
    match fields {
        [TokenTree::Ident(name)] => name == "test",
        [TokenTree::Ident(name), TokenTree::Group(body)] if name == "cfg" => {
            product_value(&body.stream().into_iter().collect::<Vec<_>>()) == Some(false)
        }
        _ => false,
    }
}

fn product_value(predicate: &[TokenTree]) -> Option<bool> {
    match without_trailing_comma(predicate) {
        [TokenTree::Ident(name)] if name == "test" || name == "false" => Some(false),
        [TokenTree::Ident(name)] if name == "true" => Some(true),
        [TokenTree::Ident(name), equals, TokenTree::Literal(value)]
            if name == "feature"
                && punct(equals, '=')
                && value.to_string() == "\"test-support\"" =>
        {
            Some(false)
        }
        [TokenTree::Ident(name), TokenTree::Group(body)] if name == "not" => {
            product_value(&body.stream().into_iter().collect::<Vec<_>>()).map(|truth| !truth)
        }
        [TokenTree::Ident(name), TokenTree::Group(body)] if name == "all" || name == "any" => {
            let fields: Vec<_> = body.stream().into_iter().collect();
            let short = name == "any";
            let mut value = Some(!short);
            if !fields.is_empty() {
                for part in without_trailing_comma(&fields).split(|token| punct(token, ',')) {
                    match product_value(part) {
                        Some(truth) if truth == short => return Some(short),
                        None => value = None,
                        _ => {}
                    }
                }
            }
            value
        }
        _ => None,
    }
}

fn attribute_sites(fields: &[TokenTree], parent: &str, found: &mut impl FnMut(String)) {
    let [TokenTree::Ident(name), TokenTree::Group(body)] = fields else {
        return;
    };
    if body.delimiter() != Delimiter::Parenthesis || (name != "cfg" && name != "cfg_attr") {
        return;
    }
    let fields: Vec<_> = body.stream().into_iter().collect();
    let mut parts = without_trailing_comma(&fields).split(|token| punct(token, ','));
    let predicate = parts.next().unwrap_or_default();
    let site = format!("{parent}{name}({})", normalized(predicate));
    if plain_os(predicate) {
        found(site.clone());
    }
    if name == "cfg_attr" && product_value(predicate) != Some(false) {
        let parent = format!("{site} -> ");
        for child in parts {
            attribute_sites(child, &parent, found);
        }
    }
}

fn without_trailing_comma(tokens: &[TokenTree]) -> &[TokenTree] {
    match tokens {
        [rest @ .., comma] if punct(comma, ',') => rest,
        _ => tokens,
    }
}

fn normalized(tokens: &[TokenTree]) -> TokenStream {
    without_trailing_comma(tokens)
        .iter()
        .map(|token| {
            if let TokenTree::Group(group) = token {
                let children: Vec<_> = group.stream().into_iter().collect();
                TokenTree::Group(Group::new(group.delimiter(), normalized(&children)))
            } else {
                token.clone()
            }
        })
        .collect()
}

fn plain_os(predicate: &[TokenTree]) -> bool {
    let mut os = false;
    only_os(predicate, &mut os) && os
}

fn only_os(predicate: &[TokenTree], os: &mut bool) -> bool {
    predicate.iter().all(|token| match token {
        TokenTree::Ident(name) => match name.to_string().trim_start_matches("r#") {
            "all" | "any" | "not" | "true" | "false" => true,
            "windows" | "unix" | "target_os" | "target_family" => {
                *os = true;
                true
            }
            _ => false,
        },
        TokenTree::Group(group) => {
            group.delimiter() == Delimiter::Parenthesis
                && only_os(&group.stream().into_iter().collect::<Vec<_>>(), os)
        }
        TokenTree::Literal(_) => true,
        TokenTree::Punct(punct) => matches!(punct.as_char(), ',' | '='),
    })
}

#[cfg(test)]
mod tests;
