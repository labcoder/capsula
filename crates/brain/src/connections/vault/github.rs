use super::{Vault, generation};
use crate::connections::authority::{self, Kind};
use crate::connections::github::{
    device::{Account, Removed},
    lane::{Job, LoadError, Reply},
    record,
};
use crate::platform::credential::CredentialStore;
use std::sync::atomic::Ordering;

impl<S: CredentialStore> Vault<S> {
    pub(crate) fn github(&mut self, job: Job) -> Reply {
        match job {
            Job::Load { stamp } => Reply::Loaded {
                stamp,
                result: self.load_github(),
            },
            Job::Save {
                stamp,
                fence,
                account,
            } => {
                let saved = record::encode(&account).is_ok_and(|bytes| {
                    fence.load(Ordering::Acquire) == stamp.generation
                        && self.write(Kind::GitHub, &bytes, None).is_ok()
                });
                Reply::Saved { stamp, saved }
            }
            Job::Remove { generation } => Reply::Removed {
                generation,
                result: self.remove_github(),
            },
        }
    }

    fn load_github(&mut self) -> Result<Option<Account>, LoadError> {
        let cleanup = self.cleanup_github();
        let bytes = self
            .read_bytes(Kind::GitHub)
            .map_err(|_| LoadError::Unavailable)?;
        let Some(bytes) = bytes else {
            return match cleanup {
                Removed::Clean => Ok(None),
                other => Err(LoadError::Cleanup(other)),
            };
        };
        match record::decode(&bytes) {
            Ok(account) => Ok(Some(account)),
            Err(_) => Err(LoadError::Cleanup(self.remove_github())),
        }
    }

    fn remove_github(&mut self) -> Removed {
        if self.authority.remove(&[Kind::GitHub], None, None).is_err() {
            return Removed::NotDurable;
        }
        self.cleanup_github()
    }

    fn cleanup_github(&mut self) -> Removed {
        let Ok(rows) = self.authority.retired_kind(Kind::GitHub) else {
            return Removed::Refused;
        };
        let mut remains = false;
        for row in rows {
            let observed = generation::sweep(&self.store, &self.service, &row);
            remains |= observed == authority::Observed::Remains;
            remains |= self.authority.observed(&row, observed).is_err();
        }
        if remains {
            Removed::Refused
        } else {
            Removed::Clean
        }
    }
}
