//! Strict brain receipts and draw-step timing joins.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::LIVE_CHATGPT_TEXT_STAMPS_FILE;

use crate::gate::observation::{Active, DRAW_STEPS, DrawKey, Observation, milliseconds};

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

fn parse(text: &str, pid: u32) -> Result<Vec<BrainStep>> {
    ensure!(text.ends_with('\n'), "truncated brain draw receipt");
    let mut lines = text.lines();
    let ready = header(lines.next(), pid)?;
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
                    ready,
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
    observed: &Observation,
) -> Result<()> {
    let text = read(File::open(stamps).context("missing brain draw receipt")?)?;
    let samples = parse(&text, brain_pid)?;
    let frequency = observed
        .startup
        .context("missing draw observer startup")?
        .frequency;
    let ms = |ticks| milliseconds(ticks, frequency);
    let mut report = String::from(
        "Offline Responses drawing: step 0 uses the checked-in capture; steps 1–3 are synthetic in the same call and scene.\n\
         The timing of record is first step byte to accepted paint, the first paint record for that step; present not matched.\n\
         Step 3 has an authored after-delay of 200 ms.\n\
         If a narrated predecessor reserves time beyond its entrance, the report estimates overlap with that extra time. It does not establish what caused the wait.\n",
    );
    writeln!(
        report,
        "run={} scene={} call_to_first_step_ms={}",
        samples[0].key.run,
        samples[0].key.scene,
        ms(samples[0].first - samples[0].call)?
    )?;
    let mut previous: Option<(i64, Active, i64)> = None;
    for (brain, shell) in samples.iter().zip(&observed.draw_steps) {
        ensure!(
            shell.key == Some(brain.key),
            "brain and shell draw identifiers differ or are missing"
        );
        let ready = shell.ready.context("missing draw ready stamp")?;
        let active = shell.active.context("missing draw active stamp")?;
        let paint = shell.paint.context("missing draw paint stamp")?;
        ensure!(brain.pipe <= ready, "shell ready precedes its pipe commit");
        let source = if brain.key.index == 0 {
            "recorded"
        } else {
            "staged-synthetic"
        };
        write!(
            report,
            "step={} source={source} argument_bytes={} segment_ms={} independent_entrance_ms={} narrated={} accepted_paint_qpc={paint}",
            brain.key.index,
            brain.bytes,
            active.segment_ms,
            active.entrance_ms,
            u8::from(active.narrated)
        )?;
        for (name, ticks) in [
            ("first_argument_to_close", brain.close - brain.first),
            ("close_to_durable", brain.durable - brain.close),
            ("durable_to_pipe_queue", brain.pipe - brain.durable),
            ("pipe_queue_to_ready", ready - brain.pipe),
            ("ready_to_active", active.qpc - ready),
            ("active_to_paint", paint - active.qpc),
            ("first_step_byte_to_accepted_paint", paint - brain.first),
        ] {
            write!(report, " {name}_ms={}", ms(ticks)?)?;
        }
        if let Some((prior_ready, prior, prior_paint)) = previous {
            ensure!(
                prior_ready <= ready && prior.qpc <= active.qpc && prior_paint <= paint,
                "shell draw step timestamps reordered"
            );
            if prior.narrated && prior.segment_ms > prior.entrance_ms {
                let overlap = narration_overlap(
                    prior_paint,
                    prior.entrance_ms,
                    prior.segment_ms,
                    ready,
                    active.qpc,
                    frequency,
                )?;
                let share = narration_share(overlap, active.qpc - ready);
                write!(
                    report,
                    " estimated_narration_overlap_ms={} estimated_narration_share_pct={share}",
                    ms(overlap)?
                )?;
            }
        } else {
            write!(
                report,
                " call_to_first_paint_ms={}",
                ms(paint - brain.call)?
            )?;
        }
        writeln!(report)?;
        previous = Some((ready, active, paint));
    }
    writeln!(report, "painted={DRAW_STEPS}/{DRAW_STEPS}")?;
    fs::write(output.join(LIVE_CHATGPT_TEXT_STAMPS_FILE), text)?;
    fs::write(output.join("draw-timing.txt"), &report)?;
    print!("{report}");
    Ok(())
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
