//! Bounded release idle CPU, cycles, DWM, private-memory and settled dock-paint measurements on
//! Rez.
#![expect(
    clippy::print_stdout,
    reason = "benchmark results are command-line output"
)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use kumochi_protocol::constants::{BRAIN_EXE, SHELL_EXE};

use crate::platform::{self, Process};

#[derive(Debug)]
pub(crate) struct GateCpu {
    pub(crate) cycles_percent: f64,
    pub(crate) time_percent: f64,
    pub(crate) private_bytes: usize,
}

pub(crate) fn gate_cpu(
    shell_pid: u32,
    metrics: &Path,
    cycle_rate: f64,
    sample: Duration,
    guard: impl Fn() -> Result<()>,
) -> Result<GateCpu> {
    ensure!(
        cycle_rate.is_finite() && cycle_rate > 0.0,
        "invalid gate cycle rate"
    );
    ensure!(
        matches!(sample.as_secs(), 30 | 60) && sample.subsec_nanos() == 0,
        "gate sample must be30s stretch or60s idle"
    );
    let seconds = sample.as_secs();
    guarded_wait(Duration::from_secs(30), &guard)?;
    let mut tracked = Vec::new();
    track(shell_pid, &mut tracked)?;
    let mut dwm = platform::capture_dwm(seconds + 1, &metrics.join("dwm.csv"))?;
    let deadline = Instant::now() + Duration::from_secs(seconds + 5);
    let measured = interval(seconds, &tracked, cycle_rate, &guard);
    let captured = (|| -> Result<()> {
        let mut failed_guard = None;
        loop {
            if let Some(status) = dwm.try_wait()? {
                ensure!(status.success(), "DWM capture failed: {status}");
                break;
            }
            if failed_guard.is_none() {
                failed_guard = guard().err();
            }
            ensure!(
                Instant::now() < deadline,
                "DWM capture exceeded its fixed sample bound"
            );
            sleep(Duration::from_millis(10));
        }
        if let Some(error) = failed_guard {
            return Err(error);
        }
        let data = fs::read(metrics.join("dwm.csv"))?;
        let rows = data
            .split(|byte| *byte == b'\n')
            .filter(|line| line.starts_with(b"\""))
            .count();
        ensure!(
            u64::try_from(rows)? == seconds + 2,
            "DWM capture lacks header and raw samples: {rows}"
        );
        println!(
            "DWM retains the invalid first sample; timestamps define its wider whole-desktop window, not product attribution"
        );
        Ok(())
    })();
    match (measured, captured) {
        (Ok(cpu), Ok(())) => Ok(cpu),
        (measured, captured) => bail!(
            "CPU: {:?}; DWM capture: {:?}",
            measured.err(),
            captured.err()
        ),
    }
}

fn guarded_wait(duration: Duration, guard: &impl Fn() -> Result<()>) -> Result<()> {
    let end = Instant::now() + duration;
    loop {
        guard()?;
        let Some(left) = end.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        sleep(left.min(Duration::from_millis(10)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    Idle,
    Panel,
}

fn options(mut args: impl Iterator<Item = String>) -> Result<(u64, Option<PathBuf>, Workload)> {
    let mut workload = match args.next().as_deref() {
        Some("idle") => Workload::Idle,
        Some("stretch") => bail!("motion trials return with the pull (C2a)"),
        _ => bail!(
            "use cargo xtask bench idle [--duration 1..60] [--baseline <release-directory> | --panel]"
        ),
    };
    let mut duration = None;
    let mut baseline = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--duration" if duration.is_none() => {
                let seconds = args
                    .next()
                    .context("--duration needs seconds")?
                    .parse::<u64>()?;
                ensure!(
                    (1..=60).contains(&seconds),
                    "duration must be 1..60 seconds"
                );
                duration = Some(seconds);
            }
            "--baseline" if baseline.is_none() => {
                baseline = Some(PathBuf::from(
                    args.next()
                        .context("--baseline needs a release directory")?,
                ));
            }
            "--panel" if workload == Workload::Idle => workload = Workload::Panel,
            _ => bail!("unknown or duplicate benchmark option"),
        }
    }
    ensure!(
        !(workload == Workload::Panel && baseline.is_some()),
        "--panel requires the current shell"
    );
    Ok((duration.unwrap_or(60), baseline, workload))
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let (seconds, baseline, workload) = options(args)?;
    let exe = std::env::current_exe()?;
    let release: PathBuf = exe
        .parent()
        .and_then(|path| path.parent())
        .context("xtask executable is not inside a Cargo profile directory")?
        .join("release");
    let release = baseline.as_ref().unwrap_or(&release);
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

    // A baseline shell may predate the settled-interval count, so only the current one is checked.
    let paints = if baseline.is_none() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            std::env::temp_dir().join(format!("idle-bench-{}-{nonce}.txt", std::process::id()));
        println!("dock paint diagnostics: {}", path.display());
        Some(path)
    } else {
        None
    };
    let mut extra: Vec<OsString> = Vec::new();
    if workload == Workload::Panel {
        extra.push("--smoke".into());
    }
    if let Some(path) = &paints {
        let mut diagnostics = OsString::from("--input-diagnostics=");
        diagnostics.push(path);
        extra.push(diagnostics);
        extra.push(format!("--settle-after-ms={}", SETTLE.as_millis()).into());
    }
    println!("workload: {workload:?}; baseline: {}", baseline.is_some());
    let cycle_rate = calibrate_rate()?;
    let mut child = platform::launch(&shell, (seconds + 40) * 1000, extra)
        .context("launching release shell at normal priority")?;
    let deadline = Instant::now() + Duration::from_secs(seconds + 60);
    let mut tracked = Vec::new();
    let measurement = measure(child.id(), seconds, &mut tracked, None, cycle_rate);
    let close = if baseline.is_some() {
        platform::close_shell_window(&mut child).context("closing baseline shell window")
    } else {
        Ok(())
    };
    let cleanup = wait_for_exit(&mut child, &tracked, deadline);
    let errors = [measurement, close, cleanup]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    if let Some(path) = paints {
        let diagnostics = fs::read_to_string(&path)?;
        fs::remove_file(&path)?;
        let settled = settled_paints(&diagnostics)?;
        for line in diagnostics.lines().filter(|line| line.contains(" kumo ")) {
            println!("{line}");
        }
        println!(
            "dock paints after the {}s settle, until Quit: {settled}",
            SETTLE.as_secs()
        );
        ensure!(settled == 0, "the dock painted {settled} frames at rest");
    }
    Ok(())
}

/// The shell's settled interval starts with the sample, after the same settle `measure` waits.
const SETTLE: Duration = Duration::from_secs(30);

/// The dock frames the shell counted from the settled interval's start until Quit.
fn settled_paints(diagnostics: &str) -> Result<u64> {
    ensure!(
        diagnostics
            .lines()
            .any(|line| line.ends_with(" dock settled")),
        "the shell never started its settled interval"
    );
    Ok(diagnostics
        .lines()
        .find_map(|line| {
            line.split_once(" dock paints settled=")
                .map(|(_, count)| count)
        })
        .context("the shell did not report its settled dock paints")?
        .parse::<u64>()?)
}

fn measure(
    shell_pid: u32,
    seconds: u64,
    tracked: &mut Vec<(&'static str, Process)>,
    metrics: Option<&Path>,
    cycle_rate: f64,
) -> Result<()> {
    sleep(Duration::from_secs(30));
    track(shell_pid, tracked)?;
    let dwm = if let Some(path) = metrics {
        Some(
            platform::capture_dwm(seconds + 1, &path.join("dwm.csv"))
                .context("starting bounded whole-desktop DWM counters")?,
        )
    } else {
        None
    };
    let measurement = interval(seconds, tracked, cycle_rate, &|| Ok(())).map(|_| ());
    let capture = (|| -> Result<()> {
        if let Some(mut child) = dwm {
            let status = child
                .wait()
                .context("waiting for bounded DWM counter capture")?;
            ensure!(status.success(), "DWM counter capture failed: {status}");
            let data = fs::read(
                metrics
                    .context("missing DWM output directory")?
                    .join("dwm.csv"),
            )?;
            let rows = data
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"\""))
                .count();
            ensure!(
                u64::try_from(rows)? == seconds + 2,
                "DWM CSV lacks the expected header and samples: {rows} rows"
            );
            println!(
                "DWM CSV retains all 1Hz rows and PIDs, including the first invalid warmup value; its bounded window extends beyond the CPU interval; use CSV timestamps; whole-desktop activity, not Kumochi attribution"
            );
        }
        Ok(())
    })();
    let errors = [measurement, capture]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn track(shell_pid: u32, tracked: &mut Vec<(&'static str, Process)>) -> Result<()> {
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
    Ok(())
}

fn interval(
    seconds: u64,
    tracked: &[(&str, Process)],
    cycle_rate: f64,
    guard: &impl Fn() -> Result<()>,
) -> Result<GateCpu> {
    guard()?;
    let start = Instant::now();
    let before = tracked
        .iter()
        .map(|(_, process)| process.sample())
        .collect::<Result<Vec<_>>>()?;
    guarded_wait(Duration::from_secs(seconds), guard)?;
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
        "release: 30s warmup; sequential endpoint counters included in wall; GetProcessTimes user+kernel CPU / wall; QueryProcessCycleTime raw cycles and cycles_cpu_% = cycles/(measured_rate*wall)*100; 100% = one calibrated core; private bytes (not peak)"
    );
    println!("role pid wall_s cpu_s cpu_% cycles cycles_cpu_% private_start_B private_end_B");
    let mut total_cpu = Duration::ZERO;
    let mut total_before = 0;
    let mut total_after = 0;
    let mut total_cycles = 0;
    for (((role, process), before), after) in tracked.iter().zip(&before).zip(&after) {
        let cpu = after
            .cpu
            .checked_sub(before.cpu)
            .context("process CPU counter went backwards")?;
        total_cpu += cpu;
        let cycles = after
            .cycles
            .checked_sub(before.cycles)
            .context("process cycle counter went backwards")?;
        total_cycles += cycles;
        total_before += before.private_bytes;
        total_after += after.private_bytes;
        println!(
            "{role} {} {:.3} {:.6} {:.3} {cycles} {:.3} {} {}",
            process.pid,
            wall.as_secs_f64(),
            cpu.as_secs_f64(),
            percent(cpu, wall),
            cycle_percent(cycles, cycle_rate, wall),
            before.private_bytes,
            after.private_bytes
        );
    }
    println!(
        "total - {:.3} {:.6} {:.3} {total_cycles} {:.3} {total_before} {total_after}",
        wall.as_secs_f64(),
        total_cpu.as_secs_f64(),
        percent(total_cpu, wall),
        cycle_percent(total_cycles, cycle_rate, wall)
    );
    Ok(GateCpu {
        cycles_percent: cycle_percent(total_cycles, cycle_rate, wall),
        time_percent: percent(total_cpu, wall),
        private_bytes: total_after,
    })
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

fn calibrate_rate() -> Result<f64> {
    gate_calibrate(|| Ok(()))
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "bounded two-second cycle deltas fit exactly in f64"
)]
pub(crate) fn gate_calibrate(guard: impl Fn() -> Result<()>) -> Result<f64> {
    let mut highest = 0.0_f64;
    for trial in 1..=3 {
        let start = Instant::now();
        let before = platform::thread_cycles()?;
        let mut next_guard = Duration::ZERO;
        while start.elapsed() < Duration::from_secs(2) {
            if start.elapsed() >= next_guard {
                guard()?;
                next_guard = start.elapsed() + Duration::from_millis(10);
            }
            std::hint::spin_loop();
        }
        let cycles = platform::thread_cycles()?
            .checked_sub(before)
            .context("calibration thread cycle counter went backwards")?;
        let wall = start.elapsed();
        let rate = cycles as f64 / wall.as_secs_f64();
        ensure!(
            rate.is_finite() && rate > 0.0,
            "invalid measured cycle rate"
        );
        highest = highest.max(rate);
        println!(
            "calibration trial={trial} wall_s={:.6} cycles={cycles} cycles_per_s={rate:.3}",
            wall.as_secs_f64()
        );
    }
    println!(
        "cycle_rate_per_s={highest:.3}; highest of three saturated-thread 2s samples; calibrated cycles CPU is the slice1 gate; GetProcessTimes retained beside it"
    );
    Ok(highest)
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "bounded process cycle deltas are well within f64 precision"
)]
fn cycle_percent(cycles: u64, rate: f64, wall: Duration) -> f64 {
    cycles as f64 / (rate * wall.as_secs_f64()) * 100.0
}

#[cfg(test)]
mod tests {
    use super::{Workload, cycle_percent, options, percent, settled_paints};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn baseline_and_duration_options_combine_in_either_order() {
        assert_eq!(
            options(["idle".to_owned()].into_iter()).unwrap(),
            (60, None, Workload::Idle),
            "default duration and binary location remain unchanged"
        );
        for args in [
            vec!["idle", "--baseline", "release", "--duration", "1"],
            vec!["idle", "--duration", "1", "--baseline", "release"],
        ] {
            assert_eq!(
                options(args.into_iter().map(str::to_owned)).unwrap(),
                (1, Some(PathBuf::from("release")), Workload::Idle),
                "combined options"
            );
        }
        for args in [
            vec!["idle", "--duration", "0"],
            vec!["idle", "--baseline"],
            vec!["idle", "--duration", "61"],
            vec!["idle", "--baseline", "a", "--baseline", "b"],
            vec!["idle", "--baseline", "a", "--panel"],
        ] {
            assert!(
                options(args.into_iter().map(str::to_owned)).is_err(),
                "invalid options must fail"
            );
        }
        assert_eq!(
            options(["idle", "--panel"].into_iter().map(str::to_owned)).unwrap(),
            (60, None, Workload::Panel)
        );
    }

    #[test]
    fn a_motion_trial_waits_for_the_pull() {
        let error = options(["stretch"].into_iter().map(str::to_owned)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "motion trials return with the pull (C2a)"
        );
    }

    #[test]
    fn the_settled_count_needs_the_interval_and_the_quit_line() {
        let log = "qpc=1 resting\nqpc=2 dock settled\nqpc=9 dock paints settled=0\n";
        assert_eq!(settled_paints(log).unwrap(), 0);
        assert_eq!(
            settled_paints("qpc=2 dock settled\nqpc=9 dock paints settled=3").unwrap(),
            3
        );
        for invalid in [
            "",
            "qpc=9 dock paints settled=0",
            "qpc=2 dock settled",
            "qpc=2 dock settled\nqpc=9 dock paints settled=",
            "qpc=2 dock settled\nqpc=9 dock paints settled=-1",
        ] {
            assert!(settled_paints(invalid).is_err(), "{invalid:?}");
        }
    }

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

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the requested calibration example is exactly representable as 25.0"
    )]
    fn calibrated_cycles_are_percent_of_one_measured_core() {
        assert_eq!(
            cycle_percent(2_000_000_000, 4_000_000_000.0, Duration::from_secs(2)),
            25.0
        );
    }
}
