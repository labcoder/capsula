//! What a screen hold carried into its turn, for its history line (docs/development/store.md).

use kumochi_protocol::message::{HeldScreen, RunId, ScreenClass};

use super::StoreError;

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
    /// Marks `run`'s screen `withdrawn` while its picture is pending: the user withdrew
    /// the image before any send carrying it was granted.
    ///
    /// # Errors
    /// When the run's id doesn't fit SQL, or the write fails.
    pub(crate) fn withdraw_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        self.unsent_screen(run, Some(column(HeldScreen::Withdrawn)))
    }

    /// Clears `run`'s screen while its picture is pending: the run ended before any send
    /// carrying the image was granted.
    ///
    /// # Errors
    /// When the run's id doesn't fit SQL, or the write fails.
    pub(crate) fn forget_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        self.unsent_screen(run, None)
    }

    /// Marks `run`'s screen `model_cant_see` while its picture is pending: its model
    /// stopped taking pictures before any send carrying the image was granted.
    ///
    /// # Errors
    /// When the run's id doesn't fit SQL, or the write fails.
    pub(crate) fn cant_see_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        let to = column(HeldScreen::NotIncluded(ScreenClass::ModelCantSee));
        self.unsent_screen(run, Some(to))
    }

    /// Marks `run`'s pending picture sent: a send carrying it may have written some of it.
    ///
    /// # Errors
    /// When the run's id doesn't fit SQL, or the write fails.
    pub(crate) fn sent_screen(&mut self, run: RunId) -> Result<(), StoreError> {
        sent(&self.conn, Some(super::sql_id(run.0, StoreError::NoRun)?))?;
        Ok(())
    }

    fn unsent_screen(&mut self, run: RunId, to: Option<&str>) -> Result<(), StoreError> {
        let run = super::sql_id(run.0, StoreError::NoRun)?;
        self.conn.execute(
            "UPDATE runs SET screen = ?2 WHERE id = ?1 AND screen IN (?3, ?4)",
            rusqlite::params![run, to, PENDING_ATTACHED, PENDING_SCREEN],
        )?;
        Ok(())
    }
}

/// What a turn's row says while its picture may still go: no line reads it as sent, and the send's
/// settlement or recovery resolves it.
pub(super) fn begun(screen: HeldScreen) -> &'static str {
    match screen {
        HeldScreen::Attached => PENDING_ATTACHED,
        HeldScreen::Screen => PENDING_SCREEN,
        other => column(other),
    }
}

const PENDING_ATTACHED: &str = "pending_attached";
const PENDING_SCREEN: &str = "pending_screen";

/// Turns `run`'s pending picture, or every run's when `run` is `None`, into the mark that says
/// it was sent.
pub(super) fn sent(conn: &rusqlite::Connection, run: Option<i64>) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE runs SET screen = CASE screen WHEN ?2 THEN ?3 ELSE ?5 END
         WHERE (?1 IS NULL OR id = ?1) AND screen IN (?2, ?4)",
        rusqlite::params![
            run,
            PENDING_ATTACHED,
            column(HeldScreen::Attached),
            PENDING_SCREEN,
            column(HeldScreen::Screen)
        ],
    )
}

/// The stored value; one this build doesn't know reads as none, so no line claims a window.
pub(super) fn read(column_text: &str) -> Option<HeldScreen> {
    ALL.into_iter()
        .find(|&screen| column(screen) == column_text)
}
