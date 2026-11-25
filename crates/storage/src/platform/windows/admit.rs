//! Admitting an override profile before a process's first write.
//!
//! A Gate or debug build with the override pair set holds the chain of both roots, from the
//! volume root down, for the process's whole life. It then makes or opens the product folders
//! under them with the checked create, and checks each write-sensitive file a writer will open,
//! so a link, a hard link or a reused folder anywhere on those paths refuses before anything is
//! written through it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use kumochi_protocol::constants::{
    AUTHORITY_FILE, CLAUDE_CODE_LOCK, CONFIG_FILE, DATA_DIR, LOGS_DIR, SESSION_LOCK, STORE_FILE,
};

use super::folders::{AppDataError, Pair};
use super::protected::{check_leaf, hold_chain, owner_only_dir};
use crate::platform::Untrusted;

/// The suffixes SQLite gives a database's side files, the database itself first.
const SQLITE_FILES: [&str; 4] = ["", "-wal", "-shm", "-journal"];

/// The pair this process admitted and the handles that keep it in place.
#[derive(Debug)]
struct Admitted {
    pair: Pair,
    _held: Vec<File>,
}

static ADMITTED: OnceLock<Result<Admitted, AppDataError>> = OnceLock::new();

/// Admits `pair` once per process, the first time any row resolves through it.
pub(crate) fn admitted(pair: &Pair) -> Result<(), AppDataError> {
    match ADMITTED.get_or_init(|| admit(pair)) {
        Ok(admitted) if admitted.pair == *pair => Ok(()),
        _ => Err(AppDataError::InvalidOverride),
    }
}

/// Checks the file at `path` under an admitted override profile before its first writer opens
/// it. It does nothing in a process that admitted no override.
///
/// # Errors
/// `InvalidOverride` when the file is a link, has another name, or isn't a plain file.
pub fn admit_leaf(path: &Path) -> Result<(), AppDataError> {
    match ADMITTED.get() {
        Some(Ok(_)) => check_leaf(path).map_err(refused),
        _ => Ok(()),
    }
}

fn admit(pair: &Pair) -> Result<Admitted, AppDataError> {
    let mut held = hold_chain(&pair.local).map_err(refused)?;
    held.extend(hold_chain(&pair.roaming).map_err(refused)?);
    let local = pair.local.join(DATA_DIR);
    let roaming = pair.roaming.join(DATA_DIR);
    let logs = local.join(LOGS_DIR);
    for folder in [&local, &roaming, &logs] {
        held.push(owner_only_dir(folder).map_err(refused)?);
    }
    let mut leaves: Vec<PathBuf> = [STORE_FILE, AUTHORITY_FILE]
        .into_iter()
        .flat_map(|db| SQLITE_FILES.map(|suffix| local.join(format!("{db}{suffix}"))))
        .collect();
    leaves.extend([
        local.join(SESSION_LOCK),
        local.join(CLAUDE_CODE_LOCK),
        roaming.join(CONFIG_FILE),
        roaming.join(format!("{CONFIG_FILE}.tmp")),
    ]);
    for entry in std::fs::read_dir(&logs).map_err(|_| AppDataError::InvalidOverride)? {
        let entry = entry.map_err(|_| AppDataError::InvalidOverride)?;
        leaves.push(logs.join(entry.file_name()));
    }
    for leaf in &leaves {
        check_leaf(leaf).map_err(refused)?;
    }
    Ok(Admitted {
        pair: pair.clone(),
        _held: held,
    })
}

fn refused(_: Untrusted) -> AppDataError {
    AppDataError::InvalidOverride
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use kumochi_protocol::constants::{
        CLAUDE_CODE_LOCK, CONFIG_FILE, DATA_DIR, LOGS_DIR, SESSION_LOCK, STORE_FILE,
    };
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

    use super::{AppDataError, Pair, admit};
    use crate::platform::windows::protected::{known_folder, set_junction};
    use crate::scratch::Scratch;

    /// A run root with existing, separate `local` and `roaming` children, and a disposable
    /// target folder beside them that a link can point at.
    struct Run {
        _scratch: Scratch,
        pair: Pair,
        target: std::path::PathBuf,
    }

    fn run(name: &str) -> Run {
        let local = known_folder(FOLDERID_LocalAppData).expect("local app data");
        let scratch = Scratch::under(&local, name);
        let pair = Pair {
            local: scratch.join("local"),
            roaming: scratch.join("roaming"),
        };
        let target = scratch.join("target");
        for folder in [&pair.local, &pair.roaming, &target] {
            fs::create_dir(folder).expect("a scratch folder");
        }
        fs::write(target.join("kept"), b"target contents").expect("a target file");
        Run {
            _scratch: scratch,
            pair,
            target,
        }
    }

    impl Run {
        fn junction(&self, at: &Path) {
            fs::create_dir(at).expect("an empty folder for the junction");
            set_junction(at, &self.target).expect("a junction to the target");
        }

        /// Refused, with the target exactly as it was made.
        fn assert_refused(&self) {
            assert_eq!(admit(&self.pair).err(), Some(AppDataError::InvalidOverride));
            let entries: Vec<_> = fs::read_dir(&self.target)
                .expect("the target lists")
                .map(|entry| entry.expect("an entry").file_name())
                .collect();
            assert_eq!(entries, ["kept"], "nothing was made in the target");
            assert_eq!(
                fs::read(self.target.join("kept")).expect("the target file"),
                b"target contents"
            );
        }
    }

    #[test]
    fn a_fresh_pair_gets_its_product_folders_and_a_reused_plain_one_passes() {
        let run = run("admit-fresh");
        drop(admit(&run.pair).expect("a fresh pair is admitted"));
        let local = run.pair.local.join(DATA_DIR);
        assert!(local.join(LOGS_DIR).is_dir(), "the logs folder");
        assert!(
            run.pair.roaming.join(DATA_DIR).is_dir(),
            "the settings folder"
        );
        fs::write(local.join(STORE_FILE), b"store").expect("a store");
        fs::write(local.join(SESSION_LOCK), b"").expect("a lock");
        fs::write(local.join(LOGS_DIR).join("brain.log"), b"log").expect("a log");
        fs::write(run.pair.roaming.join(DATA_DIR).join(CONFIG_FILE), b"").expect("settings");
        drop(admit(&run.pair).expect("a reused profile of plain files is admitted"));
    }

    #[test]
    fn a_linked_override_root_refuses() {
        let run = run("admit-linked-root");
        fs::remove_dir(&run.pair.local).expect("the plain local folder removed");
        run.junction(&run.pair.local);
        run.assert_refused();
    }

    #[test]
    fn a_junction_at_the_product_folder_refuses() {
        let run = run("admit-linked-data");
        run.junction(&run.pair.local.join(DATA_DIR));
        run.assert_refused();
    }

    #[test]
    fn a_linked_logs_folder_refuses() {
        let run = run("admit-linked-logs");
        let data = run.pair.local.join(DATA_DIR);
        fs::create_dir(&data).expect("the product folder");
        run.junction(&data.join(LOGS_DIR));
        run.assert_refused();
    }

    #[test]
    fn a_hard_linked_store_refuses() {
        let run = run("admit-hard-link");
        let data = run.pair.local.join(DATA_DIR);
        fs::create_dir(&data).expect("the product folder");
        fs::hard_link(run.target.join("kept"), data.join(STORE_FILE)).expect("a hard link");
        run.assert_refused();
    }

    #[test]
    fn a_hard_linked_settings_temp_file_refuses() {
        let run = run("admit-linked-settings-temp");
        let settings = run.pair.roaming.join(DATA_DIR);
        fs::create_dir(&settings).expect("the settings folder");
        fs::hard_link(
            run.target.join("kept"),
            settings.join(format!("{CONFIG_FILE}.tmp")),
        )
        .expect("a hard link");
        run.assert_refused();
    }

    #[test]
    fn a_hard_linked_claude_code_lock_refuses() {
        let run = run("admit-linked-claude-lock");
        let data = run.pair.local.join(DATA_DIR);
        fs::create_dir(&data).expect("the product folder");
        fs::hard_link(run.target.join("kept"), data.join(CLAUDE_CODE_LOCK)).expect("a hard link");
        run.assert_refused();
    }

    #[test]
    fn a_linked_session_lock_refuses() {
        let run = run("admit-linked-lock");
        let data = run.pair.local.join(DATA_DIR);
        fs::create_dir(&data).expect("the product folder");
        let lock = data.join(SESSION_LOCK);
        // A file symlink needs Developer Mode or the privilege; a junction is the same reparse leaf.
        if std::os::windows::fs::symlink_file(run.target.join("kept"), &lock).is_ok() {
            eprintln!("session.lock witness: symlink");
        } else {
            eprintln!("session.lock witness: junction");
            run.junction(&lock);
        }
        run.assert_refused();
    }
}
