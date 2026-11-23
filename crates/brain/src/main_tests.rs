use std::{fs, io};

use kumochi_brain::ipc::IpcError;

use super::{BrainError, constants, log_brain_stopped, logging};

#[path = "../../../tests/support/scratch.rs"]
mod scratch;

#[test]
fn brain_failure_log_keeps_only_the_kind_and_optional_os_code() {
    const SENTINEL: &str = "d0-foreign-error-sentinel";
    let dir = scratch::dir("brain-failure-log");
    let byte_list = format!("{:?}", SENTINEL.as_bytes());
    let foreign = io::Error::other(format!("{SENTINEL} {byte_list}"));
    let native = io::Error::from_raw_os_error(5);
    let subscriber = logging::file_subscriber(&dir, constants::BRAIN_LOG).unwrap();
    tracing::subscriber::with_default(subscriber, || {
        for error in [foreign, native] {
            log_brain_stopped(&BrainError::Ipc(IpcError::Accept(error)));
        }
    });

    let log: String = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(
        !log.contains(SENTINEL),
        "foreign error text reached the log"
    );
    assert!(
        !log.contains(&byte_list),
        "foreign error bytes reached the log"
    );
    assert!(
        log.lines()
            .any(|line| line.contains("ERROR")
                && line.ends_with("brain stopped kind=\"ipc_accept\""))
    );
    assert!(log.lines().any(|line| {
        line.contains("ERROR") && line.ends_with("brain stopped kind=\"ipc_accept\" os_code=5")
    }));
}
