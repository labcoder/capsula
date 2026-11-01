//! Bounded release idle CPU and private-memory measurements on Rez.
#![expect(
    clippy::print_stdout,
    reason = "benchmark results are command-line output"
)]

use std::path::PathBuf;
use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use capsula_protocol::constants::{BRAIN_EXE, SHELL_EXE};

use crate::platform::{self, Process};

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    ensure!(
        args.next().as_deref() == Some("idle"),
        "use cargo xtask bench idle [--duration 1..60]"
    );
    let seconds = match args.next().as_deref() {
        None => 60,
        Some("--duration") => args
            .next()
            .context("--duration needs seconds")?
            .parse::<u64>()?,
        Some(_) => bail!("use cargo xtask bench idle [--duration 1..60]"),
    };
    ensure!(
        (1..=60).contains(&seconds) && args.next().is_none(),
        "duration must be 1..60 seconds with no extra arguments"
    );
    let exe = std::env::current_exe()?;
    let release: PathBuf = exe
        .parent()
        .and_then(|path| path.parent())
        .context("xtask executable is not inside a Cargo profile directory")?
        .join("release");
    let shell = release.join(SHELL_EXE);
    ensure!(
        shell.is_file() && release.join(BRAIN_EXE).is_file(),
        "build both release binaries before benchmarking"
    );
    let existing = platform::processes().context("checking for existing product processes")?;
    ensure!(
        existing.is_empty(),
        "refusing to run: existing product process PIDs {:?}",
        existing.iter().map(|entry| entry.pid).collect::<Vec<_>>()
    );

    let mut child = platform::launch(&shell, (seconds + 40) * 1000)
        .context("launching release shell at normal priority")?;
    let deadline = Instant::now() + Duration::from_secs(seconds + 60);
    let mut tracked = Vec::new();
    let measurement = measure(child.id(), seconds, &mut tracked);
    let cleanup = wait_for_exit(&mut child, &tracked, deadline);
    match (measurement, cleanup) {
        (Err(measurement), Err(cleanup)) => {
            bail!("measurement failed: {measurement:#}; cleanup failed: {cleanup:#}")
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn measure(shell_pid: u32, seconds: u64, tracked: &mut Vec<(&'static str, Process)>) -> Result<()> {
    sleep(Duration::from_secs(30));
    tracked.push(("shell", Process::open(shell_pid)?));
    let entries = platform::processes()?;
    let brain = entries
        .iter()
        .find(|entry| !entry.shell && entry.parent == shell_pid)
        .context("shell did not start a brain within the 30s settle period")?;
    tracked.push(("brain", Process::open(brain.pid)?));
    let worker = entries
        .iter()
        .find(|entry| !entry.shell && entry.parent == brain.pid)
        .context("brain did not start a screen worker within the 30s settle period")?;
    tracked.push(("screen-worker", Process::open(worker.pid)?));
    ensure!(
        entries.len() == 3,
        "product process set changed during startup; refusing a contaminated measurement"
    );
    let start = Instant::now();
    let before = tracked
        .iter()
        .map(|(_, process)| process.sample())
        .collect::<Result<Vec<_>>>()?;
    sleep(Duration::from_secs(seconds));
    let after = tracked
        .iter()
        .map(|(_, process)| process.sample())
        .collect::<Result<Vec<_>>>()?;
    let wall = start.elapsed();
    let final_entries = platform::processes()?;
    ensure!(
        final_entries.len() == 3
            && final_entries
                .iter()
                .all(|entry| tracked.iter().any(|(_, process)| process.pid == entry.pid)),
        "product process set changed during measurement"
    );
    println!(
        "idle release: 30s settle; sequential endpoint counters included in wall; user+kernel CPU / wall; 100% = one core; private bytes (not peak)"
    );
    println!("role pid wall_s cpu_s cpu_% private_start_B private_end_B");
    let mut total_cpu = Duration::ZERO;
    let mut total_before = 0;
    let mut total_after = 0;
    for (((role, process), before), after) in tracked.iter().zip(&before).zip(&after) {
        let cpu = after
            .cpu
            .checked_sub(before.cpu)
            .context("process CPU counter went backwards")?;
        total_cpu += cpu;
        total_before += before.private_bytes;
        total_after += after.private_bytes;
        println!(
            "{role} {} {:.3} {:.6} {:.3} {} {}",
            process.pid,
            wall.as_secs_f64(),
            cpu.as_secs_f64(),
            percent(cpu, wall),
            before.private_bytes,
            after.private_bytes
        );
    }
    println!(
        "total - {:.3} {:.6} {:.3} {total_before} {total_after}",
        wall.as_secs_f64(),
        total_cpu.as_secs_f64(),
        percent(total_cpu, wall)
    );
    Ok(())
}

fn wait_for_exit(child: &mut Child, tracked: &[(&str, Process)], deadline: Instant) -> Result<()> {
    loop {
        let shell = child.try_wait()?;
        let codes = tracked
            .iter()
            .map(|(_, process)| process.exit_code())
            .collect::<Result<Vec<_>>>()?;
        if let Some(status) = shell
            && codes.iter().all(Option::is_some)
        {
            println!("shell exit: {status}");
            for ((role, process), code) in tracked.iter().zip(&codes) {
                if let Some(code) = code {
                    println!("{role} pid {} exit: {code}", process.pid);
                }
            }
            ensure!(status.success(), "shell exited unsuccessfully: {status}");
            for ((role, process), code) in tracked.iter().zip(&codes) {
                if *role == "screen-worker" {
                    ensure!(
                        *code == Some(1),
                        "screen worker pid {} exit {code:?}; expected job termination code 1",
                        process.pid
                    );
                } else {
                    ensure!(
                        *code == Some(0),
                        "{role} pid {} exited unsuccessfully: {code:?}",
                        process.pid
                    );
                }
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let running = tracked
                .iter()
                .zip(&codes)
                .filter_map(|((role, process), code)| {
                    code.is_none().then_some((*role, process.pid))
                })
                .collect::<Vec<_>>();
            bail!(
                "autoquit/cleanup deadline exceeded; shell pid {} exited={}; still running {running:?}; no process was force-killed",
                child.id(),
                shell.is_some()
            );
        }
        sleep(Duration::from_millis(100));
    }
}

fn percent(cpu: Duration, wall: Duration) -> f64 {
    cpu.as_secs_f64() / wall.as_secs_f64() * 100.0
}

#[cfg(test)]
mod tests {
    use super::percent;
    use std::time::Duration;

    #[test]
    fn cpu_percent_is_of_one_core_not_the_machine() {
        assert!(
            (percent(Duration::from_millis(300), Duration::from_secs(60)) - 0.5).abs()
                < f64::EPSILON,
            "300ms over 60s is 0.5% of one core"
        );
        assert!(
            (percent(Duration::from_secs(120), Duration::from_secs(60)) - 200.0).abs()
                < f64::EPSILON,
            "two CPU seconds per wall second is 200% of one core"
        );
    }
}
