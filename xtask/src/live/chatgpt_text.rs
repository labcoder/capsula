//! Prepare a disposable Dev first-text copy without starting the brain.

mod startup;
#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Result, anyhow, bail, ensure};
use kumochi_protocol::constants::FIXTURE_ENV;

const PLAN: &str = "chatgpt-text.plan";

pub(super) fn budget(words: &[String]) -> Result<u32> {
    let [phase, flag, value] = words else {
        bail!("expected first-text --budget N")
    };
    ensure!(
        phase == "first-text" && flag == "--budget" && value.bytes().all(|c| c.is_ascii_digit()),
        "expected first-text --budget N"
    );
    let budget = value.parse().map_err(|_| anyhow!("invalid budget"))?;
    ensure!((1..=60).contains(&budget), "budget must be 1–60");
    Ok(budget)
}

pub(super) fn plan(budget: u32) -> Result<String> {
    ensure!(
        std::env::var_os(FIXTURE_ENV).is_none(),
        "fixture engine override is set"
    );
    startup::plan(budget)
}

pub(super) fn prepared(folder: &Path, plan: &str) -> Result<()> {
    std::fs::write(folder.join(PLAN), plan)?;
    println!("prepared first-text; no brain was started\n{plan}");
    Ok(())
}
