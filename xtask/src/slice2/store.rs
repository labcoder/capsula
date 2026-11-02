//! What the gate reads back from a stopped brain's files: its rows, and every byte on disk.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use kumochi_protocol::message::ThreadId;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

/// Opens a stopped brain's store without writing to it.
pub(super) fn open(db: &Path) -> Result<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db.display()))
}

/// A run's state and reason.
pub(super) fn outcome(conn: &Connection, run: u64) -> Result<Option<(String, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT state, reason FROM runs WHERE id = ?1",
            [i64::try_from(run)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// A purge is still owed.
pub(super) fn purge_pending(conn: &Connection) -> Result<bool> {
    let owed: i64 = conn.query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))?;
    Ok(owed > 0)
}

/// The thread's own rows, its runs' and its events'.
pub(super) fn thread_rows(conn: &Connection, thread: ThreadId) -> Result<i64> {
    let id = i64::try_from(thread.0)?;
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM threads WHERE id = ?1)
              + (SELECT count(*) FROM runs WHERE thread = ?1)
              + (SELECT count(*) FROM events WHERE thread = ?1)",
        [id],
        |row| row.get(0),
    )?)
}

/// Rows in every table and FTS hits holding `marker`, ignoring ASCII case.
#[derive(Debug, Default)]
pub(super) struct Rows {
    pub(super) tables: usize,
    pub(super) values: usize,
    pub(super) hits: Vec<String>,
    pub(super) fts: usize,
}

pub(super) fn rows(conn: &Connection, marker: &str) -> Result<Rows> {
    let marker = marker.to_ascii_lowercase();
    let mut found = Rows::default();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for table in tables {
        found.tables += 1;
        let mut statement = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..columns {
                let bytes = match row.get_ref(column)? {
                    ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
                    ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => continue,
                };
                found.values += 1;
                if contains(bytes, marker.as_bytes()) {
                    found.hits.push(table.clone());
                }
            }
        }
    }
    let fts: i64 = conn.query_row(
        "SELECT count(*) FROM events_fts WHERE events_fts MATCH ?1",
        [format!("\"{marker}\"")],
        |row| row.get(0),
    )?;
    found.fts = usize::try_from(fts)?;
    Ok(found)
}

/// Every file under `root` whose raw bytes hold `marker` as UTF-8, ignoring ASCII case, and
/// how many files were read.
pub(super) fn files(root: &Path, marker: &str) -> Result<(usize, Vec<PathBuf>)> {
    let marker = marker.to_ascii_lowercase();
    let mut read = 0;
    let mut hits = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder)? {
            let path = entry?.path();
            if path.is_dir() {
                folders.push(path);
                continue;
            }
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            read += 1;
            if contains(&bytes, marker.as_bytes()) {
                hits.push(path);
            }
        }
    }
    Ok((read, hits))
}

/// The purge's files beside the store.
pub(super) fn purge_files(data: &Path) -> Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    for entry in fs::read_dir(data)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".purge-"))
        {
            left.push(path);
        }
    }
    Ok(left)
}

fn contains(haystack: &[u8], lower: &[u8]) -> bool {
    haystack
        .windows(lower.len())
        .any(|window| window.eq_ignore_ascii_case(lower))
}

#[cfg(test)]
mod tests {
    use super::contains;

    #[test]
    fn a_marker_is_found_in_any_ascii_case_and_only_whole() {
        assert!(contains(b"x CapsGate1A y", b"capsgate1a"), "mixed case");
        assert!(
            contains(b"CAPSGATE1A", b"capsgate1a"),
            "upper case at the edges"
        );
        assert!(!contains(b"capsgate1", b"capsgate1a"), "a prefix");
        assert!(!contains(b"", b"capsgate1a"), "empty");
    }
}
