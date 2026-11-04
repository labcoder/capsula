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

/// The peek's visibility as the pointer moves elsewhere: leaving is the grace timer's call.
pub(super) fn pointer(current: PanelVisibility, owner: Owner) -> PanelVisibility {
    match owner {
        Owner::Typing | Owner::Settings => PanelVisibility::Hidden,
        Owner::Stop => PanelVisibility::Visible,
        Owner::Pointer => current,
    }
}

/// The pointer is on the line: that opens the peek, unless typing or Settings owns it.
pub(super) fn over_line(owner: Owner) -> PanelVisibility {
    pointer(PanelVisibility::Visible, owner)
}

pub(super) fn left(owner: Owner) -> PanelVisibility {
    pointer(PanelVisibility::Hidden, owner)
}

#[cfg(test)]
mod tests {
    use super::Owner::{Pointer, Settings, Stop, Typing};
    use super::PanelVisibility::{Hidden, Visible};
    use super::{left, over_line, pointer};

    #[test]
    fn settings_and_typing_keep_the_peek_closed_even_over_the_line() {
        assert_eq!(over_line(Settings), Hidden);
        assert_eq!(over_line(Typing), Hidden);
        assert_eq!(pointer(Visible, Settings), Hidden);
        assert_eq!(left(Settings), Hidden);
    }

    #[test]
    fn the_line_opens_the_peek_and_moving_elsewhere_leaves_it_as_it_is() {
        assert_eq!(over_line(Pointer), Visible);
        assert_eq!(pointer(Hidden, Pointer), Hidden);
        assert_eq!(pointer(Visible, Pointer), Visible);
    }

    #[test]
    fn stop_keeps_the_peek_open_wherever_the_pointer_is() {
        assert_eq!(pointer(Hidden, Stop), Visible);
        assert_eq!(over_line(Stop), Visible);
        assert_eq!(left(Stop), Visible);
    }

    #[test]
    fn leaving_both_surfaces_hides_the_peek_but_preserves_stop() {
        assert_eq!(left(Pointer), Hidden);
        assert_eq!(left(Stop), Visible);
        assert_eq!(left(Typing), Hidden);
    }
}
