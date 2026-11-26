//! An in-memory credential store for tests: it can refuse, or "crash" after, any mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};


use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Fault {
    Refuse,
    CrashAfter,
}

#[derive(Debug, Clone)]
pub(crate) enum Mutation {
    Set(String),
    Delete(String),
}

#[derive(Debug, Default)]
pub(crate) struct FakeState {
    pub(crate) values: BTreeMap<String, SecretBytes>,
    pub(crate) mutations: Vec<Mutation>,
    pub(crate) fault: Option<(usize, Fault)>,
    /// Refuse every mutation while set, as a Credential Manager that keeps failing.
    pub(crate) refusing: bool,
    pub(crate) refuse_deletes: bool,
    pub(crate) denied_read: Option<String>,
    /// Reads of present names that answer "not found", as Rez's store did under concurrency.
    pub(crate) missed_reads: usize,
    /// Deletes of present names that succeed but leave the name, as a parallel delete did.
    pub(crate) sticky_deletes: usize,
    /// The next write moves this clock forward by these seconds, as a slow save does.
    pub(crate) slow_write: Option<(Arc<AtomicU64>, u64)>,
    /// Fail the test on any second write of a name, even after a delete (OSC-233 write-once).
    pub(crate) write_once: bool,
    /// While set, each mutation first notes the marker the authority file there holds, so a
    /// trace shows which control state every store mutation ran under.
    pub(crate) note_markers: Option<(PathBuf, Vec<String>)>,
    written: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FakeStore(pub(crate) Arc<Mutex<FakeState>>);

impl FakeStore {
    /// A copy of the values and the write-once history, with no faults armed.
    pub(crate) fn fork(&self) -> Self {
        let state = self.0.lock().expect("fake lock");
        Self(Arc::new(Mutex::new(FakeState {
            values: state.values.clone(),
            write_once: state.write_once,
            written: state.written.clone(),
            ..FakeState::default()
        })))
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("fake lock")
            .values
            .keys()
            .cloned()
            .collect()
    }

    /// Every name's value now, for a later [`Self::rollback`].
    pub(crate) fn snapshot(&self) -> BTreeMap<String, SecretBytes> {
        self.0.lock().expect("fake lock").values.clone()
    }

    /// Puts these names back as `snapshot` had them, as a store that serves a stale copy does:
    /// a confirmed delete comes back, or a fresh write reads as absent. It isn't a mutation.
    pub(crate) fn rollback(&self, snapshot: &BTreeMap<String, SecretBytes>, names: &[String]) {
        let mut state = self.0.lock().expect("fake lock");
        for name in names {
            match snapshot.get(name) {
                Some(value) => state.values.insert(name.clone(), value.clone()),
                None => state.values.remove(name),
            };
        }
    }

    pub(crate) fn arm(&self, index: usize, fault: Fault) {
        let mut state = self.0.lock().expect("fake lock");
        state.mutations.clear();
        state.fault = Some((index, fault));
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "fault injection deliberately models process death after a durable mutation"
    )]
    fn mutate(
        &self,
        target: &str,
        value: Option<&SecretBytes>,
    ) -> Result<(), CredentialStoreError> {
        let fault = {
            let mut state = self.0.lock().expect("fake lock");
            if value.is_some() && state.write_once {
                let fresh = state.written.insert(target.to_owned());
                assert!(fresh, "a write-once credential name was written twice");
            }
            if let Some((path, notes)) = state.note_markers.as_mut() {
                let marker = rusqlite::Connection::open(&*path)
                    .and_then(|conn| {
                        conn.query_row("SELECT marker FROM control WHERE id = 1", [], |row| {
                            row.get(0)
                        })
                    })
                    .expect("the authority's marker");
                notes.push(marker);
            }
            let index = state.mutations.len();
            state.mutations.push(value.map_or_else(
                || Mutation::Delete(target.to_owned()),
                |_| Mutation::Set(target.to_owned()),
            ));
            let fault = if state.refusing || (state.refuse_deletes && value.is_none()) {
                Some(Fault::Refuse)
            } else if state.fault.is_some_and(|(at, _)| at == index) {
                state.fault.take().map(|(_, fault)| fault)
            } else {
                None
            };
            if !matches!(fault, Some(Fault::Refuse)) {
                if let Some(value) = value {
                    if let Some((clock, seconds)) = state.slow_write.take() {
                        clock.fetch_add(seconds, Ordering::SeqCst);
                    }
                    state.values.insert(target.to_owned(), value.clone());
                } else if state.sticky_deletes > 0 && state.values.contains_key(target) {
                    state.sticky_deletes -= 1;
                } else {
                    state.values.remove(target);
                }
            }
            fault
        };
        match fault {
            Some(Fault::Refuse) => Err(if value.is_some() {
                CredentialStoreError::Write(5)
            } else {
                CredentialStoreError::Delete(5)
            }),
            Some(Fault::CrashAfter) => panic!("simulated process crash"),
            None => Ok(()),
        }
    }
}

impl CredentialStore for FakeStore {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        let mut state = self.0.lock().expect("fake lock");
        if state.denied_read.as_deref() == Some(target) {
            return Err(CredentialStoreError::Read(5));
        }
        let value = state.values.get(target).cloned();
        if value.is_some() && state.missed_reads > 0 {
            state.missed_reads -= 1;
            return Ok(None);
        }
        Ok(value)
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.mutate(target, Some(secret))
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        self.mutate(target, None)
    }
}
