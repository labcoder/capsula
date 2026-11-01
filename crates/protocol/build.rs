//! Stamps `CAPSULA_COMMIT` for the build id, so a brain and a shell built from different commits
//! refuse each other.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_owned())
}

fn main() {
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // HEAD's reflog changes on every commit and checkout, so the id follows them.
    if let Some(reflog) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
        println!("cargo::rerun-if-changed={reflog}");
    }
    println!("cargo::rustc-env=CAPSULA_COMMIT={commit}");
}
