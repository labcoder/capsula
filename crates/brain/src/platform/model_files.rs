//! Model storage uses ordinary physical local paths only. Every adapter must reject network
//! locations, redirected ancestors, reparse/symbolic links and multiply named writable files without
//! following them or changing existing data. On Windows, checked parents stay bound against renaming;
//! checked files stay bound against mutation and replacement while constructors consume paths.
//! Read-only hard-linked finals are allowed only under those same held-file guarantees.
//! Writable opens never truncate before checking ownership, aliases and exclusive access.
//! Publication renames the checked partial handle into a held parent without replacing a new
//! arrival. Callers preflight every final and partial before deleting, truncating or writing.
//! Installation denies write sharing on its owned model root and part directory until every
//! needed partial handle is acquired, so an empty directory cannot redirect creation. As with the
//! shared primitive, medium-integrity code running as this user can already change the brain,
//! so it isn't defended against here.
//!
//! macOS can't deny sharing: a descriptor holds an object, not its name. Its adapter, dormant
//! until activation, narrows the contract instead. Every model path is below the admitted
//! Application Support anchor, every folder below it is this user's at 0700 with no ACL on the
//! anchor's volume, each entry is looked at before it's opened, and one cooperating writer performs
//! every install. An entry that already exists is admitted on its present metadata, not proven to
//! have been made that way. Partials are created exclusively at 0600 and `flock`ed. Finals are sealed 0400 with
//! one link before they're renamed into place, so a hard-linked final refuses there. Each step
//! that relies on a name re-checks it against the held descriptor, and a valid final is kept
//! only once its name still gives it.
//!
//! shortcut: on macOS this user can rename or swap a checked name between two calls, and a swap to
//! another valid sealed final isn't detected; upgrade if a non-cooperating same-user writer enters
//! the threat model, or if storage made before first use must be proven private.

use std::fs::File;
use std::path::Path;

use super::{Untrusted, hold_model_chain, model_file, validate_model_path};

pub(crate) struct ModelFiles {
    pub(crate) files: Vec<File>,
    _parents: Vec<File>,
}

impl ModelFiles {
    pub(crate) fn open(dir: &Path, names: &[&str]) -> Result<Self, Untrusted> {
        validate_model_path(dir)?;
        let parents = hold_model_chain(dir)?;
        let files = names
            .iter()
            .map(|name| model_file(&dir.join(name)))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            files,
            _parents: parents,
        })
    }
}
