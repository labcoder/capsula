//! The unsupported executable modes must refuse before reaching app data.
#![cfg(not(windows))]

#[path = "../../../tests/support/scratch.rs"]
#[expect(
    clippy::print_stderr,
    reason = "the shared fixture reports cleanup failures during unwind"
)]
mod scratch;

use std::process::Command;

use kumochi_brain::{MCP_BRIDGE, SCREEN_WORKER, SPEECH_WORKER};
use kumochi_protocol::constants::{
    CATALOG_PROVENANCE_ARG, DRAW_CHECK_ARG, IMAGE_CHECK_ARG, IMAGE_DIAGNOSTIC_ARG, MARKS_CHECK_ARG,
};

#[cfg(target_os = "macos")]
#[test]
fn direct_logging_refuses_before_creating_an_appender() {
    use std::time::{Duration, Instant};

    use kumochi_brain::logging::{LoggingError, init};
    use kumochi_brain::{platform, store};
    use kumochi_protocol::constants::{ProductIdentity, STORE_FILE};

    const CHILD: &str = "KUMOCHI_TEST_DIRECT_LOGGING_CHILD";
    const CHECKS_RAN: i32 = 77;

    if std::env::var_os(CHILD).is_some() {
        let home = std::env::current_dir().unwrap().join("home");
        assert_eq!(std::env::var_os("HOME").as_deref(), Some(home.as_os_str()));
        assert!(!home.exists());
        let expected = home
            .join("Library/Application Support")
            .join(ProductIdentity::ACTIVE.data_dir);
        assert_eq!(platform::data_dir().as_ref(), Some(&expected));
        assert_eq!(platform::config_dir().as_ref(), Some(&expected));
        assert_eq!(store::default_path().unwrap(), expected.join(STORE_FILE));
        assert!(matches!(
            init("refused-entry"),
            Err(LoggingError::NativeRuntime(error))
                if error.kind() == std::io::ErrorKind::Unsupported
        ));
        std::process::exit(CHECKS_RAN);
    }

    let scratch = scratch::dir("direct-logging");
    let root = scratch.canonicalize().unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "this macOS test execs itself with a private HOME"
    )]
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "direct_logging_refuses_before_creating_an_appender",
        ])
        .env_clear()
        .env(CHILD, "1")
        .env("HOME", root.join("home"))
        .current_dir(&root)
        .spawn()
        .expect("start the direct logging fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let stopped = child.kill().is_ok() || child.try_wait().unwrap().is_some();
            assert!(stopped, "could not stop the direct logging fixture");
            let reap_by = Instant::now() + Duration::from_secs(2);
            while child.try_wait().unwrap().is_none() {
                assert!(
                    Instant::now() < reap_by,
                    "direct logging fixture did not exit"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("direct logging fixture exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        root.read_dir().unwrap().count(),
        0,
        "child wrote to its scratch"
    );
    assert_eq!(
        status.code(),
        Some(CHECKS_RAN),
        "child did not prove refusal"
    );
}

#[test]
fn native_executable_modes_refuse_without_touching_app_data() {
    let dir = scratch::dir("unsupported-entry");
    for args in [
        &[][..],
        &[MCP_BRIDGE, "0123456789abcdef0123456789abcdef"],
        &[SCREEN_WORKER],
        &[SPEECH_WORKER],
        &[IMAGE_DIAGNOSTIC_ARG],
        &[IMAGE_CHECK_ARG],
        &[MARKS_CHECK_ARG],
        &[DRAW_CHECK_ARG],
        &[CATALOG_PROVENANCE_ARG],
    ] {
        #[expect(
            clippy::disallowed_methods,
            reason = "this non-Windows test checks the real executable without inheritable Windows handles"
        )]
        let child = Command::new(env!("CARGO_BIN_EXE_kumochid"))
            .args(args)
            .env_clear()
            .env("HOME", dir.join("home"))
            .env("LOCALAPPDATA", dir.join("local"))
            .env("APPDATA", dir.join("roaming"))
            .current_dir(dir.path())
            .output()
            .expect("run the test build of the brain");
        assert_eq!(child.status.code(), Some(69), "{args:?}");
        assert!(child.stdout.is_empty(), "{args:?}");
        assert!(child.stderr.is_empty(), "{args:?}");
        assert_eq!(
            dir.read_dir().expect("scratch directory").count(),
            0,
            "{args:?}"
        );
    }
}
