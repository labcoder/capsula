//! Stamps `KUMOCHI_COMMIT` for the build id, so a brain and a shell built from different commits
//! refuse each other.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_owned())
}

fn main() {
    // Without a commit, builds from different sources would share an id and accept each other.
    let commit = git(&["rev-parse", "--short=12", "HEAD"])
        .expect("the build id needs git: build from a git checkout with git on PATH");
    // HEAD's reflog changes on every commit and checkout, so the id follows them.
    if let Some(reflog) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
        println!("cargo::rerun-if-changed={reflog}");
    }
    println!("cargo::rustc-env=KUMOCHI_COMMIT={commit}");
}
