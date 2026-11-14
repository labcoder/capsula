//! What a screen hold carried into its turn, for its history line (docs/development/store.md).

use kumochi_protocol::message::{HeldScreen, ScreenClass};

/// Every value the `screen` column takes; `read` knows only these.
const ALL: [HeldScreen; 8] = [
    HeldScreen::Attached,
    HeldScreen::NotIncluded(ScreenClass::NotReady),
    HeldScreen::NotIncluded(ScreenClass::NothingThere),
    HeldScreen::NotIncluded(ScreenClass::CantShare),
    HeldScreen::NotIncluded(ScreenClass::Unreadable),
    HeldScreen::NotIncluded(ScreenClass::TooLarge),
    HeldScreen::NotIncluded(ScreenClass::Changed),
    HeldScreen::NotIncluded(ScreenClass::ModelCantSee),
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
    }
}

/// The stored value; one this build doesn't know reads as none, so no line claims a window.
pub(super) fn read(column_text: &str) -> Option<HeldScreen> {
    ALL.into_iter()
        .find(|&screen| column(screen) == column_text)
}
