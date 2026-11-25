//! Status visibility decisions, separate from native placement and pointer timing.

use gpui_kit::App;

use super::kumo_view::{KumoView, PullChanged, WantsFrames};
use super::{Activity, Context, LineFrame, OpenError, Presence, geometry::PanelVisibility};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    /// Kumo is being pulled out of the peek, which stays open wherever the pointer goes.
    Pull,
    Pointer,
    Typing,
    Settings,
    /// The first-run window keeps the peek closed until it's gone.
    Onboarding,
    Stop,
}

impl Presence {
    pub(super) fn panel_owner(&self, cx: &App) -> Owner {
        if self.kumo.read(cx).pull_live() {
            Owner::Pull
        } else if self.onboarding.is_some() {
            Owner::Onboarding
        } else if self.settings_open() {
            Owner::Settings
        } else if self.text_open() || self.threads_open() {
            Owner::Typing
        } else if self.escape_hold.shows_stop() || self.voice_working() {
            Owner::Stop
        } else {
            Owner::Pointer
        }
    }

    /// Follows the peek's Kumo: the frames it wants, and its pull.
    pub(super) fn follow_kumo(&mut self, cx: &mut Context<'_, Self>) {
        let kumo = self.kumo.clone();
        let owner = cx.weak_entity();
        kumo.update(cx, |view, _| view.owner = Some(owner));
        cx.subscribe(
            &kumo,
            |this, _, event: &super::kumo_view::LeaseReturned, _| {
                this.presenter.input.return_drag(event.0);
            },
        )
        .detach();
        cx.subscribe(&kumo, |this, _, _: &WantsFrames, cx| {
            this.want_kumo_frames(cx);
        })
        .detach();
        cx.subscribe(&kumo, |this, _, changed: &PullChanged, cx| {
            if let PullChanged::Ended(drag, end) = *changed {
                this.carry_ended(drag, end, cx);
            }
            if let Err(error) = this.pull_changed(cx) {
                tracing::error!(%error, "can't follow Kumo's pull");
            }
        })
        .detach();
    }

    /// Quit began: a live drag ends now and gives back Esc, before any shutdown work waits.
    pub(super) fn end_pull(&mut self, cx: &mut Context<'_, Self>) {
        self.kumo.update(cx, KumoView::close);
        self.end_carry(cx);
        self.activity.pulling = false;
        self.publish_plain_escape(cx);
        if let Err(error) = self.sync_escape(cx) {
            tracing::warn!(%error, "can't give back Esc at Quit");
        }
    }

    /// The drag began or ended: Esc is held only while it's live, and the peek's owner follows.
    fn pull_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        let activity = Activity {
            pulling: self.kumo.read(cx).pull_live(),
            ..self.activity
        };
        if activity.pulling {
            self.line.pull_started();
            self.want_pull_frames(cx);
        }
        self.publish_plain_escape(cx);
        if activity != self.activity {
            self.activity = activity;
            self.sync_escape(cx)?;
        }
        self.pointer_changed(cx)
    }
}

/// The peek's visibility as the pointer moves elsewhere: leaving is the grace timer's call.
pub(super) fn pointer(current: PanelVisibility, owner: Owner) -> PanelVisibility {
    match owner {
        Owner::Typing | Owner::Settings | Owner::Onboarding => PanelVisibility::Hidden,
        Owner::Stop | Owner::Pull => PanelVisibility::Visible,
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

pub(super) fn gap_contains(
    line: LineFrame,
    [panel_x, panel_y, panel_width, panel_height]: [i32; 4],
    [x, y]: [i32; 2],
) -> bool {
    let [_, line_y, _, line_height] = line.paint;
    (panel_x..panel_x + panel_width).contains(&x)
        && ((line_y + line_height..panel_y).contains(&y)
            || (panel_y + panel_height..line_y).contains(&y))
}

#[cfg(test)]
mod tests {
    use super::Owner::{Onboarding, Pointer, Pull, Settings, Stop, Typing};
    use super::PanelVisibility::{Hidden, Visible};
    use super::{LineFrame, gap_contains, left, over_line, pointer};

    #[test]
    fn the_hover_gap_spans_the_panel_width_above_or_below_the_painted_line() {
        let line = LineFrame {
            paint: [20, 30, 40, 10],
            hit: [20, 22, 40, 26],
        };
        let below = [0, 42, 80, 20];
        assert!(gap_contains(line, below, [0, 40]));
        assert!(gap_contains(line, below, [79, 41]));
        assert!(!gap_contains(line, below, [80, 41]));
        assert!(!gap_contains(line, below, [30, 39]));
        assert!(!gap_contains(line, below, [30, 42]));

        let above = [0, 8, 80, 20];
        assert!(gap_contains(line, above, [0, 28]));
        assert!(gap_contains(line, above, [79, 29]));
        assert!(!gap_contains(line, above, [30, 27]));
        assert!(!gap_contains(line, above, [30, 30]));
        assert!(!gap_contains(line, [0, 40, 80, 20], [30, 40]));
    }

    #[test]
    fn the_peek_covers_the_gap_to_the_side_of_the_native_hit_band() {
        use crate::presence::{PEEK_HEIGHT, Position, line, scaled};

        let layout = Position::TopLeft.layout(
            [0, 0, 1024, 768],
            line::slot(1.0),
            [380, scaled(PEEK_HEIGHT, 1.0)],
            0,
        );
        let frame = LineFrame::in_slot(layout.dock, 1.0);
        assert!(gap_contains(frame, layout.panel, [60, 16]));
        assert!(!gap_contains(frame, layout.panel, [380, 16]));
    }

    #[test]
    fn settings_typing_and_onboarding_keep_the_peek_closed_even_over_the_line() {
        for owner in [Settings, Typing, Onboarding] {
            assert_eq!(over_line(owner), Hidden);
            assert_eq!(pointer(Visible, owner), Hidden);
            assert_eq!(left(owner), Hidden);
        }
    }

    #[test]
    fn the_line_opens_the_peek_and_moving_elsewhere_leaves_it_as_it_is() {
        assert_eq!(over_line(Pointer), Visible);
        assert_eq!(pointer(Hidden, Pointer), Hidden);
        assert_eq!(pointer(Visible, Pointer), Visible);
        assert_eq!(left(Pointer), Hidden);
    }

    #[test]
    fn stop_keeps_the_peek_open_wherever_the_pointer_is() {
        assert_eq!(pointer(Hidden, Stop), Visible);
        assert_eq!(over_line(Stop), Visible);
        assert_eq!(left(Stop), Visible);
    }

    #[test]
    fn a_live_pull_keeps_the_peek_open_even_when_its_grace_has_run_out() {
        assert_eq!(over_line(Pull), Visible);
        assert_eq!(pointer(Hidden, Pull), Visible);
        assert_eq!(left(Pull), Visible);
    }
}
