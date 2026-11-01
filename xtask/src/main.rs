//! Capsula's dev tasks, run as `cargo xtask <task>`. Dev tasks live here, in Rust, not in scripts.
#![expect(
    clippy::print_stdout,
    reason = "a command-line tool's output is its stdout"
)]

use std::fs::File;
use std::io::BufReader;

use anyhow::{Context as _, Result, bail};
use capsula_protocol::constants::PIPE_TAP_ENV;
use capsula_protocol::tap::{self, Direction};

#[cfg(windows)]
mod bench;
#[cfg(windows)]
mod platform;

fn usage() -> String {
    format!(
        "usage: cargo xtask <task>

tasks:
  bench idle [--duration 1..60]  release idle CPU/private bytes on Rez (default 60s)
                    refuses existing product processes; normal-priority launch,
                    5s settle, GetProcessTimes + private bytes, bounded autoquit;
                    CPU is percent of one core, not the whole machine
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
        Some("pipe-dump") => pipe_dump(
            &args
                .next()
                .with_context(|| format!("pipe-dump needs a file\n\n{}", usage()))?,
        ),
        #[cfg(windows)]
        Some("bench") => bench::run(args),
        #[cfg(not(windows))]
        Some("bench") => bail!("idle benchmarking requires Windows on Rez"),
        Some(other) => bail!("unknown task {other:?}\n\n{}", usage()),
    }
}

/// Prints one line per recorded frame: its number, pipe, direction and decoded message.
fn pipe_dump(path: &str) -> Result<()> {
    let mut reader = BufReader::new(File::open(path).with_context(|| format!("opening {path}"))?);
    let mut seq = 0u64;
    while let Some(record) = tap::read(&mut reader).with_context(|| format!("record {seq}"))? {
        let arrow = match record.direction {
            Direction::ToBrain => "shell -> brain",
            Direction::ToShell => "brain -> shell",
        };
        println!(
            "{seq:>5} {:<7?} {arrow}  {}",
            record.pipe,
            record.describe()
        );
        seq += 1;
    }
    println!("{seq} frames");
    Ok(())
}
