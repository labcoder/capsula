//! The image gate on every route: whether a hold shows its model the screen now, or why not.
//! `ChatGPT` reads its account's catalog; a Claude route reads its own availability, so its
//! switch and its fresh fact for the chosen model decide (docs/development/screen.md, "The
//! brain's hold client").

use kumochi_protocol::message::{
    ConnectionKind, ModelChoice, NoticeRevision, ScreenHolds, ScreenOff,
};

use crate::coordinator::{Availability, PictureRoutes};

impl super::Brain {
    /// The gate for a hold on `choice`, with `capture` whether this platform and its worker can
    /// capture that kind of hold and `enabled` the user's switch for it.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the platform's capture and the user's switch for that kind of hold"
    )]
    pub(super) fn picture_gate(
        &self,
        choice: Option<&ModelChoice>,
        capture: bool,
        enabled: bool,
    ) -> ScreenHolds {
        match choice {
            Some(choice) if choice.connection != ConnectionKind::ChatGpt => claude(
                self.coordinator.availability_of(choice.connection),
                choice,
                self.coordinator.pictures(),
                capture,
                enabled,
            ),
            _ => self.account.screen_holds(choice, capture, enabled),
        }
    }
}

/// The gate on a Claude route, checked in `Accounts::screen_holds`' order: capture, the user's
/// switch, a ready route, then the chosen model listed there with the route's switch on and the
/// route's fresh fact that it takes pictures.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the caller supplies the platform's capture and the user's switch"
)]
pub(super) fn claude(
    availability: &Availability,
    choice: &ModelChoice,
    pictures: PictureRoutes,
    capture: bool,
    enabled: bool,
) -> ScreenHolds {
    if !capture {
        return ScreenHolds::Off(ScreenOff::Unsupported);
    }
    if !enabled {
        return ScreenHolds::Off(ScreenOff::TurnedOff);
    }
    let Availability::Ready(ready) = availability else {
        return ScreenHolds::Off(ScreenOff::NotReady);
    };
    ready
        .models
        .iter()
        .find(|info| info.slug == choice.model)
        .filter(|_| pictures.allows(choice.connection) && ready.images.contains(&choice.model))
        .map_or(ScreenHolds::Off(ScreenOff::ModelCantSee), |info| {
            ScreenHolds::On {
                model: info.display_name.clone(),
                connection: choice.connection,
                slug: choice.model.clone(),
                revision: NoticeRevision::FIRST,
            }
        })
}

#[cfg(test)]
#[path = "screen_gate_tests.rs"]
mod tests;
