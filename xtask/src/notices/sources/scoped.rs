use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};

use super::PREFIX_LIMIT;

pub(super) const SCOPED: &[(&str, &str, &str, &str)] = &[
    (
        "ring 0.17.14",
        "crypto/cipher/asm/chacha20_poly1305_x86_64.pl",
        "# Copyright (c) 2015, CloudFlare Ltd.",
        "# limitations under the License.",
    ),
    (
        "ring 0.17.14",
        "crypto/chacha/asm/chacha-x86_64.pl",
        "# Copyright 2016 The OpenSSL Project Authors. All Rights Reserved.",
        "# limitations under the License.",
    ),
    (
        "gpui-pre 0.3.10",
        "src/platform.rs",
        "// Adapted from https://github.com/microsoft/terminal/blob/",
        "// Licensed under the MIT license.",
    ),
    (
        "icu_provider 2.4.0",
        "src/marker.rs",
        "// This code is adapted from https://github.com/rust-lang/rustc-hash,",
        "// except according to those terms.",
    ),
    (
        "image 0.25.10",
        "src/lib.rs",
        "// The inspiration for the macro and implementation is from",
        "// Copyright (c) 2018 Guillaume Gomez",
    ),
    (
        "minimal-lexical 0.2.1",
        "src/libm.rs",
        "/* origin: FreeBSD /usr/src/lib/msun/src/e_powf.c */",
        "* is preserved.\n * ====================================================\n */",
    ),
    (
        "minimal-lexical 0.2.1",
        "src/libm.rs",
        "/* origin: FreeBSD /usr/src/lib/msun/src/e_pow.c */",
        "* is preserved.\n * ====================================================\n */",
    ),
    (
        "schemars 1.2.3",
        "src/_private/regex_syntax.rs",
        "// Copied from regex_syntax crate",
        "// DEALINGS IN THE SOFTWARE.",
    ),
    (
        "xn 0.2.13",
        "src/models/mimi.rs",
        "// Copyright (c) Kyutai, all rights reserved.",
        "// LICENSE file in the root directory of this source tree.",
    ),
    (
        "esaxx-rs 0.1.10",
        "src/esaxx.cpp",
        "//  * sais.hxx for sais-lite",
        "//  * OTHER DEALINGS IN THE SOFTWARE.",
    ),
    (
        "esaxx-rs 0.1.10",
        "src/esaxx.cpp",
        "//  * esa.hxx",
        "//  * OTHER DEALINGS IN THE SOFTWARE.",
    ),
];

pub(super) fn read(dir: &Path, package: &str) -> Result<Vec<String>> {
    SCOPED
        .iter()
        .filter(|(id, _, _, _)| *id == package)
        .map(|&(_, relative, begin, end)| {
            let file = dir.join(relative);
            let data = fs::read_to_string(file)
                .with_context(|| {
                    format!("reading reviewed notice input for {package}: {relative}")
                })?
                .replace("\r\n", "\n");
            let text = span(&data, begin, end, |body| {
                SCOPED.iter().any(|&(id, path, start, _)| {
                    id == package && path == relative && body.contains(start)
                })
            })
            .with_context(|| format!("checking reviewed notice input for {package}: {relative}"))?;
            Ok(text.to_owned())
        })
        .collect()
}

fn span<'a>(
    source: &'a str,
    begin: &str,
    end: &str,
    crosses_start: impl Fn(&str) -> bool,
) -> Result<&'a str> {
    let mut found = None;
    for (offset, _) in source.match_indices(begin) {
        let after = &source[offset..];
        let (body, _) = after[begin.len()..]
            .split_once(end)
            .context("reviewed notice end missing")?;
        ensure!(
            !crosses_start(body),
            "reviewed notice end missing before next start"
        );
        let text = &after[..begin.len() + body.len() + end.len()];
        ensure!(
            u64::try_from(text.len())? <= PREFIX_LIMIT && found.is_none_or(|prior| prior == text),
            "oversized or ambiguous reviewed notice"
        );
        found = Some(text);
    }
    found.context("reviewed notice start missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_requires_both_reviewed_generator_grants_without_copying_programs() {
        let dir = crate::scratch::dir("ring-generator-grants");
        let grants = [
            (
                "crypto/cipher/asm/chacha20_poly1305_x86_64.pl",
                "# Copyright (c) 2015, CloudFlare Ltd.",
            ),
            (
                "crypto/chacha/asm/chacha-x86_64.pl",
                "# Copyright 2016 The OpenSSL Project Authors. All Rights Reserved.",
            ),
        ];
        let mut expected = Vec::new();
        for (relative, credit) in grants {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let notice = format!(
                "{credit}\n# Apache-2.0 fixture terms and\n# limitations under the License."
            );
            fs::write(
                path,
                format!("#!/usr/bin/env perl\n\n{notice}\n\nsub fixture {{}}"),
            )
            .unwrap();
            expected.push(notice);
        }
        assert_eq!(read(&dir, "ring 0.17.14").unwrap(), expected);
        fs::remove_file(dir.join(grants[1].0)).unwrap();
        assert_eq!(
            read(&dir, "ring 0.17.14").unwrap_err().to_string(),
            "reading reviewed notice input for ring 0.17.14: crypto/chacha/asm/chacha-x86_64.pl"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_complete_unambiguous_reviewed_spans_are_returned() {
        let span = |source| span(source, "A", "END", |body| body.contains('A'));
        assert_eq!(
            span("A grant END\nunrelated\nA grant END").unwrap(),
            "A grant END"
        );
        assert_eq!(
            span("no grant").unwrap_err().to_string(),
            "reviewed notice start missing"
        );
        assert_eq!(
            span("A grant").unwrap_err().to_string(),
            "reviewed notice end missing"
        );
        assert_eq!(
            span("A grant END\nA other END").unwrap_err().to_string(),
            "oversized or ambiguous reviewed notice"
        );
    }

    #[test]
    fn esaxx_cannot_use_the_next_grants_terminator() {
        let dir = crate::scratch::dir("esaxx-grant-boundary");
        fs::create_dir(dir.join("src")).unwrap();
        let path = dir.join("src/esaxx.cpp");
        let end = "//  * OTHER DEALINGS IN THE SOFTWARE.";
        let first = "//  * sais.hxx for sais-lite\n//  * First grant";
        let second = "//  * esa.hxx\n//  * Second grant";
        let code = "int fixture() { return 0; }";
        fs::write(&path, format!("{first}\n{end}\n{code}\n{second}\n{end}\n")).unwrap();
        assert_eq!(
            read(&dir, "esaxx-rs 0.1.10").unwrap(),
            [format!("{first}\n{end}"), format!("{second}\n{end}")]
        );
        fs::write(&path, format!("{first}\n{code}\n{second}\n{end}\n")).unwrap();
        assert_eq!(
            read(&dir, "esaxx-rs 0.1.10")
                .unwrap_err()
                .chain()
                .last()
                .unwrap()
                .to_string(),
            "reviewed notice end missing before next start"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
