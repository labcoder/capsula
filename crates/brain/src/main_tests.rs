use std::{fs, io};

use kumochi_brain::ipc::IpcError;

use super::{BrainError, DevCheck, Route, constants, log_brain_stopped, logging, route};

#[path = "../../../tests/support/scratch.rs"]
mod scratch;

#[path = "platform/test_args.rs"]
mod test_args;

#[test]
fn only_the_first_flag_selects_a_route_and_only_parent_bound_rejects_a_tail() {
    const LAUNCH: &str = "0123456789abcdef0123456789abcdef";
    let diagnostics = [
        ("--image-check", DevCheck::Image),
        ("--image-diagnostic", DevCheck::ImageDiagnostic),
        ("--marks-check", DevCheck::Marks),
        ("--draw-check", DevCheck::Draw),
        ("--catalog-provenance", DevCheck::CatalogProvenance),
    ];
    for (args, expected) in [
        (&[][..], Route::Detached),
        (&["--unknown", "--parent-bound"], Route::Detached),
        (&["--screen"], Route::Screen),
        (&["--screen", "--parent-bound"], Route::Screen),
        (&["--speech"], Route::Speech),
        (&["--speech", "--parent-bound"], Route::Speech),
        (&["--mcp-bridge"], Route::Bridge(String::new())),
        (
            &["--mcp-bridge", "--parent-bound"],
            Route::Bridge("--parent-bound".to_owned()),
        ),
        (&["--mcp-bridge", LAUNCH], Route::Bridge(LAUNCH.to_owned())),
        (
            &["--mcp-bridge", LAUNCH, "--parent-bound"],
            Route::Bridge(LAUNCH.to_owned()),
        ),
        (&["--parent-bound"], Route::ParentBound),
        (&["--parent-bound", ""], Route::BadParentBound),
        (&["--parent-bound", "--screen"], Route::BadParentBound),
    ] {
        assert!(route(args.iter().map(Into::into)) == expected, "{args:?}");
    }
    for (flag, diagnostic) in diagnostics {
        let expected = Route::Diagnostic(diagnostic);
        for args in [&[flag][..], &[flag, "--parent-bound"]] {
            let selected = route(args.iter().map(Into::into));
            assert!(selected == expected, "{args:?}");
            #[cfg(not(debug_assertions))]
            if let Route::Diagnostic(kind) = selected {
                assert_eq!(
                    super::run_diagnostic(kind),
                    std::process::ExitCode::from(10)
                );
            }
        }
    }
}

#[test]
fn non_unicode_arguments_follow_the_same_position_rules_without_panicking() {
    use test_args::non_unicode as invalid;

    assert!(invalid().to_str().is_none());
    let launch = "0123456789abcdef0123456789abcdef";
    for (index, (args, expected)) in [
        (vec![invalid(), "--parent-bound".into()], Route::Detached),
        (
            vec!["--parent-bound".into(), invalid()],
            Route::BadParentBound,
        ),
        (
            vec!["--mcp-bridge".into(), invalid(), launch.into()],
            Route::Bridge(String::new()),
        ),
        (
            vec!["--mcp-bridge".into(), launch.into(), invalid()],
            Route::Bridge(launch.to_owned()),
        ),
        (vec!["--screen".into(), invalid()], Route::Screen),
        (vec!["--speech".into(), invalid()], Route::Speech),
        (
            vec!["--image-check".into(), invalid()],
            Route::Diagnostic(DevCheck::Image),
        ),
        (vec!["--unknown".into(), invalid()], Route::Detached),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(route(args.into_iter()) == expected, "routing case {index}");
    }
}

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
