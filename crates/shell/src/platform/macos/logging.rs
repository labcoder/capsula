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
    /// Starts and installs the shell logger in the lock's admitted storage, or hands the lock
    /// back when it can't, so a refusal still holds it.
    pub(crate) fn start(instance: InstanceLock) -> Result<Self, InstanceLock> {
        let Some(storage) = instance.storage().cloned() else {
            return Err(instance);
        };
        match Logging::start(Backend::new(storage, constants::SHELL_LOG)).and_then(Logging::install)
        {
            Ok(logging) => Ok(Self {
                _logging: logging,
                _instance: instance,
            }),
            Err(_) => Err(instance),
        }
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kumochi_protocol::constants;
    use kumochi_shell_core::instance::{FirstTouchError, InstanceLock};

    use super::Admitted;

    const SENTINEL: &[u8] = b"planted-path-sentinel";

    #[test]
    #[ignore = "requires exact-head review and a bounded native fixture-parent commission; sets the process's global subscriber"]
    fn a_logger_that_fails_after_its_file_opens_hands_the_held_lock_back() {
        let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
            .expect("an explicitly commissioned fixture parent");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let support = Path::new(&parent).join(format!(
            "first-touch-planted-path-sentinel-{}-{nanos}",
            std::process::id()
        ));
        let product = support.join(constants::DATA_DIR);
        for folder in [&support, &product] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(folder)
                .expect("exclusive fixture folder");
        }
        // A subscriber is already global, so installing fails after the log file opened.
        tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default())
            .expect("the fixture's subscriber is the process's first");

        let lock = InstanceLock::try_acquire_existing(&support).expect("first touch");
        let refused = Admitted::start(lock);
        let held = InstanceLock::try_acquire_existing(&support).map(drop);
        drop(refused.err().expect("the logger couldn't install"));
        let released = InstanceLock::try_acquire_existing(&support).map(drop);

        let logs = product.join("logs");
        let mut names = Vec::new();
        let mut written = Vec::new();
        for entry in std::fs::read_dir(&logs).expect("list the fixture logs") {
            let path = entry.expect("a fixture log").path();
            names.push(
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
            );
            written.push(std::fs::read(&path).expect("read a fixture log"));
            std::fs::remove_file(&path).expect("remove a fixture log");
        }
        std::fs::remove_dir(&logs).expect("remove the fixture logs folder");
        std::fs::remove_file(product.join(format!("{}.lock", constants::SHELL_LOCK)))
            .expect("remove the fixture lock");
        std::fs::remove_dir(&product).expect("remove the fixture product folder");
        std::fs::remove_dir(&support).expect("remove the fixture anchor");

        assert_eq!(
            held,
            Err(FirstTouchError::Busy),
            "the refused logger handed back the held lock"
        );
        assert_eq!(
            released,
            Ok(()),
            "dropping the handed-back lock releases it"
        );
        let prefix = format!("{}.", constants::SHELL_LOG);
        let day = |name: &String| {
            name.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".log"))
                .map(str::len)
        };
        assert!(
            matches!(names.as_slice(), [Some(name)] if day(name) == Some("YYYY-MM-DD".len())),
            "the logger opened its one log before refusing: {names:?}"
        );
        assert!(
            written
                .iter()
                .all(|log| !log.windows(SENTINEL.len()).any(|bytes| bytes == SENTINEL)),
            "no log names the support path"
        );
    }
}
