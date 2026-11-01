//! Strict `PresentMon` 2.6 v1/raw-QPC rows and bounded present/display joins.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

const HEADER: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime";

#[derive(Debug)]
pub(super) struct PresentRows {
    rows: Vec<Present>,
    first_rows: Vec<SeedSource>,
    joined: HashSet<(u64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Present {
    pub(super) qpc: i64,
    pub(super) swapchain: u64,
    pub(super) dropped: bool,
    pub(super) display_after_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SeedSource {
    pub(super) csv_row: usize,
    pub(super) present: Present,
    pub(super) between_presents_ms: Option<f64>,
    pub(super) between_display_change_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Source {
    Csv { row: usize },
    Seed(SeedSource),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct MatchedPresent {
    pub(super) present: Present,
    pub(super) displayed_qpc: Option<i64>,
    pub(super) source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Join {
    Displayed {
        present_qpc: i64,
        displayed_qpc: i64,
    },
    Missing,
    Ambiguous,
    Dropped,
    Unavailable,
    InvalidClock,
    Reused,
}

pub(super) fn read(path: &Path, expected_pid: u32) -> Result<PresentRows> {
    parse(
        &fs::read_to_string(path).context("reading stopped PresentMon CSV")?,
        expected_pid,
    )
}

fn parse(text: &str, expected_pid: u32) -> Result<PresentRows> {
    ensure!(expected_pid > 0, "invalid expected PresentMon PID");
    ensure!(text.ends_with('\n'), "empty or truncated PresentMon CSV");
    let mut lines = text.strip_prefix('\u{feff}').unwrap_or(text).lines();
    let header = csv_fields(lines.next().context("missing PresentMon header")?)?;
    ensure!(
        header.iter().map(String::as_str).eq(HEADER.split(',')),
        "expected PresentMon 2.6 --v1_metrics --qpc_time header"
    );
    let mut rows = Vec::new();
    let mut first_rows = Vec::new();
    let mut swapchains = HashSet::new();
    let mut identities = HashSet::new();
    for (index, line) in lines.enumerate() {
        let source = parse_row(line, expected_pid, index + 2)
            .with_context(|| format!("invalid PresentMon row {}", index + 2))?;
        let row = source.present;
        ensure!(
            identities.insert((row.swapchain, row.qpc)),
            "duplicate PresentMon present"
        );
        if swapchains.insert(row.swapchain) {
            first_rows.push(source);
        }
        rows.push(row);
    }
    ensure!(!rows.is_empty(), "PresentMon CSV contains no presents");
    Ok(PresentRows {
        rows,
        first_rows,
        joined: HashSet::new(),
    })
}

fn parse_row(line: &str, expected_pid: u32, csv_row: usize) -> Result<SeedSource> {
    let fields = csv_fields(line)?;
    ensure!(fields.len() == 19, "wrong PresentMon column count");
    ensure!(
        !fields[0].is_empty() && !fields[3].is_empty() && !fields[11].is_empty(),
        "empty PresentMon text field"
    );
    let pid = unsigned(&fields[1])?;
    ensure!(pid == u64::from(expected_pid), "unexpected PresentMon PID");
    let address = fields[2]
        .strip_prefix("0x")
        .context("invalid swapchain address")?;
    ensure!(
        !address.is_empty() && address.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid swapchain address"
    );
    let swapchain = u64::from_str_radix(address, 16)?;
    ensure!(swapchain > 0, "null PresentMon swapchain");
    unsigned(&fields[4])?;
    unsigned(&fields[5])?;
    let dropped = bit(&fields[6])?;
    bit(&fields[10])?;
    for index in [7, 8, 12, 15, 16, 17] {
        decimal(&fields[index])?;
    }
    let display_after_ms = optional_interval(&fields[13])?.filter(|value| *value > 0.0);
    let qpc = i64::try_from(unsigned(&fields[18])?)?;
    ensure!(qpc > 0, "invalid raw PresentMon QPC");
    Ok(SeedSource {
        csv_row,
        present: Present {
            qpc,
            swapchain,
            dropped,
            display_after_ms,
        },
        between_presents_ms: optional_interval(&fields[9])?,
        between_display_change_ms: optional_interval(&fields[14])?,
    })
}

fn optional_interval(value: &str) -> Result<Option<f64>> {
    match value {
        "" | "NA" => Ok(None),
        value => Ok(Some(decimal(value)?)),
    }
}

fn unsigned(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid unsigned PresentMon integer"
    );
    Ok(value.parse()?)
}

fn bit(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("invalid PresentMon boolean"),
    }
}

fn decimal(value: &str) -> Result<f64> {
    let mut dots = 0;
    ensure!(
        !value.is_empty()
            && value.bytes().all(|byte| {
                if byte == b'.' {
                    dots += 1;
                    true
                } else {
                    byte.is_ascii_digit()
                }
            })
            && dots <= 1
            && value.bytes().any(|byte| byte.is_ascii_digit()),
        "invalid PresentMon decimal"
    );
    let number: f64 = value.parse()?;
    ensure!(
        number.is_finite() && number >= 0.0,
        "invalid PresentMon timing"
    );
    Ok(number)
}

fn csv_fields(line: &str) -> Result<Vec<String>> {
    let mut chars = line.chars().peekable();
    let mut fields = Vec::new();
    loop {
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    Some('"') => break,
                    Some('\r' | '\n') | None => bail!("truncated quoted PresentMon field"),
                    Some(ch) => field.push(ch),
                }
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch == ',' {
                    break;
                }
                ensure!(
                    !matches!(ch, '"' | '\r' | '\n'),
                    "invalid PresentMon CSV quoting"
                );
                field.push(ch);
                chars.next();
            }
        }
        fields.push(field);
        match chars.next() {
            Some(',') => {}
            None => return Ok(fields),
            _ => bail!("invalid text after quoted PresentMon field"),
        }
    }
}

impl PresentRows {
    pub(super) fn rows(&self) -> &[Present] {
        &self.rows
    }

    pub(super) fn seed_sources(&self) -> &[SeedSource] {
        &self.first_rows
    }

    pub(super) fn seed(
        &self,
        swapchain: u64,
        frequency: i64,
    ) -> std::result::Result<MatchedPresent, Join> {
        if swapchain == 0 || frequency <= 0 {
            return Err(Join::InvalidClock);
        }
        let source = self
            .first_rows
            .iter()
            .find(|source| source.present.swapchain == swapchain)
            .ok_or(Join::Missing)?;
        let interval = source
            .between_presents_ms
            .filter(|ms| *ms > 0.0)
            .ok_or(Join::Unavailable)?;
        let delta = qpc_delta(interval, frequency).ok_or(Join::InvalidClock)?;
        let qpc = source
            .present
            .qpc
            .checked_sub(delta)
            .filter(|qpc| *qpc > 0)
            .ok_or(Join::InvalidClock)?;
        let displayed_qpc = source
            .present
            .display_after_ms
            .filter(|_| !source.present.dropped)
            .and_then(|ms| display_qpc(source.present.qpc, ms, frequency))
            .zip(source.between_display_change_ms)
            .and_then(|(displayed, ms)| displayed.checked_sub(qpc_delta(ms, frequency)?))
            .filter(|displayed| *displayed >= qpc)
            .ok_or(Join::Unavailable)?;
        Ok(MatchedPresent {
            present: Present {
                qpc,
                swapchain,
                dropped: false,
                display_after_ms: None,
            },
            displayed_qpc: Some(displayed_qpc),
            source: Source::Seed(*source),
        })
    }

    pub(super) fn candidates(&self, frequency: i64) -> impl Iterator<Item = MatchedPresent> + '_ {
        let observed = self
            .rows
            .iter()
            .enumerate()
            .map(move |(index, &present)| MatchedPresent {
                displayed_qpc: present
                    .display_after_ms
                    .and_then(|ms| display_qpc(present.qpc, ms, frequency)),
                present,
                source: Source::Csv { row: index + 2 },
            });
        observed.chain(
            self.first_rows
                .iter()
                .filter_map(move |source| self.seed(source.present.swapchain, frequency).ok()),
        )
    }

    pub(super) fn non_dock_swapchain(
        &self,
        dock_swapchain: u64,
        paint_qpc: i64,
        completion_qpc: i64,
        frequency: i64,
    ) -> std::result::Result<u64, Join> {
        if dock_swapchain == 0 || paint_qpc <= 0 || completion_qpc <= paint_qpc || frequency <= 0 {
            return Err(Join::InvalidClock);
        }
        let mut candidates = self.candidates(frequency).filter(|row| {
            row.present.swapchain != dock_swapchain
                && (paint_qpc..=completion_qpc).contains(&row.present.qpc)
        });
        let row = candidates.next().ok_or(Join::Missing)?;
        if candidates.next().is_some() {
            return Err(Join::Ambiguous);
        }
        Ok(row.present.swapchain)
    }

    pub(super) fn warm_swapchain(&self) -> Result<u64> {
        let swapchain = self
            .rows
            .first()
            .context("warm capture contains no presents")?
            .swapchain;
        ensure!(
            self.rows.iter().all(|row| row.swapchain == swapchain),
            "warm capture contains multiple swapchains"
        );
        ensure!(
            self.rows
                .iter()
                .any(|row| !row.dropped && row.display_after_ms.is_some()),
            "warm capture contains no displayed eligible present"
        );
        Ok(swapchain)
    }

    pub(super) fn join(
        &mut self,
        dock_swapchain: u64,
        paint_qpc: i64,
        completion_qpc: i64,
        qpc_frequency: i64,
    ) -> Join {
        if qpc_frequency <= 0 {
            return Join::InvalidClock;
        }
        let row = match self.candidate(dock_swapchain, paint_qpc, completion_qpc, qpc_frequency) {
            Ok(row) => row,
            Err(status) => return status,
        };
        let Some(displayed_qpc) = row.displayed_qpc else {
            return Join::Unavailable;
        };
        if !self.joined.insert((row.present.swapchain, row.present.qpc)) {
            return Join::Reused;
        }
        Join::Displayed {
            present_qpc: row.present.qpc,
            displayed_qpc,
        }
    }

    pub(super) fn join_present(
        &mut self,
        swapchain: u64,
        paint_qpc: i64,
        completion_qpc: i64,
        frequency: i64,
    ) -> std::result::Result<MatchedPresent, Join> {
        let row = self.candidate(swapchain, paint_qpc, completion_qpc, frequency)?;
        if !self.joined.insert((row.present.swapchain, row.present.qpc)) {
            return Err(Join::Reused);
        }
        Ok(row)
    }

    fn candidate(
        &self,
        swapchain: u64,
        paint_qpc: i64,
        completion_qpc: i64,
        frequency: i64,
    ) -> std::result::Result<MatchedPresent, Join> {
        if swapchain == 0 || paint_qpc <= 0 || completion_qpc <= paint_qpc || frequency <= 0 {
            return Err(Join::InvalidClock);
        }
        let mut candidates = self.candidates(frequency).filter(|row| {
            row.present.swapchain == swapchain
                && (paint_qpc..=completion_qpc).contains(&row.present.qpc)
        });
        let Some(row) = candidates.next() else {
            return Err(Join::Missing);
        };
        if candidates.next().is_some() {
            return Err(Join::Ambiguous);
        }
        if row.present.dropped {
            return Err(Join::Dropped);
        }
        Ok(row)
    }
}

/// Quantizes the delay to nanoseconds, below `PresentMon`'s 0.0001 ms timing precision.
pub(super) fn display_qpc(present_qpc: i64, ms: f64, frequency: i64) -> Option<i64> {
    if present_qpc <= 0 {
        return None;
    }
    present_qpc.checked_add(qpc_delta(ms, frequency)?)
}

fn qpc_delta(ms: f64, frequency: i64) -> Option<i64> {
    if frequency <= 0 || !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    let nanos = Duration::try_from_secs_f64(ms / 1000.0).ok()?.as_nanos();
    let ticks = nanos
        .checked_mul(u128::try_from(frequency).ok()?)?
        .checked_add(500_000_000)?
        / 1_000_000_000;
    let ticks = i64::try_from(ticks).ok()?;
    if ticks == 0 {
        return None;
    }
    Some(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum Disposition {
        Presented,
        Dropped,
    }

    use Disposition::{Dropped, Presented};

    fn row(qpc: i64, swapchain: u64, disposition: Disposition, display_ms: &str) -> String {
        format!(
            "capsula.exe,35620,0x{swapchain:X},DXGI,0,0,{},0.30478100000000,0.3374,60.3792,0,Composed: Flip,0.7355,{display_ms},62.6153,0,0.3765,0.352,{qpc}\r\n",
            u8::from(matches!(disposition, Dropped))
        )
    }

    fn csv(rows: &str) -> String {
        format!("\u{feff}{HEADER}\r\n{rows}")
    }

    fn seed_row(qpc: i64, chain: u64, present_ms: &str, display_ms: &str) -> String {
        row(qpc, chain, Presented, "13")
            .replace(",60.3792,", &format!(",{present_ms},"))
            .replace(",62.6153,", &format!(",{display_ms},"))
    }

    #[test]
    fn derives_only_each_swapchains_first_seed_with_explicit_provenance() {
        let mut parsed = parse(
            &csv(&[
                seed_row(122, 10, "20", "20"),
                seed_row(222, 20, "20", "20"),
                seed_row(242, 20, "1", "1"),
            ]
            .concat()),
            35620,
        )
        .unwrap();
        let seed = parsed.seed(20, 1000).unwrap();
        assert_eq!(seed.present.qpc, 202);
        assert_eq!(seed.displayed_qpc, Some(215));
        let Source::Seed(source) = seed.source else {
            panic!("seed must retain derived provenance");
        };
        assert_eq!(source.csv_row, 3);
        assert_eq!(source.present.qpc, 222);
        assert_eq!(source.between_presents_ms, Some(20.0));
        assert_eq!(source.between_display_change_ms, Some(20.0));
        assert_eq!(parsed.rows().len(), 3, "raw CSV rows stay raw");
        assert_eq!(parsed.candidates(1000).count(), 5, "one seed per swapchain");
        assert_eq!(parsed.non_dock_swapchain(10, 201, 205, 1000), Ok(20));
        assert_eq!(parsed.join_present(20, 201, 205, 1000), Ok(seed));
        assert_eq!(parsed.join(20, 201, 205, 1000), Join::Reused);
        assert_eq!(
            parsed.join_present(20, 200, 223, 1000),
            Err(Join::Ambiguous),
            "raw and derived candidates count equally"
        );
        assert_eq!(
            parsed.join_present(20, 240, 241, 1000),
            Err(Join::Missing),
            "later rows never derive more seeds"
        );
        assert_eq!(parsed.seed(20, 0), Err(Join::InvalidClock));

        let mut parsed = parse(&csv(&seed_row(122, 10, "20", "20")), 35620).unwrap();
        assert_eq!(
            parsed.join(10, 101, 105, 1000),
            Join::Displayed {
                present_qpc: 102,
                displayed_qpc: 115
            }
        );
        assert_eq!(parsed.join_present(10, 101, 105, 1000), Err(Join::Reused));
        let dropped = seed_row(122, 10, "20", "20").replace("DXGI,0,0,0,", "DXGI,0,0,1,");
        let mut parsed = parse(&csv(&dropped), 35620).unwrap();
        assert_eq!(
            parsed.seed(10, 1000),
            Err(Join::Unavailable),
            "a dropped following row provides no seed display proof"
        );
        assert_eq!(parsed.join_present(10, 101, 105, 1000), Err(Join::Missing));
        assert_eq!(parsed.join_present(10, 121, 125, 1000), Err(Join::Dropped));
    }

    #[test]
    fn unavailable_or_invalid_seed_intervals_never_fabricate_endpoints() {
        for interval in ["", "NA", "0"] {
            let parsed = parse(&csv(&seed_row(122, 10, interval, "20")), 35620).unwrap();
            assert_eq!(
                parsed.seed(10, 1000),
                Err(Join::Unavailable),
                "present interval {interval:?}"
            );
            assert_eq!(
                parsed.candidates(1000).count(),
                1,
                "only the observed row remains"
            );
            let mut parsed = parse(&csv(&seed_row(122, 10, "20", interval)), 35620).unwrap();
            assert_eq!(parsed.seed(10, 1000), Err(Join::Unavailable));
            assert_eq!(
                parsed.join_present(10, 101, 105, 1000),
                Err(Join::Missing),
                "display interval {interval:?} cannot prove seed eligibility"
            );
            let mut parsed = parse(&csv(&seed_row(122, 10, "20", interval)), 35620).unwrap();
            assert_eq!(
                parsed.join(10, 101, 105, 1000),
                Join::Missing,
                "display joins remain strict"
            );
        }
        for interval in ["NaN", "-1", "inf"] {
            assert!(parse(&csv(&seed_row(122, 10, interval, "20")), 35620).is_err());
        }
        let parsed = parse(&csv(&seed_row(122, 10, "200", "20")), 35620).unwrap();
        assert_eq!(
            parsed.seed(10, 1000),
            Err(Join::InvalidClock),
            "seed cannot precede the clock origin"
        );
        let parsed = parse(&csv(&seed_row(122, 10, "0.0001", "20")), 35620).unwrap();
        assert_eq!(
            parsed.seed(10, 1000),
            Err(Join::InvalidClock),
            "subtick interval cannot become a fabricated zero delay"
        );
        let parsed = parse(&csv(&seed_row(122, 10, "20", "200")), 35620).unwrap();
        assert_eq!(
            parsed.seed(10, 1000),
            Err(Join::Unavailable),
            "display cannot precede the seed present"
        );
    }

    #[test]
    fn seed_eligibility_requires_display_history_from_the_suppressed_present() {
        for display_ms in ["0", "NA"] {
            let parsed = parse(&csv(&seed_row(122, 10, "20", display_ms)), 35620).unwrap();
            assert_eq!(parsed.seed(10, 1000), Err(Join::Unavailable));
            assert_eq!(parsed.candidates(1000).count(), 1);
        }
        for display_ms in ["0", "NA"] {
            let row = seed_row(122, 10, "20", "20").replace(",13,", &format!(",{display_ms},"));
            let parsed = parse(&csv(&row), 35620).unwrap();
            assert_eq!(parsed.seed(10, 1000), Err(Join::Unavailable));
        }
        let parsed = parse(&csv(&seed_row(122, 10, "20", "33")), 35620).unwrap();
        let seed = parsed.seed(10, 1000).unwrap();
        assert_eq!(seed.present.qpc, 102);
        assert_eq!(seed.displayed_qpc, Some(102));
        assert!(!seed.present.dropped);
        let parsed = parse(&csv(&seed_row(122, 10, "20", "34")), 35620).unwrap();
        assert_eq!(parsed.seed(10, 1000), Err(Join::Unavailable));
    }

    #[test]
    fn actual_bom_header_uses_raw_qpc_and_retains_dropped() {
        let text = csv(&[
            row(2_215_640_628_595, 0x277_660D_D170, Presented, "13.2356"),
            row(2_215_668_221_740, 0x277_660D_D170, Dropped, "0"),
        ]
        .concat());
        let parsed = parse(&text, 35620).unwrap();
        assert_eq!(parsed.rows.len(), 2, "dropped row must remain");
        assert_eq!(parsed.rows[0].qpc, 2_215_640_628_595, "raw integer QPC");
        assert!(parsed.rows[1].dropped, "dropped flag must remain");
        assert_eq!(
            parsed.warm_swapchain().unwrap(),
            0x277_660D_D170,
            "dock identity"
        );
        assert!(parse(&text, 1).is_err(), "every PID must match");
        let mixed_pid = csv(&[
            row(100, 10, Presented, "1"),
            row(200, 10, Presented, "1").replace(",35620,", ",1,"),
        ]
        .concat());
        assert!(
            parse(&mixed_pid, 35620).is_err(),
            "later PID must match too"
        );
    }

    #[test]
    fn rejects_empty_malformed_truncated_and_unknown_clock_csv() {
        let valid = csv(&row(1000, 10, Presented, "1"));
        for text in [
            String::new(),
            csv(""),
            valid.trim_end().to_owned(),
            valid.replace(",1000\r", ",1.5\r"),
            valid.replace(",1000\r", ",0\r"),
            valid.replace(",1000\r", ",9223372036854775808\r"),
            valid.replace("QPCTime", "CPUStartQPC"),
            valid.replace("ProcessID", "QPCTime"),
            valid.replace(",1,62", ",NaN,62"),
            format!("{valid}{}", row(1000, 10, Presented, "1")),
            valid.replace(",0.352,", ","),
            valid.replace("capsula.exe", "\"unterminated"),
        ] {
            assert!(parse(&text, 35620).is_err(), "must reject {text:?}");
        }
        let quoted = valid.replace("capsula.exe", "\"capsula, \"\"dev\"\".exe\"");
        assert_eq!(
            parse(&quoted, 35620).unwrap().rows.len(),
            1,
            "CSV escaped quotes and commas"
        );
        assert!(
            parse(&valid.replace("capsula.exe", "\"capsula\"junk"), 35620).is_err(),
            "quoted field must end at delimiter"
        );
    }

    #[test]
    fn warm_requires_one_swapchain_and_a_displayed_row() {
        assert!(
            parse(&csv(&row(100, 10, Dropped, "0")), 35620)
                .unwrap()
                .warm_swapchain()
                .is_err(),
            "dropped-only warm capture is ineligible"
        );
        assert!(
            parse(
                &csv(&[row(100, 10, Presented, "1"), row(110, 20, Presented, "1")].concat()),
                35620
            )
            .unwrap()
            .warm_swapchain()
            .is_err(),
            "warm capture must identify exactly one swapchain"
        );
    }

    #[test]
    fn joins_only_one_bounded_dock_present_without_reuse() {
        let mut parsed = parse(
            &csv(&[
                row(100, 10, Presented, "1.25"),
                row(101, 20, Presented, "1"),
                row(200, 10, Dropped, "0"),
                row(300, 10, Presented, "0"),
            ]
            .concat()),
            35620,
        )
        .unwrap();
        assert_eq!(
            parsed.join(10, 1, 99, 10_000),
            Join::Missing,
            "no future matching"
        );
        assert_eq!(
            parsed.join(10, 99, 201, 10_000),
            Join::Ambiguous,
            "dropped candidates still count"
        );
        assert_eq!(
            parsed.join(10, 100, 101, 10_000),
            Join::Displayed {
                present_qpc: 100,
                displayed_qpc: 113
            },
            "inclusive bounded dock join ignores panel swapchain"
        );
        assert_eq!(
            parsed.join(10, 99, 100, 10_000),
            Join::Reused,
            "no present may serve two frames"
        );
        assert_eq!(
            parsed.join(10, 199, 200, 10_000),
            Join::Dropped,
            "dropped is explicit"
        );
        assert_eq!(
            parsed.join(10, 299, 300, 10_000),
            Join::Unavailable,
            "zero display latency is unavailable"
        );
        assert_eq!(
            parsed.join(10, 100, 100, 10_000),
            Join::InvalidClock,
            "interval must be positive"
        );
        assert_eq!(
            parsed.join(10, 1, 99, 0),
            Join::InvalidClock,
            "frequency must be positive"
        );
    }

    #[test]
    fn present_joins_allow_unavailable_display_but_share_strict_identity_checks() {
        let mut parsed = parse(
            &csv(&[
                row(100, 10, Presented, "13"),
                row(200, 10, Presented, "NA"),
                row(300, 10, Dropped, "0"),
                row(301, 10, Presented, "1"),
            ]
            .concat()),
            35620,
        )
        .unwrap();
        assert_eq!(
            parsed.join_present(10, 99, 100, 1000).unwrap().present.qpc,
            100,
            "present endpoint does not include display delay"
        );
        assert_eq!(
            parsed.join(10, 99, 100, 1000),
            Join::Reused,
            "display joins cannot reuse a present-gate sample"
        );
        assert_eq!(
            parsed.join(10, 199, 200, 1000),
            Join::Unavailable,
            "receipt/display joins still require display feedback"
        );
        let present = parsed.join_present(10, 199, 200, 1000).unwrap().present;
        assert_eq!(present.qpc, 200, "unavailable display retains present QPC");
        assert_eq!(
            present.display_after_ms, None,
            "no fabricated display delay"
        );
        assert_eq!(parsed.join_present(10, 199, 200, 1000), Err(Join::Reused));
        assert_eq!(parsed.join_present(10, 1, 39, 1000), Err(Join::Missing));
        assert_eq!(
            parsed.join_present(10, 299, 301, 1000),
            Err(Join::Ambiguous)
        );
        assert_eq!(parsed.join_present(10, 299, 300, 1000), Err(Join::Dropped));
        assert_eq!(
            parsed.join_present(10, 100, 100, 1000),
            Err(Join::InvalidClock)
        );

        let mut parsed = parse(&csv(&row(100, 10, Presented, "1")), 35620).unwrap();
        assert!(matches!(
            parsed.join(10, 99, 100, 1000),
            Join::Displayed { .. }
        ));
        assert_eq!(
            parsed.join_present(10, 99, 100, 1000),
            Err(Join::Reused),
            "present joins cannot reuse a receipt/display sample"
        );
    }

    #[test]
    fn display_conversion_rounds_delta_without_rounding_absolute_qpc() {
        assert_eq!(
            display_qpc(9_007_199_254_740_993, 13.2356, 10_000_000),
            Some(9_007_199_254_873_349),
            "absolute QPC beyond f64 integer precision remains exact"
        );
        assert_eq!(
            display_qpc(100, 1.25, 10_000),
            Some(113),
            "half tick rounds up"
        );
        assert_eq!(
            display_qpc(i64::MAX - 1, 1.0, 1000),
            Some(i64::MAX),
            "last valid tick"
        );
        assert_eq!(display_qpc(i64::MAX, 1.0, 1000), None, "addition overflow");
        assert_eq!(
            display_qpc(1, f64::INFINITY, 1000),
            None,
            "nonfinite latency"
        );
        assert_eq!(display_qpc(1, f64::MAX, 1000), None, "conversion overflow");
        assert_eq!(display_qpc(1, 1.0, -1), None, "invalid frequency");
    }

    #[test]
    fn panel_identification_counts_all_non_dock_candidates() {
        let parsed = parse(
            &csv(&[
                row(100, 10, Presented, "1"),
                row(101, 20, Dropped, "0"),
                row(102, 30, Presented, "1"),
            ]
            .concat()),
            35620,
        )
        .unwrap();
        assert_eq!(
            parsed.non_dock_swapchain(10, 99, 100, 1000),
            Err(Join::Missing),
            "dock cannot identify panel"
        );
        assert_eq!(
            parsed.non_dock_swapchain(10, 100, 101, 1000),
            Ok(20),
            "dropped candidate retains its identity"
        );
        assert_eq!(
            parsed.non_dock_swapchain(10, 100, 102, 1000),
            Err(Join::Ambiguous),
            "never choose one of two chains"
        );
    }
}
