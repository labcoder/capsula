//! Status visibility decisions, separate from native placement and pointer timing.

use super::{Presence, geometry::PanelVisibility};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Pointer,
    Typing,
    Settings,
    Stop,
}

impl Presence {
    pub(super) fn panel_owner(&self) -> Owner {
        if self.settings_open() {
            Owner::Settings
        } else if self.text_open() || self.threads_open() {
            Owner::Typing
        } else if self.escape_hold.shows_stop() {
            Owner::Stop
        } else {
            Owner::Pointer
        }
    }
}

pub(super) fn pointer(current: PanelVisibility, owner: Owner) -> PanelVisibility {
    match owner {
        Owner::Typing | Owner::Settings => PanelVisibility::Hidden,
        Owner::Stop => PanelVisibility::Visible,
        Owner::Pointer => current,
    }
}

pub(super) fn pressed(current: PanelVisibility, owner: Owner) -> PanelVisibility {
    match (owner, current) {
        (Owner::Typing | Owner::Settings, _) | (Owner::Pointer, PanelVisibility::Visible) => {
            PanelVisibility::Hidden
        }
        _ => PanelVisibility::Visible,
    }
}

pub(super) fn left(owner: Owner) -> PanelVisibility {
    pointer(PanelVisibility::Hidden, owner)
}

#[cfg(test)]
mod tests {
    use super::Owner::{Pointer, Settings, Stop, Typing};
    use super::PanelVisibility::{Hidden, Visible};
    use super::{left, pointer, pressed};

    #[test]
    fn settings_owns_status_visibility_until_it_is_dismissed() {
        assert_eq!(pointer(Visible, Settings), Hidden);
        assert_eq!(pressed(Hidden, Settings), Hidden);
        assert_eq!(left(Settings), Hidden);
        assert_eq!(pressed(Hidden, Pointer), Visible);
    }

    #[test]
    fn hover_never_opens_status_except_for_the_stop_fallback() {
        assert_eq!(pointer(Hidden, Pointer), Hidden);
        assert_eq!(pointer(Visible, Pointer), Visible);
        assert_eq!(pointer(Hidden, Stop), Visible);
        assert_eq!(pointer(Hidden, Typing), Hidden);
    }

    #[test]
    fn click_toggles_unless_typing_or_stop_owns_the_panel() {
        assert_eq!(pressed(Hidden, Pointer), Visible);
        assert_eq!(pressed(Visible, Pointer), Hidden);
        assert_eq!(pressed(Visible, Stop), Visible);
        assert_eq!(pressed(Hidden, Stop), Visible);
        assert_eq!(pressed(Hidden, Typing), Hidden);
    }

    #[test]
    fn leaving_both_surfaces_hides_status_but_preserves_stop() {
        assert_eq!(left(Pointer), Hidden);
        assert_eq!(left(Stop), Visible);
        assert_eq!(left(Typing), Hidden);
    }
}
