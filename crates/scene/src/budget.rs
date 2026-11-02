//! The local admission generation, extended with expanded-work charges by the budget layer.

use std::sync::Arc;

/// Shared admission context for visible scenes in one process.
///
/// An admission is valid only for this instance and its current generation.
#[derive(Debug)]
pub struct GlobalBudget {
    pub(crate) identity: Arc<()>,
    generation: u64,
}

impl Default for GlobalBudget {
    fn default() -> Self {
        Self {
            identity: Arc::new(()),
            generation: 0,
        }
    }
}
impl GlobalBudget {
    /// Returns the local generation used to reject stale admissions.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn advance(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("budget changes cannot exhaust u64 within a process lifetime");
    }
}
