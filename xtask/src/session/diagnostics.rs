//! Validated non-content input milestones and callback-to-Canvas pairing.

use anyhow::{Context as _, Result, bail, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Applied { us: u64, start_qpc: i64 },
    Painted(u64),
    Resting,
    Hold(bool),
    HoldEnded(bool),
    Opened(bool),
    Draft { nonempty: bool, preserved: bool },
    Dismissed,
    EscapeStop,
    EscapeClear,
    EscapeDismiss,
    EscapeIgnore,
    EscapeState { wanted: bool, stop: bool },
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) qpc: i64,
    pub(crate) mark: Mark,
}

impl Record {
    pub(super) fn parse(line: &str) -> Result<Self> {
        let (counter, event) = line
            .strip_prefix("qpc=")
            .and_then(|line| line.split_once(' '))
            .context("invalid input diagnostic envelope")?;
        let qpc = counter.parse()?;
        ensure!(qpc > 0, "invalid diagnostic QPC counter");
        let mark = if let Some(value) = event.strip_prefix("listening applied_us=") {
            let (us, start) = value
                .split_once(" start_qpc=")
                .context("input diagnostic start QPC missing")?;
            let start_qpc = start.parse()?;
            ensure!(
                start_qpc > 0 && start_qpc <= qpc,
                "invalid input diagnostic start QPC"
            );
            Mark::Applied {
                us: us.parse()?,
                start_qpc,
            }
        } else if let Some(value) = event.strip_prefix("listening painted_us=") {
            Mark::Painted(value.parse()?)
        } else if let Some(value) = event.strip_prefix("hold excluded=") {
            Mark::Hold(value.parse()?)
        } else if let Some(value) = event.strip_prefix("hold ended restored=") {
            Mark::HoldEnded(value.parse()?)
        } else if let Some(value) = event.strip_prefix("input opened focused=") {
            Mark::Opened(value.parse()?)
        } else if let Some(value) = event.strip_prefix("input draft nonempty=") {
            let (nonempty, preserved) = value
                .split_once(" preserved=")
                .context("invalid draft diagnostic checkpoint")?;
            Mark::Draft {
                nonempty: nonempty.parse()?,
                preserved: preserved.parse()?,
            }
        } else if let Some(value) = event.strip_prefix("escape wanted=") {
            let (wanted, stop) = value
                .split_once(" stop=")
                .context("invalid Escape diagnostic state")?;
            Mark::EscapeState {
                wanted: wanted.parse()?,
                stop: stop.parse()?,
            }
        } else if let Some(value) = event.strip_prefix("stretch ") {
            stretch_event(value)?;
            Mark::Stretch
        } else {
            match event {
                "resting" => Mark::Resting,
                "input dismissed" => Mark::Dismissed,
                "escape action=Stop" => Mark::EscapeStop,
                "escape action=Clear" => Mark::EscapeClear,
                "escape action=Dismiss" => Mark::EscapeDismiss,
                "escape action=Ignore" => Mark::EscapeIgnore,
                _ => bail!("unknown input diagnostic event"),
            }
        };
        Ok(Self { qpc, mark })
    }
}

fn stretch_event(event: &str) -> Result<()> {
    if let Some(active) = event.strip_prefix("active source=") {
        let (source, bounds) = active
            .split_once(" pill=")
            .context("stretch pill missing")?;
        ensure!(
            matches!(source, "cursor" | "fixture"),
            "unknown stretch source"
        );
        let (pill, overlay) = bounds
            .split_once(" overlay=")
            .context("stretch overlay missing")?;
        for rect in [pill, overlay] {
            ensure!(rect.split(',').count() == 4, "invalid stretch rectangle");
            for (index, value) in rect.split(',').enumerate() {
                let value: i32 = value.parse()?;
                ensure!(index < 2 || value > 0, "empty stretch rectangle");
            }
        }
    } else {
        let counts = event
            .strip_prefix("settled ")
            .or_else(|| event.strip_prefix("totals "))
            .context("unknown stretch milestone")?;
        let mut fields = counts.split_whitespace();
        for prefix in ["ticks=", "shape=", "moves=", "unchanged=", "failed="] {
            fields
                .next()
                .and_then(|field| field.strip_prefix(prefix))
                .context("invalid stretch counters")?
                .parse::<u64>()?;
        }
        ensure!(
            fields.next().is_none(),
            "unexpected stretch diagnostic field"
        );
    }
    Ok(())
}

type CanvasSample = (i64, i64, u64);

pub(super) fn pair_paints(records: &[Record]) -> Result<(Vec<CanvasSample>, usize)> {
    let mut latest = None;
    let mut applied = 0usize;
    let mut pairs = Vec::new();
    for record in records {
        match record.mark {
            Mark::Applied { start_qpc, .. } => {
                latest = Some(start_qpc);
                applied += 1;
            }
            Mark::Painted(us) => pairs.push((
                latest
                    .take()
                    .context("Canvas paint has no unmatched preceding applied milestone")?,
                record.qpc,
                us,
            )),
            _ => {}
        }
    }
    let missing = applied
        .checked_sub(pairs.len())
        .context("more Canvas paints than applied milestones")?;
    Ok((pairs, missing))
}

#[cfg(test)]
mod tests {
    use super::{Mark, Record, pair_paints};

    #[test]
    fn diagnostics_accept_only_documented_transcript_free_milestones() {
        assert_eq!(
            Record::parse("qpc=123 listening applied_us=42 start_qpc=100").unwrap(),
            Record {
                qpc: 123,
                mark: Mark::Applied {
                    us: 42,
                    start_qpc: 100
                }
            },
            "native callback QPC is retained with the applied duration"
        );
        assert_eq!(
            Record::parse("qpc=123 listening painted_us=42").unwrap(),
            Record {
                qpc: 123,
                mark: Mark::Painted(42)
            },
            "paint duration remains a Canvas callback marker"
        );
        assert_eq!(
            Record::parse("qpc=124 escape wanted=true stop=false")
                .unwrap()
                .mark,
            Mark::EscapeState {
                wanted: true,
                stop: false
            },
            "Escape registration state"
        );
        for line in [
            "listening applied_us=1",
            "qpc=0 resting",
            "qpc=1 hold excluded=maybe",
            "qpc=2 unexpected",
            "qpc=3 listening applied_us=-1 start_qpc=1",
            "qpc=3 listening applied_us=1 start_qpc=4",
            "qpc=3 listening applied_us=1 start_qpc=0",
        ] {
            assert!(
                Record::parse(line).is_err(),
                "invalid diagnostic envelope or milestone"
            );
        }
    }

    #[test]
    fn stretch_milestones_are_validated_without_becoming_input_milestones() {
        for event in [
            "active source=fixture pill=-10,20,112,48 overlay=-30,0,300,200",
            "active source=cursor pill=10,20,112,48 overlay=0,0,300,200",
            "settled ticks=12 shape=8 moves=1 unchanged=3 failed=0",
            "totals ticks=12 shape=8 moves=1 unchanged=3 failed=0",
        ] {
            let record = Record::parse(&format!("qpc=1 stretch {event}")).unwrap();
            assert_eq!(
                record,
                Record {
                    qpc: 1,
                    mark: Mark::Stretch
                }
            );
            assert_eq!(pair_paints(&[record]).unwrap(), (Vec::new(), 0));
        }
        for event in [
            "active source=other pill=1,2,3,4 overlay=1,2,3,4",
            "active source=cursor pill=1,2,0,4 overlay=1,2,3,4",
            "active source=cursor pill=1,2,3,4 overlay=1,2,3,4 extra=text",
            "totals ticks=12 shape=8 moves=1 unchanged=3",
            "totals ticks=12 shape=8 moves=1 unchanged=3 failed=oops",
            "totals ticks=12 shape=8 moves=1 unchanged=3 failed=0 extra=text",
        ] {
            assert!(
                Record::parse(&format!("qpc=1 stretch {event}")).is_err(),
                "invalid stretch event: {event}"
            );
        }
    }

    #[test]
    fn draft_checkpoints_retain_false_verdicts_and_reject_malformed_fields() {
        assert_eq!(
            Record::parse("qpc=5 input draft nonempty=true preserved=true")
                .unwrap()
                .mark,
            Mark::Draft {
                nonempty: true,
                preserved: true
            }
        );
        assert_eq!(
            Record::parse("qpc=6 input draft nonempty=false preserved=false")
                .unwrap()
                .mark,
            Mark::Draft {
                nonempty: false,
                preserved: false
            }
        );
        assert_eq!(
            Record::parse("qpc=7 input draft nonempty=false preserved=true")
                .unwrap()
                .mark,
            Mark::Draft {
                nonempty: false,
                preserved: true
            }
        );
        for line in [
            "qpc=5 input draft nonempty=true",
            "qpc=5 input draft nonempty=1 preserved=true",
            "qpc=5 input draft nonempty=true preserved=maybe",
            "qpc=5 input draft preserved=true nonempty=true",
            "qpc=5 input draft nonempty=true preserved=true extra=false",
            "qpc=5 input draft nonempty=true preserved=true preserved=false",
        ] {
            assert!(
                Record::parse(line).is_err(),
                "malformed draft checkpoint: {line}"
            );
        }
    }

    #[test]
    fn paint_pairs_with_latest_applied_without_hiding_a_missing_sample() {
        let records = [
            Record {
                qpc: 11,
                mark: Mark::Applied {
                    us: 1,
                    start_qpc: 10,
                },
            },
            Record {
                qpc: 12,
                mark: Mark::Resting,
            },
            Record {
                qpc: 21,
                mark: Mark::Applied {
                    us: 1,
                    start_qpc: 20,
                },
            },
            Record {
                qpc: 22,
                mark: Mark::Painted(2),
            },
        ];
        assert_eq!(
            pair_paints(&records).unwrap(),
            (vec![(20, 22, 2)], 1),
            "the latest callback is paired and the earlier missing paint remains counted"
        );
        assert!(
            pair_paints(&[Record {
                qpc: 1,
                mark: Mark::Painted(2)
            }])
            .is_err(),
            "orphan paints cannot become successful samples"
        );
    }
}
