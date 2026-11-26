//! Validated non-content input milestones.

use anyhow::{Context as _, Result, bail, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The shortcut's chord went down (`recognized: false`) or its 300 ms hold was recognised,
    /// stamped with the native callback's QPC.
    Hold {
        recognized: bool,
        callback_qpc: i64,
    },
    /// The dock's line took or left the Listening colour.
    Listening(bool),
    /// The brain reported whether speech input is loaded and ready.
    SpeechReady(bool),
    Opened(bool),
    Draft {
        nonempty: bool,
        preserved: bool,
    },
    Dismissed,
    EscapeStop,
    EscapeClear,
    EscapeDismiss,
    EscapeIgnore,
    EscapeState {
        wanted: bool,
        stop: bool,
    },
    /// A line the shell records about something other than input; no gate reads it.
    Status,
}

/// Every non-input line the shell records; any other unknown event still fails.
const STATUS: [&str; 10] = [
    "link state=",
    "carry drop perch=",
    "run phase=",
    "submit sent=",
    "dock settled",
    "dock paints settled=",
    "kumo baked ",
    "kumo frame shade_us=",
    "kumo reshades=",
    "kumo press at=",
];

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
        let mark = if let Some(value) = event.strip_prefix("hold ") {
            let (kind, callback) = value
                .split_once(" callback_qpc=")
                .context("hold diagnostic callback QPC missing")?;
            let recognized = match kind {
                "warm" => false,
                "recognized" => true,
                _ => bail!("unknown hold diagnostic"),
            };
            let callback_qpc = callback.parse()?;
            ensure!(
                callback_qpc > 0 && callback_qpc <= qpc,
                "invalid hold callback QPC"
            );
            Mark::Hold {
                recognized,
                callback_qpc,
            }
        } else if let Some(value) = event.strip_prefix("speech input ready=") {
            Mark::SpeechReady(value.parse()?)
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
        } else if STATUS.iter().any(|prefix| event.starts_with(prefix)) {
            Mark::Status
        } else {
            match event {
                "resting" => Mark::Status,
                "listening on" => Mark::Listening(true),
                "listening off" => Mark::Listening(false),
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

pub(super) fn summarize_feel(records: &[Record]) -> String {
    let mut opened = [0; 2];
    for record in records {
        if let Mark::Opened(focused) = record.mark {
            opened[usize::from(focused)] += 1;
        }
    }
    format!(
        "recognized_double_taps={} focused_opens={} unfocused_opens={}\nsource=owner feel milestones only; not a human verdict or automated gate\n",
        opened.iter().sum::<usize>(),
        opened[1],
        opened[0]
    )
}

#[cfg(test)]
mod tests {
    use super::{Mark, Record, summarize_feel};

    #[test]
    fn feel_summary_reports_recognition_and_focus_without_treating_it_as_a_gate() {
        let records = [
            "qpc=20 carry drop perch=true",
            "qpc=30 resting",
            "qpc=70 input opened focused=false",
            "qpc=80 input opened focused=true",
        ]
        .map(|line| Record::parse(line).unwrap());
        assert_eq!(
            summarize_feel(&records),
            "recognized_double_taps=2 focused_opens=1 unfocused_opens=1\nsource=owner feel milestones only; not a human verdict or automated gate\n"
        );
    }

    #[test]
    fn diagnostics_accept_only_documented_transcript_free_milestones() {
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
            "qpc=2 kumo",
            "qpc=2 kumo bakedsize=64",
            "qpc=2 link",
            "qpc=3 listening applied_us=1 start_qpc=1",
            "qpc=3 listening painted_us=42",
            "qpc=3 restingX",
        ] {
            assert!(
                Record::parse(line).is_err(),
                "invalid diagnostic envelope or milestone"
            );
        }
    }

    #[test]
    fn hold_and_listening_lines_keep_their_callback_clock_and_reject_the_rest() {
        for (line, mark) in [
            (
                "qpc=50 hold warm callback_qpc=40",
                Mark::Hold {
                    recognized: false,
                    callback_qpc: 40,
                },
            ),
            (
                "qpc=50 hold recognized callback_qpc=50",
                Mark::Hold {
                    recognized: true,
                    callback_qpc: 50,
                },
            ),
            ("qpc=60 listening on", Mark::Listening(true)),
            ("qpc=70 listening off", Mark::Listening(false)),
            ("qpc=80 speech input ready=true", Mark::SpeechReady(true)),
            ("qpc=90 speech input ready=false", Mark::SpeechReady(false)),
        ] {
            assert_eq!(
                Record::parse(line).unwrap(),
                Record {
                    qpc: line[4..6].parse().unwrap(),
                    mark
                },
                "{line}"
            );
        }
        for line in [
            "qpc=50 hold warm callback_qpc=51",
            "qpc=50 hold warm callback_qpc=0",
            "qpc=50 hold warm",
            "qpc=50 hold ended callback_qpc=40",
            "qpc=50 hold recognized callback_qpc=x",
            "qpc=60 listening",
            "qpc=60 listening yes",
            "qpc=80 speech input ready=maybe",
        ] {
            assert!(Record::parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn current_main_status_lines_are_not_input_marks() {
        for (line, qpc) in [
            ("qpc=4847822942044 link state=Connecting", 4_847_822_942_044),
            (
                "qpc=4847822964004 kumo baked size=64 us=3877",
                4_847_822_964_004,
            ),
            (
                "qpc=4847828032301 kumo reshades=0 unchanged=0 shade_us=0",
                4_847_828_032_301,
            ),
            ("qpc=7 kumo frame shade_us=12", 7),
            ("qpc=8 run phase=Thinking", 8),
            ("qpc=9 submit sent=true", 9),
            ("qpc=10 dock settled", 10),
            ("qpc=11 dock paints settled=3", 11),
            ("qpc=12 carry drop perch=false", 12),
            ("qpc=13 resting", 13),
        ] {
            assert_eq!(
                Record::parse(line).unwrap(),
                Record {
                    qpc,
                    mark: Mark::Status
                },
                "{line}"
            );
        }
    }

    #[test]
    fn draft_checkpoints_retain_false_verdicts_and_reject_malformed_fields() {
        for (line, nonempty, preserved) in [
            ("qpc=5 input draft nonempty=true preserved=true", true, true),
            (
                "qpc=6 input draft nonempty=false preserved=false",
                false,
                false,
            ),
            (
                "qpc=7 input draft nonempty=false preserved=true",
                false,
                true,
            ),
            (
                "qpc=8 input draft nonempty=true preserved=false",
                true,
                false,
            ),
        ] {
            assert_eq!(
                Record::parse(line).unwrap().mark,
                Mark::Draft {
                    nonempty,
                    preserved
                }
            );
        }
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
}
