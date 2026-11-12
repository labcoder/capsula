//! Kumochi's dev tasks, run as `cargo xtask <task>`. Dev tasks live here, in Rust, not in scripts.
#![expect(
    clippy::print_stdout,
    reason = "a command-line tool's output is its stdout"
)]

use std::fs::File;
use std::io::BufReader;

use anyhow::{Context as _, Result, bail};
use kumochi_protocol::constants::PIPE_TAP_ENV;
use kumochi_protocol::tap;

#[cfg(windows)]
mod bench;
#[cfg(windows)]
mod gate;
#[cfg(windows)]
mod hermetic;
#[cfg(windows)]
mod live;
#[cfg(windows)]
mod platform;
#[cfg(test)]
mod scratch;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod slice2;
mod vendor;
#[cfg(windows)]
mod voice_assets;

fn usage() -> String {
    format!(
        "usage: cargo xtask <task>

The gate tasks (cold-start, gate-session, hover-gate, bench, slice2-gate) run only as the gate
identity, never as Kumochi or Kumochi Dev, and build the images they launch themselves:
  cargo run --release --package xtask --features kumochi-protocol/gate-identity -- <task>
See docs/development/gate-identity.md.

tasks:
  cold-start <path-session.txt>
                    approved input-free ten-launch cold gate; create-new redirect
                    stdout and stderr to this file; separate from owner Ready session
  gate-session <path-session.txt>
                    approved final-main gate; stdout and stderr must already be
                    redirected create-new to this file; one real Ready click,
                    one persistent fixture, at most one hour, no input retries
  hover-gate <path-session.txt>
                    the hover phase alone, same redirect rule; injection opens only
                    after ten minutes with no input on Rez, then the quiet second
  input-fixture     internal bounded separate-process EDIT/Ready fixture
  input-quiet <seconds>
                    observe raw input on a hidden window for 1..120 seconds;
                    class, motion and marker only; no keys/text or injected input
  bench idle [--duration 1..60] [--baseline <checkout> | --panel]
                    release idle CPU/private bytes on Rez (default 60s)
                    refuses existing product processes; normal-priority launch,
                    30s settle, GetProcessTimes + private bytes, bounded autoquit;
                    CPU is percent of one core, not the whole machine;
                    three 2s saturated-thread trials calibrate the highest cycle rate
                    before launch; cycles CPU is the gate, GetProcessTimes stays beside it
                    baseline builds that checkout's images and closes its own shell window
                    panel opens the current shell's panel before settling, without input
                    the current shell must paint no dock frame from the settle to Quit
  live account      print only the dev brain's non-secret account state; never signs in
                    or sends a model request; refuses while any product process runs
  live signin       sign the dev brain in to its account: the brain opens the browser,
                    and only the account's state is printed; refuses while any
                    product process runs; needs the owner at the desk
  live claude-signin [--disconnect]
                    sign the dev brain's Claude Code in: its own sign-in runs in a
                    console the owner answers; --disconnect signs it out and deletes
                    Kumochi's folder for it; only the view's state is printed; refuses
                    while any product process runs; needs the owner at the desk
  live catalog-provenance
                    one model-list request with the dev account's saved session, never
                    renewing it; writes target\\live\\<time>\\catalog-provenance.json with
                    only approved keys and closed words; exits 10-19 having sent nothing;
                    refuses while any product process runs
  slice2-gate       the slice 2 gate: the real brain with fixture scripts, driven
                    over its real pipes by a scripted client; no input; refuses while
                    any product process runs
  check-vendor      fail unless every crate in vendor/ is what Cargo.lock builds;
                    CI runs it so a GPUI bump can't silently drop a vendored patch
  voice-assets link <models-dir> <natives-dir>
                    verify the voice models and native runtime already on this machine
                    against the manifest, then hardlink them into the dev models folder
                    and beside target\\{{debug,release}}\\kumochid.exe; never copies or downloads
  pipe-dump <file>  decode a pipe recording; a debug brain writes one when
                    {PIPE_TAP_ENV} names a file
  help              show this message"
    )
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("help") => {
            println!("{}", usage());
            Ok(())
        }
        Some("check-vendor") => vendor::run(),
        Some("pipe-dump") => pipe_dump(
            &args
                .next()
                .with_context(|| format!("pipe-dump needs a file\n\n{}", usage()))?,
        ),
        #[cfg(windows)]
        Some("bench") => bench::run(args),
        #[cfg(windows)]
        Some("gate-session") => gate::run(args),
        #[cfg(windows)]
        Some("hover-gate") => gate::hover(args),
        #[cfg(windows)]
        Some("live") => live::run(args),
        #[cfg(windows)]
        Some("slice2-gate") => slice2::run(args),
        #[cfg(windows)]
        Some("cold-start") => gate::cold(args),
        #[cfg(windows)]
        Some("voice-assets") => voice_assets::run(args),
        #[cfg(windows)]
        Some("input-fixture") => platform::input_session::fixture(),
        #[cfg(windows)]
        Some("input-quiet") => platform::input_session::quiet(quiet_seconds(args)?),
        #[cfg(not(windows))]
        Some("bench") => bail!("idle benchmarking requires Windows on Rez"),
        Some(other) => bail!("unknown task {other:?}\n\n{}", usage()),
    }
}

#[cfg(windows)]
fn quiet_seconds(mut args: impl Iterator<Item = String>) -> Result<u64> {
    let seconds = args
        .next()
        .context("input-quiet needs 1..120 seconds")?
        .parse()?;
    anyhow::ensure!(
        (1..=120).contains(&seconds),
        "input-quiet needs 1..120 seconds"
    );
    anyhow::ensure!(args.next().is_none(), "input-quiet accepts only seconds");
    Ok(seconds)
}

#[cfg(all(test, windows))]
mod tests {
    use super::quiet_seconds;

    #[test]
    fn quiet_duration_is_explicit_bounded_and_has_no_extra_arguments() {
        for seconds in [1, 120] {
            assert_eq!(
                quiet_seconds([seconds.to_string()].into_iter()).unwrap(),
                seconds
            );
        }
        for args in [
            vec![],
            vec!["0"],
            vec!["121"],
            vec!["-1"],
            vec!["abc"],
            vec!["1", "2"],
        ] {
            assert!(quiet_seconds(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}

/// Prints one line per recorded frame: its number, pipe, direction, op and length, and the
/// fields of content-free ops.
fn pipe_dump(path: &str) -> Result<()> {
    let mut reader = BufReader::new(File::open(path).with_context(|| format!("opening {path}"))?);
    tap::dump(&mut reader, &mut std::io::stdout().lock())?;
    Ok(())
}
