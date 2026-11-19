//! Strict brain receipts and draw-step timing joins.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::LIVE_CHATGPT_TEXT_STAMPS_FILE;

use crate::gate::observation::{DRAW_STEPS, DrawKey, DrawStep, Kind, Observation, milliseconds};
use crate::gate::present::{Join, MatchedPresent, PresentRows, Source};
use crate::platform::input_session::Session;

const RECEIPT_CAP: u64 = 2048;

#[derive(Debug, PartialEq, Eq)]
struct BrainStep {
    key: DrawKey,
    bytes: usize,
    call: i64,
    first: i64,
    close: i64,
    durable: i64,
    pipe: i64,
}

fn read(file: File) -> Result<String> {
    use std::io::Read as _;

    let text = std::io::read_to_string(file.take(RECEIPT_CAP + 1))?;
    ensure!(
        u64::try_from(text.len())? <= RECEIPT_CAP,
        "brain draw receipt exceeds bound"
    );
    Ok(text)
}

pub(super) fn ready(path: &Path, pid: u32, session: &Session) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        session.guard()?;
        match File::open(path) {
            Ok(file) => {
                let text = read(file)?;
                if text.ends_with('\n') {
                    ensure!(text.lines().count() == 1, "unexpected stamps before submit");
                    header(text.lines().next(), pid)?;
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("opening brain draw receipt"),
        }
        ensure!(Instant::now() < end, "owned brain draw ready stamp missing");
        session.wait(Duration::from_millis(10))?;
    }
}

fn fields<'a, const N: usize>(line: Option<&'a str>, kind: &str) -> Result<[&'a str; N]> {
    let (actual, text) = line
        .and_then(|value| value.split_once(' '))
        .context("missing or invalid brain draw row")?;
    ensure!(actual == kind, "unexpected brain draw row kind");
    let mut input = text.split(' ');
    let values = std::array::from_fn(|_| input.next().unwrap_or_default());
    ensure!(
        input.next().is_none()
            && values
                .iter()
                .all(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())),
        "invalid brain draw row fields"
    );
    Ok(values)
}

fn header(line: Option<&str>, pid: u32) -> Result<i64> {
    let [owner, budget, qpc] = fields(line, "ready")?;
    let qpc = qpc.parse()?;
    ensure!(
        pid > 0 && owner.parse::<u32>()? == pid && budget == "1" && qpc > 0,
        "invalid brain draw ready stamp"
    );
    Ok(qpc)
}

fn parse(text: &str, pid: u32, dismissed: i64) -> Result<Vec<BrainStep>> {
    ensure!(text.ends_with('\n'), "truncated brain draw receipt");
    let mut lines = text.lines();
    let ready = header(lines.next(), pid)?;
    ensure!(
        ready <= dismissed,
        "brain draw readiness followed typing dismissal"
    );
    let mut samples: Vec<BrainStep> = Vec::with_capacity(DRAW_STEPS);
    for expected in 0..DRAW_STEPS {
        let [
            owner,
            run,
            scene,
            index,
            bytes,
            call,
            first,
            close,
            durable,
            pipe,
        ] = fields(lines.next(), "draw")?;
        let sample = BrainStep {
            key: DrawKey {
                run: run.parse()?,
                scene: scene.parse()?,
                index: index.parse()?,
            },
            bytes: bytes.parse()?,
            call: call.parse()?,
            first: first.parse()?,
            close: close.parse()?,
            durable: durable.parse()?,
            pipe: pipe.parse()?,
        };
        ensure!(
            owner.parse::<u32>()? == pid
                && sample.key.run > 0
                && sample.key.scene > 0
                && sample.key.index == expected
                && sample.bytes > 0
                && [
                    dismissed,
                    sample.call,
                    sample.first,
                    sample.close,
                    sample.durable,
                    sample.pipe
                ]
                .is_sorted(),
            "invalid brain draw step identity or clock order"
        );
        if let Some(previous) = samples.last() {
            ensure!(
                sample.key.run == previous.key.run
                    && sample.key.scene == previous.key.scene
                    && sample.call == previous.call
                    && previous.first <= sample.first
                    && previous.close <= sample.close
                    && previous.durable <= sample.durable
                    && previous.pipe <= sample.pipe,
                "brain draw steps changed owner, call, or clock order"
            );
        }
        samples.push(sample);
    }
    let [owner, model, aux, qpc] = fields(lines.next(), "final")?;
    ensure!(
        owner.parse::<u32>()? == pid
            && model == "0"
            && aux == "0"
            && qpc.parse::<i64>()? >= samples.last().context("no brain draw steps")?.pipe
            && lines.next().is_none(),
        "invalid or nonterminal brain draw final stamp"
    );
    Ok(samples)
}

pub(super) fn report(
    output: &Path,
    stamps: &Path,
    brain_pid: u32,
    dismissed: i64,
    observed: &Observation,
    presents: &mut PresentRows,
    dock: u64,
) -> Result<()> {
    let text = read(File::open(stamps).context("missing brain draw receipt")?)?;
    fs::write(output.join(LIVE_CHATGPT_TEXT_STAMPS_FILE), &text)?;
    let samples = parse(&text, brain_pid, dismissed)?;
    let frequency = observed
        .startup
        .context("missing draw observer startup")?
        .frequency;
    let ms = |ticks| milliseconds(ticks, frequency);
    let mut report = String::from(
        "Offline Responses drawing: step 0 uses the checked-in capture; steps 1–3 are synthetic staged narration in the same call and scene.\n\
         Display feedback, when available, is separate from Present.\n\
         Eligible narration is the estimated overlap of ready-to-active with the prior synthetic segment beyond its independent entrance (no word anchors), using its confirmed first paint as the shifted start. Other constraints can also delay activation; this is not a causal breakdown.\n",
    );
    writeln!(
        report,
        "run={} scene={} typing_dismissed_qpc={dismissed} call_to_first_argument_ms={}",
        samples[0].key.run,
        samples[0].key.scene,
        ms(samples[0].first - samples[0].call)?
    )?;
    let mut chain = None;
    let mut frames = Vec::with_capacity(DRAW_STEPS);
    let mut matched = 0;
    for (brain, shell) in samples.iter().zip(&observed.draw_steps) {
        let previous = brain
            .key
            .index
            .checked_sub(1)
            .map(|index| &observed.draw_steps[index]);
        let Some(active) = write_step(&mut report, brain, shell, previous, frequency)? else {
            continue;
        };
        let endpoint = endpoint(
            observed,
            shell,
            presents,
            dock,
            frequency,
            &mut chain,
            &mut frames,
        );
        match endpoint {
            Ok(row) => {
                let paint = shell.paint.context("matched step has no confirmed paint")?;
                write_endpoint(&mut report, brain, active, paint.qpc, row, frequency)?;
                matched += 1;
            }
            Err(reason) => writeln!(report, " status=unmatched:{}", reason.label())?,
        }
    }
    writeln!(report, "matched={matched}/{DRAW_STEPS}")?;
    fs::write(output.join("draw-timing.txt"), &report)?;
    print!("{report}");
    ensure!(
        matched == DRAW_STEPS,
        "one or more draw steps have no exact present match"
    );
    Ok(())
}

fn write_step(
    report: &mut String,
    brain: &BrainStep,
    shell: &DrawStep,
    previous: Option<&DrawStep>,
    frequency: i64,
) -> Result<Option<i64>> {
    ensure!(
        shell.key.is_none_or(|key| key == brain.key),
        "brain and shell draw identifiers differ"
    );
    let ms = |ticks| milliseconds(ticks, frequency);
    let source = if brain.key.index == 0 {
        "recorded"
    } else {
        "staged-synthetic"
    };
    write!(
        report,
        "step={} source={source} argument_bytes={}",
        brain.key.index, brain.bytes
    )?;
    for (name, ticks) in [
        ("first_argument_to_close", brain.close - brain.first),
        ("close_to_durable", brain.durable - brain.close),
        ("durable_to_pipe_queue", brain.pipe - brain.durable),
    ] {
        write!(report, " {name}_ms={}", ms(ticks)?)?;
    }
    let Some(ready) = shell.ready else {
        writeln!(report, " status=unmatched:MissingReady")?;
        return Ok(None);
    };
    ensure!(brain.pipe <= ready, "shell ready precedes its pipe commit");
    ensure!(
        previous
            .and_then(|step| step.ready)
            .is_none_or(|earlier| earlier <= ready),
        "draw readiness reordered"
    );
    write!(
        report,
        " durable_to_ready_ms={} pipe_queue_to_ready_ms={}",
        ms(ready - brain.durable)?,
        ms(ready - brain.pipe)?
    )?;
    let Some(active) = shell.active else {
        writeln!(report, " status=unmatched:MissingActive")?;
        return Ok(None);
    };
    write!(
        report,
        " ready_to_active_ms={} segment_ms={} independent_entrance_ms={}",
        ms(active.qpc - ready)?,
        active.segment_ms,
        active.entrance_ms
    )?;
    if let Some(earlier) = previous.and_then(|step| step.active) {
        ensure!(earlier.qpc <= active.qpc, "draw activations reordered");
        if brain.key.index > 1
            && let Some(paint) = previous.and_then(|step| step.paint)
        {
            ensure!(
                paint.qpc <= active.qpc,
                "draw activated before previous confirmed paint"
            );
            let eligible = narration_overlap(
                paint.qpc,
                earlier.entrance_ms,
                earlier.segment_ms,
                ready,
                active.qpc,
                frequency,
            )?;
            let share = narration_share(eligible, active.qpc - ready);
            write!(
                report,
                " estimated_narration_overlap_ms={} estimated_narration_share_pct={share}",
                ms(eligible)?
            )?;
        }
    }
    if let Some(paint) = shell.paint {
        write!(
            report,
            " overlay_hwnd={:#x} frame={} paint_qpc={} active_to_paint_ms={}",
            paint.hwnd,
            paint.frame,
            paint.qpc,
            ms(paint.qpc - active.qpc)?
        )?;
        if brain.key.index == 0 {
            write!(
                report,
                " call_to_first_paint_ms={}",
                ms(paint.qpc - brain.call)?
            )?;
        }
    }
    Ok(Some(active.qpc))
}

fn write_endpoint(
    report: &mut String,
    brain: &BrainStep,
    active: i64,
    paint: i64,
    row: MatchedPresent,
    frequency: i64,
) -> Result<()> {
    let Source::Csv { row: csv } = row.source else {
        anyhow::bail!("matched step has no CSV row");
    };
    let present = row.present.qpc;
    let ms = |ticks| milliseconds(ticks, frequency);
    write!(
        report,
        " status=matched swapchain={:#x} csv_row={csv} present_qpc={present} paint_to_present_ms={} active_to_present_ms={} first_argument_to_present_ms={}",
        row.present.swapchain,
        ms(present - paint)?,
        ms(present - active)?,
        ms(present - brain.first)?,
    )?;
    if brain.key.index == 0 {
        write!(
            report,
            " call_to_first_present_ms={}",
            ms(present - brain.call)?
        )?;
    }
    if let Some(displayed) = row.displayed_qpc {
        writeln!(
            report,
            " displayed_qpc={displayed} present_to_display_ms={}",
            ms(displayed - present)?
        )?;
    } else {
        writeln!(report, " present_to_display_ms=NA")?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Unmatched {
    MissingPaint,
    MissingDrawn,
    ReusedFrame,
    CompetingPanelPaint,
    EstimatedSeed,
    SwapchainChanged,
    Present(Join),
}

impl Unmatched {
    fn label(&self) -> String {
        match self {
            Self::Present(join) => format!("{join:?}"),
            other => format!("{other:?}"),
        }
    }
}

fn endpoint(
    observed: &Observation,
    shell: &DrawStep,
    presents: &mut PresentRows,
    dock: u64,
    frequency: i64,
    chain: &mut Option<u64>,
    frames: &mut Vec<u64>,
) -> std::result::Result<MatchedPresent, Unmatched> {
    let paint = shell.paint.ok_or(Unmatched::MissingPaint)?;
    let drawn = observed
        .frame(paint.frame)
        .and_then(|frame| frame.completion_qpc)
        .ok_or(Unmatched::MissingDrawn)?;
    if frames.contains(&paint.frame) {
        return Err(Unmatched::ReusedFrame);
    }
    frames.push(paint.frame);
    if observed
        .frames
        .iter()
        .any(|frame| frame.kind == Kind::Panel && (paint.qpc..=drawn).contains(&frame.paint_qpc))
    {
        return Err(Unmatched::CompetingPanelPaint);
    }
    let candidate = presents
        .non_dock_swapchain(dock, paint.qpc, drawn, frequency)
        .map_err(Unmatched::Present)?;
    if chain.is_some_and(|known| known != candidate) {
        return Err(Unmatched::SwapchainChanged);
    }
    let matched = presents
        .join_present(candidate, paint.qpc, drawn, frequency)
        .map_err(Unmatched::Present)?;
    if !matches!(matched.source, Source::Csv { .. }) {
        return Err(Unmatched::EstimatedSeed);
    }
    *chain = Some(candidate);
    Ok(matched)
}

fn narration_overlap(
    previous_paint: i64,
    entrance_ms: u64,
    segment_ms: u64,
    ready: i64,
    active: i64,
    frequency: i64,
) -> Result<i64> {
    let from_previous =
        |ms| i128::from(previous_paint) + i128::from(ms) * i128::from(frequency) / 1000;
    let start = i128::from(ready).max(from_previous(entrance_ms));
    let end = i128::from(active).min(from_previous(segment_ms));
    Ok(i64::try_from((end - start).max(0))?)
}

fn narration_share(eligible: i64, wait: i64) -> String {
    if wait == 0 {
        return "NA".to_owned();
    }
    let share = i128::from(eligible) * 10_000 / i128::from(wait);
    format!("{}.{:02}", share / 100, share % 100)
}
