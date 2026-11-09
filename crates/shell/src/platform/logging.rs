//! File ownership boundary for the dormant shared logger.

use std::io;

use time::Date;

#[cfg(all(test, windows))]
pub(crate) use super::windows::logging::active_file;

pub(crate) const KEEP_FILES: usize = 7;
pub(crate) const SCAN_LIMIT: usize = 256;

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

pub(crate) fn file_date(prefix: &str, name: &str) -> Option<Date> {
    let date = name
        .strip_prefix(prefix)?
        .strip_prefix('.')?
        .strip_suffix(".log")?;
    if date.len() != 10
        || date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || !date
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return None;
    }
    let year = date[..4].parse().ok()?;
    if year == 0 {
        return None;
    }
    Date::from_calendar_date(
        year,
        date[5..7].parse::<u8>().ok()?.try_into().ok()?,
        date[8..].parse().ok()?,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_names_are_exact_and_reject_other_prefixes_dates_or_paths() {
        let date = Date::from_calendar_date(2026, time::Month::November, 8).unwrap();
        assert_eq!(
            filename("shell-dev", date).unwrap(),
            "shell-dev.2026-11-08.log"
        );
        for name in [
            "shell-devx.2026-11-08.log",
            "shell-dev.2026-02-30.log",
            "shell-dev.0000-11-08.log",
            "shell-dev.2026-11-08.log.old",
            "shell-dev.lock",
            "shell-dev.２０２６-11-08.log",
        ] {
            assert_eq!(file_date("shell-dev", name), None, "{name}");
        }
        assert_eq!(
            file_date("shell-dev", "shell-dev.2024-02-29.log"),
            Some(Date::from_calendar_date(2024, time::Month::February, 29).unwrap())
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
