use std::{io::Read as _, path::Path};

use anyhow::{Result, anyhow, bail, ensure};
use kumochi_protocol::message::RunId;

pub(super) struct Attempt {
    pub(super) run: RunId,
    pub(super) target: u64,
    pub(super) reserved: u64,
    pub(super) text: Option<u64>,
    key: u64,
    ended: bool,
}

#[derive(Default)]
pub(super) struct Ledger {
    pub(super) attempts: Vec<Attempt>,
    pub(super) auxiliary: [u64; 5],
    pub(super) finished: bool,
}

pub(super) fn read(path: &Path, pid: u32, ceiling: u32) -> Result<Ledger> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(32_769).read_to_end(&mut bytes))
        .map_err(|_| anyhow!("cannot read stamp file"))?;
    parse(&bytes, pid, ceiling)
}

pub(super) fn parse(bytes: &[u8], pid: u32, ceiling: u32) -> Result<Ledger> {
    ensure!(
        bytes.len() <= 32_768 && bytes.ends_with(b"\n"),
        "incomplete stamp file"
    );
    let mut tally = Ledger::default();
    let mut latest = 0;
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    for line in text.split_terminator('\n') {
        let (kind, fields) = line.split_once(' ').ok_or_else(invalid)?;
        let fields: Vec<u64> = fields
            .split(' ')
            .map(|field| {
                ensure!(
                    field.bytes().all(|byte| byte.is_ascii_digit()),
                    "invalid stamp field"
                );
                field.parse().map_err(|_| invalid())
            })
            .collect::<Result<_>>()?;
        let Some((&at, [owner, data @ ..])) = fields.split_last() else {
            return Err(invalid());
        };
        ensure!(
            !tally.finished && *owner == u64::from(pid) && at > 0 && at >= latest,
            "stamp ownership or order mismatch"
        );
        let n = u64::try_from(tally.attempts.len())?;
        let aux = tally.auxiliary.iter().sum::<u64>();
        match (kind, data) {
            ("ready", [cap]) if latest == 0 && *cap == u64::from(ceiling) => {}
            _ if latest == 0 => return Err(invalid()),
            ("reserve", [ordinal, run, 1, key, 1, target @ (1 | 2), 1])
                if *ordinal == n + 1
                    && *ordinal <= u64::from(ceiling)
                    && *run > 0
                    && *key > 0
                    && tally
                        .attempts
                        .last()
                        .is_none_or(|previous| *key > previous.key)
                    && tally.attempts.iter().all(|a| a.run.0 != *run && a.ended) =>
            {
                tally.attempts.push(Attempt {
                    run: RunId(*run),
                    target: *target,
                    reserved: at,
                    text: None,
                    key: *key,
                    ended: false,
                });
            }
            ("aux", [ordinal, class @ 1..=5]) if *ordinal == aux + 1 && *ordinal <= 64 => {
                tally.auxiliary[usize::try_from(class - 1)?] += 1;
            }
            ("final", [models, auxiliary])
                if *models == n && *auxiliary == aux && tally.attempts.iter().all(|a| a.ended) =>
            {
                tally.finished = true;
            }
            ("text", [ordinal]) if *ordinal == n => {
                let attempt = tally.attempts.last_mut().ok_or_else(invalid)?;
                ensure!(
                    attempt.text.is_none() && !attempt.ended,
                    "duplicate or late text stamp"
                );
                attempt.text = Some(at);
            }
            ("closed", [ordinal, before])
                if *ordinal == n && *before >= latest && *before <= at =>
            {
                end_attempt(&mut tally)?;
            }
            ("pooled", [ordinal]) if *ordinal == n => end_attempt(&mut tally)?,
            _ => bail!("invalid stamp record"),
        }
        latest = at;
    }
    ensure!(latest != 0, "missing ready record");
    Ok(tally)
}

fn invalid() -> anyhow::Error {
    anyhow!("invalid stamp record")
}

fn end_attempt(tally: &mut Ledger) -> Result<()> {
    let attempt = tally.attempts.last_mut().ok_or_else(invalid)?;
    ensure!(attempt.text.is_some() && !attempt.ended, "wrong stream end");
    attempt.ended = true;
    Ok(())
}
