//! What a screen hold carried into its turn, for its history line (docs/development/store.md).

use kumochi_protocol::message::{HeldScreen, RunId, ScreenClass, ThreadId};
use rusqlite::OptionalExtension as _;

use super::StoreError;

const CORRECT_SCREEN: &str = "UPDATE runs SET screen = ?2 WHERE id = ?1
    AND screen IN ('pending_attached', 'pending_screen', 'granted_attached', 'granted_screen')";

/// A held picture proved unsent, and what should replace its pending or authorized class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsentScreen {
    /// No picture line remains.
    Clear,
    /// The user withdrew the picture.
    Withdrawn,
    /// Its chosen model stopped taking pictures.
    ModelCantSee,
}

impl UnsentScreen {
    pub(super) fn column(self) -> Option<&'static str> {
        match self {
            Self::Clear => None,
            Self::Withdrawn => Some(column(HeldScreen::Withdrawn)),
            Self::ModelCantSee => Some(column(HeldScreen::NotIncluded(ScreenClass::ModelCantSee))),
        }
    }
}

/// Every value the `screen` column takes; `read` knows only these.
const ALL: [HeldScreen; 10] = [
    HeldScreen::Attached,
    HeldScreen::NotIncluded(ScreenClass::NotReady),
    HeldScreen::NotIncluded(ScreenClass::NothingThere),
    HeldScreen::NotIncluded(ScreenClass::CantShare),
    HeldScreen::NotIncluded(ScreenClass::Unreadable),
    HeldScreen::NotIncluded(ScreenClass::TooLarge),
    HeldScreen::NotIncluded(ScreenClass::Changed),
    HeldScreen::NotIncluded(ScreenClass::ModelCantSee),
    HeldScreen::Withdrawn,
    HeldScreen::Screen,
];

/// The column's text for `screen`.
pub(super) fn column(screen: HeldScreen) -> &'static str {
    match screen {
        HeldScreen::Attached => "attached",
        HeldScreen::NotIncluded(ScreenClass::NotReady) => "not_ready",
        HeldScreen::NotIncluded(ScreenClass::NothingThere) => "nothing_there",
        HeldScreen::NotIncluded(ScreenClass::CantShare) => "cant_share",
        HeldScreen::NotIncluded(ScreenClass::Unreadable) => "unreadable",
        HeldScreen::NotIncluded(ScreenClass::TooLarge) => "too_large",
        HeldScreen::NotIncluded(ScreenClass::Changed) => "changed",
        HeldScreen::NotIncluded(ScreenClass::ModelCantSee) => "model_cant_see",
        HeldScreen::Withdrawn => "withdrawn",
        HeldScreen::Screen => "screen",
    }
}

impl super::Store {
    /// Commits the authorization before releasing the first transport grant for this picture.
    /// A failed restore can leave the same class already durable. The coordinator only asks
    /// again once it knows the previous authorization did not produce a write.
    pub(crate) fn grant_screen(&mut self, run: RunId) -> Result<bool, StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        Ok(self.conn.execute(
            "UPDATE runs SET screen = CASE WHEN screen IN ('pending_attached', 'granted_attached')
                                           THEN 'granted_attached' ELSE 'granted_screen' END
             WHERE id = ?1 AND state = 'active'
             AND screen IN ('pending_attached', 'pending_screen', 'granted_attached', 'granted_screen')",
            [run],
        )? == 1)
    }

    /// Restores a never-written authorization to pending so the same picture can try again.
    pub(crate) fn restore_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        self.conn.execute(
            "UPDATE runs SET screen = CASE screen WHEN 'granted_attached' THEN 'pending_attached'
                                                   ELSE 'pending_screen' END
             WHERE id = ?1 AND screen IN ('granted_attached', 'granted_screen')",
            [run],
        )?;
        Ok(())
    }

    /// Marks an authorized picture sent: its transport may have attempted a write.
    ///
    /// # Errors
    /// When the run's id doesn't fit SQL, or the write fails.
    pub(crate) fn sent_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        self.conn.execute(
            "UPDATE runs SET screen = CASE screen WHEN 'granted_attached' THEN 'attached'
                                                   ELSE 'screen' END
             WHERE id = ?1 AND screen IN ('granted_attached', 'granted_screen')",
            [run],
        )?;
        Ok(())
    }

    /// Records a transport write attempt for an offline history fixture.
    ///
    /// # Errors
    /// Returns an error if the run cannot authorize a picture or the store write fails.
    #[cfg(feature = "test-support")]
    pub fn record_sent_screen_for_tests(&mut self, run: RunId) -> Result<(), StoreError> {
        if !self.grant_screen(run)? {
            return Err(StoreError::NoRun);
        }
        self.sent_screen(run)
    }

    /// Corrects a known-unsent picture. An already corrected or absent row is a success.
    pub(crate) fn unsent_screen(&mut self, run: RunId, to: UnsentScreen) -> Result<(), StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        self.conn
            .execute(CORRECT_SCREEN, rusqlite::params![run, to.column()])?;
        Ok(())
    }

    /// Repairs a deferred correction and returns the live thread only if its row changed.
    pub(crate) fn repair_screen(
        &mut self,
        run: RunId,
        to: UnsentScreen,
    ) -> Result<Option<ThreadId>, StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        let tx = self.conn.transaction()?;
        let changed = tx.execute(CORRECT_SCREEN, rusqlite::params![run, to.column()])?;
        let thread = if changed == 0 {
            None
        } else {
            tx.query_row(
                "SELECT r.thread FROM runs r JOIN threads t ON r.thread = t.id
                 WHERE r.id = ?1 AND t.state = 'live'",
                [run],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|id| ThreadId(super::row_id(id)))
        };
        tx.commit()?;
        Ok(thread)
    }
}

/// What a turn's row says while its picture may still go: no line reads it as sent, and the send's
/// settlement or recovery resolves it.
pub(super) fn begun(screen: HeldScreen) -> &'static str {
    match screen {
        HeldScreen::Attached => "pending_attached",
        HeldScreen::Screen => "pending_screen",
        other => column(other),
    }
}

/// No transport is left to settle an authorization. Only a durable grant may have written.
pub(super) fn recover(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE runs SET screen = CASE screen WHEN 'granted_attached' THEN 'attached'
                                               WHEN 'granted_screen' THEN 'screen' END
         WHERE screen IN ('pending_attached', 'pending_screen', 'granted_attached', 'granted_screen')",
        [],
    )
}

/// A pending picture never reads as sent. An unsettled authorization reads conservatively
/// once the run ends, as it would after recovery.
pub(super) fn shown(column_text: &str, active: Option<bool>) -> Option<HeldScreen> {
    match column_text {
        "granted_attached" if active == Some(false) => Some(HeldScreen::Attached),
        "granted_screen" if active == Some(false) => Some(HeldScreen::Screen),
        other => ALL.into_iter().find(|&screen| column(screen) == other),
    }
}
