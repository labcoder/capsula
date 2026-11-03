//! An in-memory credential store for tests: it can refuse, or "crash" after, any mutation.

use std::collections::BTreeMap;
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
    pub(crate) denied_read: Option<String>,
    /// Reads of present names that answer "not found", as Rez's store did under concurrency.
    pub(crate) missed_reads: usize,
    /// Deletes of present names that succeed but leave the name, as a parallel delete did.
    pub(crate) sticky_deletes: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FakeStore(pub(crate) Arc<Mutex<FakeState>>);

impl FakeStore {
    pub(crate) fn fork(&self) -> Self {
        let values = self.0.lock().expect("fake lock").values.clone();
        Self(Arc::new(Mutex::new(FakeState {
            values,
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
            let index = state.mutations.len();
            state.mutations.push(value.map_or_else(
                || Mutation::Delete(target.to_owned()),
                |_| Mutation::Set(target.to_owned()),
            ));
            let fault = if state.refusing {
                Some(Fault::Refuse)
            } else if state.fault.is_some_and(|(at, _)| at == index) {
                state.fault.take().map(|(_, fault)| fault)
            } else {
                None
            };
            if !matches!(fault, Some(Fault::Refuse)) {
                if let Some(value) = value {
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
