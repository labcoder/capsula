//! Jobs on the existing credential worker, never on a second credential executor.

use super::device::{Account, Removed, Stamp};
use std::{
    fmt,
    sync::{Arc, atomic::AtomicU64},
};

pub(crate) enum Job {
    Load {
        stamp: Stamp,
    },
    Save {
        stamp: Stamp,
        fence: Arc<AtomicU64>,
        account: Account,
    },
    Remove {
        generation: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LoadError {
    #[error("github credentials unavailable")]
    Unavailable,
    #[error("github record required cleanup")]
    Cleanup(Removed),
}

pub(crate) enum Reply {
    Loaded {
        stamp: Stamp,
        result: Result<Option<Account>, LoadError>,
    },
    Saved {
        stamp: Stamp,
        saved: bool,
    },
    Removed {
        generation: u64,
        result: Removed,
    },
}

impl Job {
    pub(crate) fn unavailable(self) -> Reply {
        match self {
            Self::Load { stamp } => Reply::Loaded {
                stamp,
                result: Err(LoadError::Unavailable),
            },
            Self::Save { stamp, .. } => Reply::Saved {
                stamp,
                saved: false,
            },
            Self::Remove { generation } => Reply::Removed {
                generation,
                result: Removed::NotDurable,
            },
        }
    }
}
impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubCredentialJob")
    }
}
impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubCredentialReply")
    }
}
