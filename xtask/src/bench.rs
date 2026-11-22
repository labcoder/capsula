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
use kumochi_protocol::constants::SHELL_EXE;

use crate::hermetic::{self, Folders, Prebuilt, Profile, Run};
use crate::platform::{self, Process};
use crate::session::{self, Startup, unchanged};

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
    /// Idle with onboarding owed, so the onboarding window is open.
    Onboarding,
}

#[derive(Debug, PartialEq, Eq)]
enum Images {
    Build(Option<PathBuf>),
    Prepared(Prebuilt),
}

fn options(mut args: impl Iterator<Item = String>) -> Result<(u64, Images, Workload, bool)> {
    let mut workload = match args.next().as_deref() {
        Some("idle") => Workload::Idle,
        Some("stretch") => bail!("motion trials return with the pull (C2a)"),
        _ => bail!(
            "use bench idle [--duration 1..60] [--baseline <checkout> | --prebuilt <shell-sha> <brain-sha>] [--panel | --onboarding] [--paint-check]"
        ),
    };
    let mut duration = None;
    let mut baseline = None;
    let mut prepared = None;
    let mut paint_check = false;
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
                    args.next().context("--baseline needs a checkout")?,
                ));
            }
            "--prebuilt" if prepared.is_none() => prepared = Some(Prebuilt::parse(&mut args)?),
            "--panel" if workload == Workload::Idle => workload = Workload::Panel,
            "--onboarding" if workload == Workload::Idle => workload = Workload::Onboarding,
            "--paint-check" if !paint_check => paint_check = true,
            _ => bail!("unknown or duplicate benchmark option"),
        }
    }
    ensure!(
        !(workload == Workload::Panel && baseline.is_some()),
        "--panel requires the current shell"
    );
    ensure!(
        prepared.is_none() || baseline.is_none(),
        "--prebuilt cannot be combined with --baseline"
    );
    ensure!(
        !paint_check || baseline.is_none(),
        "--paint-check cannot be combined with --baseline"
    );
    let images = match prepared {
        Some(pins) => Images::Prepared(pins),
        None => Images::Build(baseline),
    };
    Ok((duration.unwrap_or(60), images, workload, paint_check))
}

/// Measures prepared Gate images, or builds the images before sampling.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let (seconds, images, workload, paint_check) = options(args)?;
    let (baseline, prepared) = match images {
        Images::Prepared(pins) => {
            let release = pins.release()?;
            (None, Some((pins, release)))
        }
        Images::Build(baseline) => (baseline, None),
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .context("xtask is not in a Cargo profile folder")?
        .join("bench")
        .join(stamp.to_string());
    let run = Run::begin(root)?;
    let measured = (|| {
        let folders = run.folders("app")?;
        let paints = paint_check.then(|| folders.local.with_file_name("dock-paints.txt"));
        if workload == Workload::Onboarding {
            folders.owe_onboarding()?;
        }
        if let Some((pins, release)) = prepared {
            let release = pins.stage(&release, &folders)?;
            println!(
                "driver_build_id={} identity={} shell_sha256={} brain_sha256={} build_seconds=0.0",
                kumochi_protocol::BUILD_ID,
                kumochi_protocol::constants::DATA_DIR,
                pins.shell_sha256,
                pins.brain_sha256
            );
            let measured = idle(
                &release.join(SHELL_EXE),
                &folders,
                seconds,
                None,
                workload,
                paints.as_deref(),
            );
            return session::combine([measured, pins.verify(&release)]);
        }
        let (release, build) = hermetic::build(
            &["kumochi-shell", "kumochi-brain"],
            Profile::Release,
            baseline.as_deref(),
        )?;
        println!("build_seconds={:.1}", build.as_secs_f64());
        idle(
            &release.join(SHELL_EXE),
            &folders,
            seconds,
            baseline.as_deref(),
            workload,
            paints.as_deref(),
        )
    })();
    run.finish(measured)
}

fn idle(
    shell: &Path,
    folders: &Folders,
    seconds: u64,
    baseline: Option<&Path>,
    workload: Workload,
    paints: Option<&Path>,
) -> Result<()> {
    if let Some(path) = paints {
        println!("dock paint diagnostics: {}", path.display());
    }
    println!("workload: {workload:?}; baseline: {}", baseline.is_some());
    let cycle_rate = calibrate_rate()?;
    crate::live::refuse_running(&platform::processes()?, std::process::id())?;
    let mut child = platform::launch(
        shell,
        folders,
        (seconds + 40) * 1000,
        idle_args(workload, paints),
    )
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
        let diagnostics = fs::read_to_string(path)?;
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

fn idle_args(workload: Workload, paints: Option<&Path>) -> Vec<OsString> {
    let mut extra = Vec::new();
    if workload == Workload::Panel {
        extra.push("--smoke".into());
    }
    if let Some(path) = paints {
        let mut diagnostics = OsString::from("--input-diagnostics=");
        diagnostics.push(path);
        extra.push(diagnostics);
        extra.push(format!("--settle-after-ms={}", SETTLE.as_millis()).into());
    }
    extra
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
    sleep(SETTLE);
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
    let entries = platform::processes()?;
    let mut opened = entries
        .iter()
        .map(|entry| Ok((entry.pid, Process::open(entry.pid)?)))
        .collect::<Result<Vec<_>>>()?;
    let tree = match session::product_tree(shell_pid, &entries, platform::helper_role)? {
        Startup::Complete(tree) => tree,
        Startup::Missing(role) => bail!("the {role} didn't start within the 30s settle period"),
    };
    for (role, pid) in tree {
        let at = opened
            .iter()
            .position(|(opened_pid, _)| *opened_pid == pid)
            .context("product process disappeared from the startup set")?;
        tracked.push((role, opened.swap_remove(at).1));
    }
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
        unchanged(
            &final_entries,
            &tracked
                .iter()
                .map(|(_, process)| process.pid)
                .collect::<Vec<_>>()
        ),
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
                if matches!(*role, "screen-worker" | "speech-worker") {
                    ensure!(
                        *code == Some(1),
                        "{role} pid {} exit {code:?}; expected job termination code 1",
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
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use kumochi_protocol::constants::{BRAIN_EXE, SHELL_EXE};

    use super::{Images, Workload, cycle_percent, idle_args, options, percent, settled_paints};
    use crate::hermetic::{Folders, Prebuilt};

    #[test]
    fn baseline_and_duration_options_combine_in_either_order() {
        assert_eq!(
            options(["idle".to_owned()].into_iter()).unwrap(),
            (60, Images::Build(None), Workload::Idle, false),
            "default duration and binary location remain unchanged"
        );
        for args in [
            vec!["idle", "--baseline", "release", "--duration", "1"],
            vec!["idle", "--duration", "1", "--baseline", "release"],
        ] {
            assert_eq!(
                options(args.into_iter().map(str::to_owned)).unwrap(),
                (
                    1,
                    Images::Build(Some(PathBuf::from("release"))),
                    Workload::Idle,
                    false
                ),
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
            (60, Images::Build(None), Workload::Panel, false)
        );
        assert_eq!(
            options(
                ["idle", "--onboarding", "--baseline", "a"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .unwrap(),
            (
                60,
                Images::Build(Some(PathBuf::from("a"))),
                Workload::Onboarding,
                false
            )
        );
        assert!(
            options(
                ["idle", "--panel", "--onboarding"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .is_err(),
            "one workload at a time"
        );
    }

    #[test]
    fn current_and_baseline_idle_launch_without_paint_diagnostics() {
        let args = |cli: &[&str]| {
            let (_, _, workload, paint_check) =
                options(cli.iter().map(|arg| (*arg).to_owned())).unwrap();
            idle_args(
                workload,
                paint_check.then_some(Path::new("dock paints.txt")),
            )
        };
        let current = args(&["idle"]);
        let baseline = args(&["idle", "--baseline", "old"]);
        assert_eq!(current, baseline);
        assert!(current.is_empty());
        assert_eq!(
            args(&["idle", "--paint-check"]),
            [
                OsString::from("--input-diagnostics=dock paints.txt"),
                OsString::from("--settle-after-ms=30000"),
            ]
        );
        assert!(
            options(
                ["idle", "--baseline", "old", "--paint-check"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .is_err()
        );
    }

    #[test]
    fn a_timed_bench_refuses_when_prebuilt_pins_are_incomplete() {
        let pin = "1".repeat(64);
        let idle = ["idle".to_owned(), "--prebuilt".to_owned(), pin.clone()];
        let cold = [
            "unused-session.txt".to_owned(),
            "--prebuilt".to_owned(),
            pin,
        ];
        for result in [
            super::run(idle.into_iter()),
            crate::gate::cold(cold.into_iter()),
        ] {
            let error = result.expect_err("an incomplete prebuild cannot run");
            assert_eq!(
                error.to_string(),
                "--prebuilt needs the shell and brain SHA-256 pins"
            );
        }

        let dir = crate::scratch::dir("prebuilt-release-pair");
        let gate = kumochi_protocol::constants::ProductIdentity::GATE.instance_service;
        let shell = format!("shell started|{gate}");
        let brain = format!("brain started|{gate}");
        let mut hashes = [shell.as_bytes(), brain.as_bytes()]
            .into_iter()
            .map(|bytes| {
                crate::platform::gate_capture::sha256_through(bytes, std::io::sink())
                    .expect("hash fixture")
            });
        let prebuilt = Prebuilt::parse(&mut hashes).expect("two SHA-256 pins");
        std::fs::write(dir.join(SHELL_EXE), shell.as_bytes()).expect("write shell fixture");
        assert_eq!(
            prebuilt
                .verify(&dir)
                .expect_err("missing brain")
                .to_string(),
            format!("prebuilt {BRAIN_EXE} is unavailable; no build fallback")
        );
        std::fs::write(dir.join(BRAIN_EXE), b"xyz").expect("write different brain");
        assert_eq!(
            prebuilt
                .verify(&dir)
                .expect_err("changed brain")
                .to_string(),
            format!("prebuilt {BRAIN_EXE} SHA-256 differs from its pin")
        );
        std::fs::write(dir.join(BRAIN_EXE), brain.as_bytes()).expect("write matching brain");
        prebuilt.verify(&dir).expect("the exact pair is ready");
        let folders = Folders::create(&dir.join("owned")).expect("isolated folders");
        let bin = prebuilt
            .stage(&dir, &folders)
            .expect("stage the exact pair");
        std::fs::write(dir.join(BRAIN_EXE), b"xyz").expect("replace the source brain");
        assert_eq!(
            std::fs::read(bin.join(BRAIN_EXE)).expect("read the staged brain"),
            brain.as_bytes()
        );
        let refused = Folders::create(&dir.join("refused")).expect("fresh isolated folders");
        assert_eq!(
            prebuilt
                .stage(&dir, &refused)
                .expect_err("the source changed after the successful preflight")
                .to_string(),
            format!("prebuilt {BRAIN_EXE} SHA-256 differs from its pin")
        );
    }

    #[test]
    fn matching_hashes_cannot_admit_a_non_gate_or_swapped_image() {
        use crate::platform::gate_capture::file_sha256;
        use kumochi_protocol::constants::ProductIdentity;

        let gate = ProductIdentity::GATE.instance_service;
        let release = ProductIdentity::RELEASE.instance_service;
        let dev = ProductIdentity::DEV.instance_service;
        for (shell, brain, wrong) in [
            (
                format!("shell started|{release}"),
                format!("brain started|{gate}"),
                SHELL_EXE,
            ),
            (
                format!("shell started|{gate}"),
                format!("brain started|{dev}"),
                BRAIN_EXE,
            ),
            (
                format!("shell started|{gate}"),
                format!("brain started|{gate}|{release}"),
                BRAIN_EXE,
            ),
            (
                format!("brain started|{gate}"),
                format!("shell started|{gate}"),
                SHELL_EXE,
            ),
        ] {
            let dir = crate::scratch::dir("prebuilt-wrong-identity");
            std::fs::write(dir.join(SHELL_EXE), shell).expect("shell fixture");
            std::fs::write(dir.join(BRAIN_EXE), brain).expect("brain fixture");
            let mut hashes = [SHELL_EXE, BRAIN_EXE]
                .into_iter()
                .map(|name| file_sha256(&dir.join(name)).expect("matching pin"));
            let pins = Prebuilt::parse(&mut hashes).expect("both supplied hashes match");
            let folders = Folders::create(&dir.join("owned")).expect("isolated folders");
            let expected =
                format!("prebuilt {wrong} does not have the expected Gate identity and role");
            assert_eq!(
                pins.stage(&dir, &folders)
                    .expect_err("matching hashes do not establish which app will run")
                    .to_string(),
                expected
            );
            assert_eq!(
                pins.verify(&dir)
                    .expect_err("preflight checks identity")
                    .to_string(),
                expected
            );
        }
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
