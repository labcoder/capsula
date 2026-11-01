//! Strict `PresentMon` 2.6 v1/raw-QPC rows and bounded dock-present display joins.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

const HEADER: &str = "Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay,msUntilRenderStart,msGPUActive,QPCTime";

#[derive(Debug)]
pub(super) struct PresentRows {
    rows: Vec<Present>,
    joined: HashSet<(u64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Present {
    pub(super) qpc: i64,
    pub(super) swapchain: u64,
    pub(super) dropped: bool,
    pub(super) display_after_ms: Option<f64>,
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
    let mut identities = HashSet::new();
    for (index, line) in lines.enumerate() {
        let row = parse_row(line, expected_pid)
            .with_context(|| format!("invalid PresentMon row {}", index + 2))?;
        ensure!(
            identities.insert((row.swapchain, row.qpc)),
            "duplicate PresentMon present"
        );
        rows.push(row);
    }
    ensure!(!rows.is_empty(), "PresentMon CSV contains no presents");
    Ok(PresentRows {
        rows,
        joined: HashSet::new(),
    })
}

fn parse_row(line: &str, expected_pid: u32) -> Result<Present> {
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
    for index in [7, 8, 9, 12, 14, 15, 16, 17] {
        decimal(&fields[index])?;
    }
    let display_after_ms = match fields[13].as_str() {
        "" | "NA" => None,
        value => Some(decimal(value)?).filter(|value| *value > 0.0),
    };
    let qpc = i64::try_from(unsigned(&fields[18])?)?;
    ensure!(qpc > 0, "invalid raw PresentMon QPC");
    Ok(Present {
        qpc,
        swapchain,
        dropped,
        display_after_ms,
    })
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

    pub(super) fn non_dock_swapchain(
        &self,
        dock_swapchain: u64,
        paint_qpc: i64,
        completion_qpc: i64,
    ) -> std::result::Result<u64, Join> {
        if dock_swapchain == 0 || paint_qpc <= 0 || completion_qpc <= paint_qpc {
            return Err(Join::InvalidClock);
        }
        let mut candidates = self.rows.iter().filter(|row| {
            row.swapchain != dock_swapchain && (paint_qpc..=completion_qpc).contains(&row.qpc)
        });
        let row = candidates.next().ok_or(Join::Missing)?;
        if candidates.next().is_some() {
            return Err(Join::Ambiguous);
        }
        Ok(row.swapchain)
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
        if dock_swapchain == 0
            || paint_qpc <= 0
            || completion_qpc <= paint_qpc
            || qpc_frequency <= 0
        {
            return Join::InvalidClock;
        }
        let mut candidates = self.rows.iter().filter(|row| {
            row.swapchain == dock_swapchain && (paint_qpc..=completion_qpc).contains(&row.qpc)
        });
        let Some(row) = candidates.next() else {
            return Join::Missing;
        };
        if candidates.next().is_some() {
            return Join::Ambiguous;
        }
        if row.dropped {
            return Join::Dropped;
        }
        let Some(displayed_qpc) = row
            .display_after_ms
            .and_then(|ms| display_qpc(row.qpc, ms, qpc_frequency))
        else {
            return Join::Unavailable;
        };
        if !self.joined.insert((row.swapchain, row.qpc)) {
            return Join::Reused;
        }
        Join::Displayed {
            present_qpc: row.qpc,
            displayed_qpc,
        }
    }
}

/// Quantizes the delay to nanoseconds, below `PresentMon`'s 0.0001 ms timing precision.
pub(super) fn display_qpc(present_qpc: i64, ms: f64, frequency: i64) -> Option<i64> {
    if present_qpc <= 0 || frequency <= 0 || !ms.is_finite() || ms <= 0.0 {
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
    present_qpc.checked_add(ticks)
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
            parsed.non_dock_swapchain(10, 99, 100),
            Err(Join::Missing),
            "dock cannot identify panel"
        );
        assert_eq!(
            parsed.non_dock_swapchain(10, 100, 101),
            Ok(20),
            "dropped candidate retains its identity"
        );
        assert_eq!(
            parsed.non_dock_swapchain(10, 100, 102),
            Err(Join::Ambiguous),
            "never choose one of two chains"
        );
    }
}
