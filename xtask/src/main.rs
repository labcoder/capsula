//! Capsula's dev tasks, run as `cargo xtask <task>`. Dev tasks live here, in Rust, not in scripts.
#![expect(
    clippy::print_stdout,
    reason = "a command-line tool's output is its stdout"
)]

use anyhow::{Result, bail};

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  help    show this message";

fn main() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        None | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task {other:?}\n\n{USAGE}"),
    }
}
