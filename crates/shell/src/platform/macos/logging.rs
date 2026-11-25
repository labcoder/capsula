//! Opaque admitted files; no pathname reconstruction or descriptor escape.

use std::io;

use kumochi_protocol::constants;
use kumochi_shell_core::instance::InstanceLock;
use kumochi_storage::{AcquisitionError, AdmittedLog, ShellStorage};
use time::Date;

use crate::platform::logging::owner::Logging;
use crate::platform::logging::{self, Error, Log, Retention, Store};

/// Startup admission: the logger and the lock whose storage it writes to.
/// Field order is drop order, so the logger closes and joins before the lock is released.
pub(crate) struct Admitted {
    _logging: Logging<Backend>,
    _instance: InstanceLock,
}

impl Admitted {
    /// Starts and installs the shell logger in the lock's admitted storage.
    pub(crate) fn start(instance: InstanceLock) -> Option<Self> {
        let storage = instance.storage()?.clone();
        let logging = Logging::start(Backend::new(storage, constants::SHELL_LOG))
            .and_then(Logging::install)
            .ok()?;
        Some(Self {
            _logging: logging,
            _instance: instance,
        })
    }
}

pub(crate) struct Backend {
    storage: ShellStorage,
    prefix: String,
}

impl Backend {
    pub(crate) fn new(storage: ShellStorage, prefix: &str) -> Self {
        Self {
            storage,
            prefix: prefix.to_owned(),
        }
    }
}

impl Store for Backend {
    type Log = AdmittedLog;

    fn open(&mut self, day: Date) -> Result<AdmittedLog, Error> {
        self.storage
            .append_log(&logging::filename(&self.prefix, day)?)
            .map_err(classify)
    }

    fn retain(&mut self, current: &AdmittedLog) -> Result<Retention, Error> {
        self.storage
            .retain_logs(&self.prefix, logging::KEEP_FILES, current)
            .map(|r| Retention {
                skipped: r.skipped,
                incomplete: r.incomplete,
            })
            .map_err(classify)
    }
}

impl Log for AdmittedLog {
    fn append(&self, bytes: &[u8]) -> io::Result<()> {
        let mut log = self;
        io::Write::write_all(&mut log, bytes)
    }
    fn flush(&self) -> io::Result<()> {
        let mut log = self;
        io::Write::flush(&mut log)
    }
}

fn classify(error: AcquisitionError) -> Error {
    match error {
        AcquisitionError::Open(kind) => Error::Open(kind),
        AcquisitionError::Admission(_) => Error::SecurityRefused,
    }
}
