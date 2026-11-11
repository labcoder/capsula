//! Model storage uses ordinary physical local paths only. Every adapter must reject network
//! locations, redirected ancestors, reparse/symbolic links and multiply named writable files without
//! following them or changing existing data. Checked parents stay bound against renaming;
//! checked files stay bound against mutation and replacement while constructors consume paths.
//! Read-only hard-linked finals are allowed only under those same held-file guarantees.
//! Writable opens never truncate before checking ownership, aliases and exclusive access.
//! Publication renames the checked partial handle into a held parent without replacing a new
//! arrival. Callers preflight every final and partial before deleting, truncating or writing.
//! Installation denies write sharing on its owned model root and part directory until every
//! needed partial handle is acquired, so an empty directory cannot redirect creation. As with the
//! shared primitive, medium-integrity code running as this user can already change the brain,
//! so it isn't defended against here.

use std::fs::File;
use std::path::Path;

use super::{Untrusted, hold_chain, model_file, validate_model_path};

pub(crate) struct ModelFiles {
    pub(crate) files: Vec<File>,
    _parents: Vec<File>,
}

impl ModelFiles {
    pub(crate) fn open(dir: &Path, names: &[&str]) -> Result<Self, Untrusted> {
        validate_model_path(dir)?;
        let parents = hold_chain(dir)?;
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
