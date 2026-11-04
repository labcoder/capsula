//! The diagnostic launch flags. Every diagnostic launch quits on its own, so none of them can
//! hold global Esc, run frames or write a gate record forever.

use std::path::PathBuf;
use std::time::Duration;

use super::OpenError;
use super::geometry::Position;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Launch {
    pub(super) quit_delay: Option<Duration>,
    pub(super) escape_fixture: bool,
    /// `--gate-observer=<file>`: where the gate harness's records go.
    pub(super) observer: Option<PathBuf>,
    /// `--receipt-fixture`: start the dock middle-left for the gate's click-receipt phases.
    pub(super) receipt_fixture: bool,
}

impl Launch {
    /// Reads the flags after the program name.
    ///
    /// # Errors
    /// Fails on a bad value, a fixture or observer without a quit deadline, or a receipt
    /// fixture that is repeated or has no observer.
    pub(crate) fn parse(args: impl Iterator<Item = String>) -> Result<Self, OpenError> {
        let mut launch = Self::default();
        for arg in args {
            if arg == "--smoke" {
                launch.quit_delay = Some(Duration::from_secs(10));
            } else if arg == "--esc-fixture" {
                launch.escape_fixture = true;
            } else if let Some(path) = arg.strip_prefix("--gate-observer=") {
                launch.observer = Some(PathBuf::from(path));
            } else if arg == "--receipt-fixture" {
                if launch.receipt_fixture {
                    return Err(OpenError::ReceiptFixture);
                }
                launch.receipt_fixture = true;
            } else if let Some(value) = arg.strip_prefix("--quit-after-ms=") {
                let millis = value.parse::<u64>().map_err(|_| OpenError::QuitDelay)?;
                if !(1..=120_000).contains(&millis) {
                    return Err(OpenError::QuitDelay);
                }
                launch.quit_delay = Some(Duration::from_millis(millis));
            }
        }
        if launch.quit_delay.is_none() {
            if launch.escape_fixture {
                return Err(OpenError::EscFixtureDeadline);
            }
            if launch.observer.is_some() || launch.receipt_fixture {
                return Err(OpenError::ObserverDeadline);
            }
        }
        if launch.receipt_fixture && launch.observer.is_none() {
            return Err(OpenError::ReceiptFixture);
        }
        Ok(launch)
    }

    /// Where the dock starts: middle-left for the receipt fixture, else the default.
    pub(super) fn position(&self) -> Position {
        if self.receipt_fixture {
            Position::MiddleLeft
        } else {
            Position::default()
        }
    }

    /// The observer's file, if this run records the gate.
    pub(crate) fn observer(&self) -> Option<&std::path::Path> {
        self.observer.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{Launch, OpenError};
    use crate::presence::geometry::Position;

    fn launch(args: impl Iterator<Item = String>) -> Result<Launch, OpenError> {
        Launch::parse(args)
    }

    fn args(args: &[&str]) -> impl Iterator<Item = String> {
        args.iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn diagnostic_launches_have_a_bounded_quit_deadline() {
        assert_eq!(
            launch(args(&["--smoke"])).unwrap().quit_delay,
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            launch(args(&["--quit-after-ms=75000"])).unwrap().quit_delay,
            Some(Duration::from_secs(75))
        );
        for invalid in ["0", "120001", "-1", "later"] {
            assert!(launch([format!("--quit-after-ms={invalid}")].into_iter()).is_err());
        }
    }

    #[test]
    fn the_esc_fixture_only_runs_with_a_quit_deadline() {
        assert!(matches!(
            launch(args(&["--esc-fixture"])),
            Err(OpenError::EscFixtureDeadline)
        ));
        assert_eq!(
            launch(args(&["--esc-fixture", "--quit-after-ms=500"])).unwrap(),
            Launch {
                quit_delay: Some(Duration::from_millis(500)),
                escape_fixture: true,
                ..Launch::default()
            }
        );
        assert!(
            launch(args(&["--smoke", "--esc-fixture"]))
                .unwrap()
                .escape_fixture
        );
        assert!(!launch(args(&[])).unwrap().escape_fixture);
    }

    #[test]
    fn the_observer_and_receipt_fixture_only_run_with_a_quit_deadline() {
        for flags in [
            &["--gate-observer=gate.log"][..],
            &["--gate-observer=gate.log", "--receipt-fixture"][..],
        ] {
            assert!(matches!(
                launch(args(flags)),
                Err(OpenError::ObserverDeadline)
            ));
        }
        let flags = launch(args(&[
            "--gate-observer=gate.log",
            "--receipt-fixture",
            "--quit-after-ms=90000",
        ]))
        .unwrap();
        assert_eq!(
            flags,
            Launch {
                quit_delay: Some(Duration::from_secs(90)),
                observer: Some(PathBuf::from("gate.log")),
                receipt_fixture: true,
                ..Launch::default()
            }
        );
        assert_eq!(flags.position(), Position::MiddleLeft);
        let plain = launch(args(&[])).unwrap();
        assert_eq!(plain.observer, None);
        assert_eq!(plain.position(), Position::TopCenter);
    }

    #[test]
    fn the_receipt_fixture_needs_the_observer_and_comes_once() {
        assert!(matches!(
            launch(args(&["--receipt-fixture", "--smoke"])),
            Err(OpenError::ReceiptFixture)
        ));
        assert!(matches!(
            launch(args(&[
                "--gate-observer=gate.log",
                "--receipt-fixture",
                "--receipt-fixture",
                "--smoke",
            ])),
            Err(OpenError::ReceiptFixture)
        ));
    }
}
