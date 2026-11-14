//! Bounded PID-filtered traces: validate a completed warm capture before opening trials.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use kumochi_protocol::constants::{OBJECT_PREFIX, SHELL_EXE};

use crate::platform::{Process, gate_capture};

use super::present::{self, PresentRows};

pub(super) struct Capture {
    child: Option<Child>,
    target: Option<Process>,
    output: PathBuf,
    name: String,
    deadline: Instant,
    input_end: Instant,
    pub(super) dock_swapchain: u64,
}

impl Capture {
    pub(super) fn warm_and_start(
        pid: u32,
        output: &Path,
        duration: Duration,
        mut repaint: impl FnMut() -> Result<()>,
        guard: impl Fn() -> Result<()>,
    ) -> Result<Self> {
        ensure!(
            (10..=70).contains(&duration.as_secs()) && duration.subsec_nanos() == 0,
            "main trace bound must be 10..70 whole seconds"
        );
        guard()?;
        guarded_wait(Duration::from_secs(1), &guard)?;
        let warm_output = output.with_file_name(format!(
            "{}-warm.csv",
            output
                .file_stem()
                .context("trace output lacks a stem")?
                .to_string_lossy()
        ));
        let dock_swapchain = Self::warm_only(pid, &warm_output, &mut repaint, &guard)?;
        guard()?;
        let mut main = Self::start(Some(Process::open(pid)?), output, duration, &guard)?;
        main.dock_swapchain = dock_swapchain;
        // A new trace records nothing for its first ~200 ms, and sometimes longer, so six repaints
        // over 600 ms keep a seed landing after a slow start; a resting dock repaints for free.
        let seeded = (|| -> Result<()> {
            for _ in 0..6 {
                guarded_wait(Duration::from_millis(100), &guard)?;
                repaint()?;
            }
            guarded_wait(Duration::from_millis(100), &guard)
        })();
        if let Err(error) = seeded {
            let cleanup = main.finish(&guard);
            return Err(error.context(format!("main seed cleanup: {:?}", cleanup.err())));
        }
        Ok(main)
    }

    pub(super) fn warm_only(
        pid: u32,
        output: &Path,
        mut repaint: impl FnMut() -> Result<()>,
        guard: &impl Fn() -> Result<()>,
    ) -> Result<u64> {
        let mut warm = Self::start(
            Some(Process::open(pid)?),
            output,
            Duration::from_secs(8),
            guard,
        )?;
        let warmup = (|| -> Result<()> {
            for _ in 0..3 {
                guard()?;
                repaint()?;
                guarded_wait(Duration::from_millis(100), &guard)?;
            }
            guarded_wait(Duration::from_secs(2), &guard)
        })();
        let warm_rows = warm.finish(guard);
        warmup?;
        warm_rows?.warm_swapchain()
    }

    pub(super) fn before_launch(output: &Path, guard: &impl Fn() -> Result<()>) -> Result<Self> {
        Self::start(None, output, Duration::from_secs(15), guard)
    }

    pub(super) fn bind(&mut self, pid: u32) -> Result<()> {
        ensure!(self.target.is_none(), "capture target was already bound");
        self.target = Some(Process::open(pid)?);
        Ok(())
    }

    pub(super) fn input_deadline(&self) -> Instant {
        self.input_end
    }

    fn start(
        target: Option<Process>,
        output: &Path,
        duration: Duration,
        guard: &impl Fn() -> Result<()>,
    ) -> Result<Self> {
        let input_duration = duration
            .checked_sub(Duration::from_secs(2))
            .context("trace has no guarded input interval")?;
        ensure!(
            !output.exists(),
            "refusing to overwrite capture {}",
            output.display()
        );
        if let Some(target) = &target {
            ensure!(
                target.exit_code()?.is_none(),
                "capture target exited before launch"
            );
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let name = format!("{OBJECT_PREFIX}-gate-{}-{nonce}", std::process::id());
        ensure!(
            gate_capture::trace_stats(&name)?.is_none(),
            "unique trace name already exists"
        );
        let (filter, value) = target.as_ref().map_or_else(
            || ("--process_name", SHELL_EXE.to_owned()),
            |target| ("--process_id", target.pid.to_string()),
        );
        let args = [
            filter.into(),
            value,
            "--session_name".into(),
            name.clone(),
            "--qpc_time".into(),
            "--v1_metrics".into(),
            "--no_track_input".into(),
            "--timed".into(),
            duration.as_secs().to_string(),
            "--terminate_after_timed".into(),
            "--no_console_stats".into(),
        ];
        let stdout = File::create_new(output.with_extension("stdout.txt"))?;
        let stderr = File::create_new(output.with_extension("stderr.txt"))?;
        let mut command = gate_capture::presentmon_command()?;
        command
            .args(&args)
            .arg("--output_file")
            .arg(output)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        let mut evidence = File::create_new(output.with_extension("command.txt"))?;
        std::io::Write::write_fmt(
            &mut evidence,
            format_args!(
                "start_unix_ns={nonce}\ncommand={command:?}\nfixed_backstop_s={}\n",
                duration.as_secs()
            ),
        )?;
        guard()?;
        let started = Instant::now();
        let child = command
            .spawn()
            .context("starting bounded own-process PresentMon")?;
        let mut capture = Self {
            child: Some(child),
            target,
            output: output.to_owned(),
            name,
            deadline: started + duration + Duration::from_secs(5),
            input_end: started + input_duration,
            dock_swapchain: 0,
        };
        let startup = capture.await_ready(guard);
        if let Err(error) = startup {
            let cleanup = capture.finish(guard);
            return Err(error.context(format!("trace cleanup: {:?}", cleanup.err())));
        }
        Ok(capture)
    }

    fn await_ready(&mut self, guard: &impl Fn() -> Result<()>) -> Result<()> {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            guard()?;
            if let Some(target) = &self.target {
                ensure!(
                    target.exit_code()?.is_none(),
                    "stable capture PID exited during startup"
                );
            }
            ensure!(
                self.child
                    .as_mut()
                    .context("trace child missing")?
                    .try_wait()?
                    .is_none(),
                "PresentMon exited before readiness"
            );
            if let Some(stats) = gate_capture::trace_stats(&self.name)? {
                stats.validate()?;
                return Ok(());
            }
            ensure!(
                Instant::now() < end,
                "owned PresentMon ETW session did not become ready"
            );
            sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn finish(&mut self, guard: impl Fn() -> Result<()>) -> Result<PresentRows> {
        let mut child = self.child.take().context("capture was already finished")?;
        let guarded = guard();
        let stopped =
            gate_capture::stop_trace(&self.name).and_then(gate_capture::TraceLoss::validate);
        let mut failures = Vec::new();
        if let Err(error) = guarded {
            failures.push(format!("guard: {error:#}"));
        }
        if let Err(error) = stopped {
            failures.push(format!("trace stop/loss: {error:#}"));
        }
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    failures.push(format!("PresentMon exit: {status}"));
                }
                fs::write(
                    self.output.with_extension("exit.txt"),
                    format!(
                        "exit={status}\nstop_unix_ns={}\nfailures={failures:?}\n",
                        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
                    ),
                )?;
                break;
            }
            if failures.is_empty()
                && let Err(error) = guard()
            {
                failures.push(format!("guard: {error:#}"));
            }
            ensure!(
                Instant::now() < self.deadline,
                "bounded PresentMon failed to exit; PID{} left for its fixed timer; no force-kill",
                child.id()
            );
            sleep(Duration::from_millis(10));
        }
        let target = self
            .target
            .as_ref()
            .context("prelaunch trace never acquired its owned PID")?;
        ensure!(
            target.exit_code()?.is_none(),
            "stable capture PID exited before trace completed"
        );
        let rows = present::read(&self.output, target.pid);
        if let Err(error) = &rows {
            failures.push(format!("CSV: {error:#}"));
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        rows
    }
}

pub(super) fn guarded_wait(duration: Duration, guard: &impl Fn() -> Result<()>) -> Result<()> {
    let end = Instant::now() + duration;
    loop {
        guard()?;
        let Some(left) = end.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        sleep(left.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_an_empty_bounded_wait_checks_its_abort_guard() {
        assert!(guarded_wait(Duration::ZERO, &|| Err(anyhow::anyhow!("aborted"))).is_err());
        assert!(guarded_wait(Duration::ZERO, &|| Ok(())).is_ok());
    }
}
