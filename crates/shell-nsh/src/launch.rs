//! The shell's launch flags: `--quit-after-ms=<1..=120000>`, at most once, and nothing else.

use std::ffi::OsString;
use std::time::Duration;

use kumochi_shell_core::startup::quit_after_ms;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Launch {
    /// When the shell quits on its own, if it is given a deadline.
    pub(crate) quit_after: Option<Duration>,
}

/// Why the flags were refused. Unit variants only, so no argument value is kept or shown.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LaunchError {
    #[error("--quit-after-ms must be between 1 and 120000")]
    QuitDelay,
    #[error("--quit-after-ms comes at most once")]
    Repeated,
    #[error("unknown launch flag")]
    Unknown,
}

impl Launch {
    /// The flags in `args`, the program name left out.
    pub(crate) fn parse(args: impl Iterator<Item = OsString>) -> Result<Self, LaunchError> {
        let mut quit_after = None;
        for arg in args {
            let value = arg
                .to_str()
                .and_then(|arg| arg.strip_prefix("--quit-after-ms="))
                .ok_or(LaunchError::Unknown)?;
            let delay = quit_after_ms(value).ok_or(LaunchError::QuitDelay)?;
            if quit_after.replace(delay).is_some() {
                return Err(LaunchError::Repeated);
            }
        }
        Ok(Self { quit_after })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Launch, LaunchError> {
        Launch::parse(args.iter().map(OsString::from))
    }

    fn after(millis: u64) -> Launch {
        Launch {
            quit_after: Some(Duration::from_millis(millis)),
        }
    }

    #[test]
    fn launch_parses_only_its_flag() {
        for (args, expected) in [
            (&["--quit-after-ms=2000"][..], Ok(after(2000))),
            (
                &["--quit-after-ms=2000", "--quit-after-ms=3000"],
                Err(LaunchError::Repeated),
            ),
            (
                &["--quit-after-ms=2000", "--other"],
                Err(LaunchError::Unknown),
            ),
            (&["--other"], Err(LaunchError::Unknown)),
            (&["--quit-after-ms"], Err(LaunchError::Unknown)),
            // The brain relaunches a shell with no flags.
            (&[], Ok(Launch { quit_after: None })),
        ] {
            assert_eq!(parse(args), expected, "{args:?}");
        }
    }

    #[test]
    fn the_quit_delay_takes_one_to_120000() {
        for (value, expected) in [
            ("1", Ok(after(1))),
            ("120000", Ok(after(120_000))),
            ("0", Err(LaunchError::QuitDelay)),
            ("120001", Err(LaunchError::QuitDelay)),
            ("", Err(LaunchError::QuitDelay)),
        ] {
            assert_eq!(
                parse(&[&format!("--quit-after-ms={value}")]),
                expected,
                "--quit-after-ms={value}"
            );
        }
    }

    #[test]
    fn launch_errors_show_only_fixed_text() {
        const MARKER: &str = "private-fixture-value";
        for (args, debug, display) in [
            (
                vec![format!("--quit-after-ms={MARKER}")],
                "QuitDelay",
                "--quit-after-ms must be between 1 and 120000",
            ),
            (
                vec![format!("--{MARKER}")],
                "Unknown",
                "unknown launch flag",
            ),
            (
                vec![
                    "--quit-after-ms=1".to_owned(),
                    "--quit-after-ms=2".to_owned(),
                ],
                "Repeated",
                "--quit-after-ms comes at most once",
            ),
        ] {
            let error =
                Launch::parse(args.iter().map(OsString::from)).expect_err("the flags are refused");
            assert_eq!(format!("{error:?}"), debug, "{args:?}");
            assert_eq!(error.to_string(), display, "{args:?}");
        }
    }
}
