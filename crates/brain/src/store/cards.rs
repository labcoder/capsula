//! Snapshot card reconstruction and invalidation at the existing deletion boundary.

use std::sync::{Arc, Weak};

use kumochi_protocol::message::IssueCard;
use rusqlite::{Connection, params};

use super::{Side, StoreError, actions, context::Snapshot};
use crate::coordinator::{CardRead, Cards};

impl Side {
    pub(crate) fn register_cards(&self, cards: &Arc<Cards>) -> Result<(), StoreError> {
        let mut batches = self.cards.lock().map_err(|_| StoreError::Issue)?;
        batches.retain(|batch| {
            batch
                .upgrade()
                .is_some_and(|batch| batch.key.control.check().is_ok())
        });
        if batches.len() >= kumochi_protocol::message::MAX_PENDING_PAGES {
            return Err(StoreError::Issue);
        }
        batches.push(Arc::downgrade(cards));
        Ok(())
    }

    pub(super) fn delete_cards(&self, conn: &Connection, thread: i64) -> Result<(), StoreError> {
        let batches: Vec<_> = self
            .cards
            .lock()
            .map_err(|_| StoreError::Issue)?
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for batch in batches {
            for source in &batch.sources {
                let Ok(draft) = i64::try_from(source.draft.0) else {
                    continue;
                };
                let deleted: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM actions WHERE id = ?1 AND thread = ?2)",
                    params![draft, thread],
                    |row| row.get(0),
                )?;
                if deleted {
                    source.delete();
                }
            }
        }
        Ok(())
    }
}

impl Snapshot<'_> {
    pub(crate) fn cards(&self, batch: &Cards) -> Result<Vec<CardRead>, StoreError> {
        let mut cards = Vec::new();
        for source in &batch.sources {
            batch.key.control.check().map_err(StoreError::from)?;
            let Ok(id) = i64::try_from(source.draft.0) else {
                source.delete();
                continue;
            };
            let Some(action) = actions::load(&self.0, id)? else {
                source.delete();
                continue;
            };
            let Some(generation) = self.generation(action.thread)? else {
                source.delete();
                continue;
            };
            if !source.bind(action.thread, generation) {
                continue;
            }
            let card = IssueCard {
                hash: action.hash(),
                state: action.state(),
                draft: action.draft,
                thread: action.thread,
                repo: action.repo,
                account: action.account.map(|account| account.login),
                title: action.title,
                body: action.body,
                approval: action.approval,
                version: action.version,
            };
            cards.push(CardRead {
                card,
                generation,
                source: Arc::clone(source),
            });
        }
        Ok(cards)
    }
}

#[cfg(test)]
mod tests;
