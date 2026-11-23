use std::fs;

use super::{BUILD_ID, ControlToShell, LinkError, Pipe, frame, welcome};

#[test]
fn build_mismatch_log_does_not_include_the_peer_build() {
    const SENTINEL: &str = "d0-peer-build-sentinel";
    let dir = crate::scratch::dir("build-mismatch-log");
    let frame = frame::encode(
        &ControlToShell::BuildMismatch {
            brain_build: SENTINEL.to_owned(),
        },
        Pipe::Control,
    )
    .unwrap();
    let subscriber =
        crate::logging::file_subscriber(&dir, kumochi_protocol::constants::SHELL_LOG).unwrap();
    let result = tracing::subscriber::with_default(subscriber, || welcome(&mut frame.as_slice()));
    assert!(matches!(result, Err(LinkError::BuildMismatch)));

    let log: String = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(!log.contains(SENTINEL), "the peer's build reached the log");
    assert!(!log.contains(&format!("{:?}", SENTINEL.as_bytes())));
    assert!(log.lines().any(|line| {
        line.contains("WARN")
            && line.ends_with(&format!(
                "brain from another build shell_build={BUILD_ID:?}"
            ))
    }));
}
