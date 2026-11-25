//! File ownership boundary for the dormant shared logger.

use std::io;

use time::Date;

pub(crate) mod owner;

pub(crate) const KEEP_FILES: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("log object operation failed")]
    Open(io::ErrorKind),
    #[error("log object admission refused")]
    SecurityRefused,
}

#[derive(Debug, Default)]
pub(crate) struct Retention {
    pub(crate) skipped: usize,
    pub(crate) incomplete: bool,
}

pub(crate) trait Log: Send + Sync {
    fn append(&self, bytes: &[u8]) -> io::Result<()>;
    fn flush(&self) -> io::Result<()>;
}

pub(crate) trait Store: Send + 'static {
    type Log: Log + 'static;
    fn open(&mut self, day: Date) -> Result<Self::Log, Error>;
    fn retain(&mut self, current: &Self::Log) -> Result<Retention, Error>;
}

pub(crate) fn filename(prefix: &str, day: Date) -> Result<String, Error> {
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !(1..=9999).contains(&day.year())
    {
        return Err(Error::Open(io::ErrorKind::InvalidInput));
    }
    Ok(format!("{prefix}.{day}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_names_are_exact_and_reject_invalid_prefixes_or_years() {
        let date = Date::from_calendar_date(2026, time::Month::November, 8).unwrap();
        assert_eq!(
            filename("shell-dev", date).unwrap(),
            "shell-dev.2026-11-08.log"
        );
        for prefix in ["", "../escape", "a.b", "a/b", "a\\b"] {
            assert_eq!(
                filename(prefix, date),
                Err(Error::Open(io::ErrorKind::InvalidInput))
            );
        }
        assert_eq!(
            filename("shell-dev", Date::MIN),
            Err(Error::Open(io::ErrorKind::InvalidInput))
        );
    }
}
