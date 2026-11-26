use gpui_kit::{App, Context};
use kumochi_protocol::message::DockPosition;

use super::{
    Layout, LineFrame, OpenError, PEEK_HEIGHT, PanelVisibility, Placement, Position, Presence,
    line, observer, platform, scaled,
};
use crate::settings::Save;

impl Presence {
    pub(super) fn pick_dock_position(
        &mut self,
        position: DockPosition,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.request_save(Save::DockPosition(position), cx);
        self.position(position.into(), cx)
    }

    pub(super) fn save_dock_position(
        &mut self,
        position: DockPosition,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.request_save(Save::DockPosition(position), cx);
        self.place_position(position.into(), cx)
    }

    pub(super) fn restore_dock_position(
        &mut self,
        position: Option<DockPosition>,
        cx: &mut Context<'_, Self>,
    ) {
        let mode = if observer::active(cx) {
            RestoreMode::ObserverSeed
        } else {
            RestoreMode::Saved
        };
        if let Some(position) = restored_position(position, self.line.position_index(), mode)
            && let Err(error) = self.place_position(position, cx)
        {
            tracing::warn!(%error, "can't restore the saved dock position");
        }
    }

    pub(super) fn position(
        &mut self,
        position: Position,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.place_position(position, cx)?;
        self.dismiss_settings(cx)?;
        self.dismiss_text(cx)?;
        self.dismiss_threads(cx)?;
        self.set_panel(PanelVisibility::Hidden, cx)?;
        if let Err(error) = self.pointer_changed(cx) {
            tracing::debug!(%error, "can't recheck the pointer after moving the dock");
        }
        Ok(())
    }

    pub(super) fn place_position(
        &mut self,
        position: Position,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let area = platform::work_area()?;
        // Moving the anchor invalidates a live selection's screen-space input geometry.
        self.end_selection(
            crate::presenter::overlay_input::ExitCause::DisplayChanged,
            cx,
        );
        let layout = position.layout(
            area,
            line::slot(self.scale),
            [scaled(380.0, self.scale), scaled(PEEK_HEIGHT, self.scale)],
            0,
        );
        let index = Position::ALL
            .iter()
            .position(|candidate| *candidate == position)
            .expect("every dock position has a menu entry");
        self.request_line(
            Placement {
                layout,
                frame: LineFrame::in_slot(layout.dock, self.scale),
                index,
            },
            cx,
        )
    }

    pub(super) fn reanchor_panels(&mut self, cx: &mut App) -> Result<(), OpenError> {
        let settings_open = self.settings_open();
        let threads_open = self.threads_open();
        let text_open = self.text_open();
        let status_open = self.panel_visibility == PanelVisibility::Visible;
        if !settings_open
            && !threads_open
            && !text_open
            && !status_open
            && self.onboarding.is_none()
        {
            return Ok(());
        }
        let area = platform::work_area()?;
        let mut status_bounds = self.layout.panel;
        let panels = [
            self.onboarding
                .as_mut()
                .map(|panel| (true, panel.window, &mut panel.bounds)),
            self.settings_panel
                .as_mut()
                .map(|panel| (settings_open, panel.window, &mut panel.bounds)),
            self.thread_panel
                .as_mut()
                .map(|panel| (threads_open, panel.window, &mut panel.bounds)),
            self.text
                .as_mut()
                .map(|panel| (text_open, panel.window, &mut panel.bounds)),
            Some((status_open, self.panel_window, &mut status_bounds)),
        ];
        for (visible, handle, bounds) in panels.into_iter().flatten() {
            let Some(next) = panel_bounds(visible.then_some(*bounds), self.layout, area) else {
                continue;
            };
            handle
                .update(cx, |_, window, cx| {
                    platform::place_presence_window(window, next)?;
                    window.bounds_changed(cx);
                    Ok::<_, platform::PresenceError>(())
                })
                .map_err(|error| OpenError::Gpui(error.into()))??;
            *bounds = next;
        }
        self.presenter.input.configure(&mut self.input_windows)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum RestoreMode {
    Saved,
    ObserverSeed,
}

fn restored_position(
    position: Option<DockPosition>,
    current: Option<usize>,
    mode: RestoreMode,
) -> Option<Position> {
    if matches!(mode, RestoreMode::ObserverSeed) {
        return None;
    }
    let position = position?;
    let index = DockPosition::ALL
        .iter()
        .position(|value| *value == position)
        .expect("every position has a tray entry");
    (current != Some(index)).then(|| position.into())
}

fn panel_bounds(bounds: Option<[i32; 4]>, layout: Layout, area: [i32; 4]) -> Option<[i32; 4]> {
    bounds.map(|bounds| layout.anchored_panel(area, [bounds[2], bounds[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_preserves_the_launch_seed_while_normal_snapshots_restore() {
        assert_eq!(
            restored_position(
                Some(DockPosition::BottomRight),
                Some(3),
                RestoreMode::ObserverSeed
            ),
            None
        );
        assert_eq!(
            restored_position(
                Some(DockPosition::TopCenter),
                Some(3),
                RestoreMode::ObserverSeed
            ),
            None
        );
        assert_eq!(
            restored_position(Some(DockPosition::BottomRight), Some(3), RestoreMode::Saved),
            Some(Position::BottomRight)
        );
        assert_eq!(
            restored_position(Some(DockPosition::BottomRight), Some(8), RestoreMode::Saved),
            None
        );
        assert_eq!(restored_position(None, Some(3), RestoreMode::Saved), None);
    }

    #[test]
    fn open_surfaces_follow_a_landed_anchor_without_resizing_or_showing_closed_ones() {
        let area = [0, 0, 1000, 800];
        let layout = Position::BottomRight.layout(area, [100, 20], [300, 120], 0);
        for (old, expected) in [
            ([0, 20, 600, 400], [400, 380, 600, 400]),
            ([0, 20, 768, 544], [232, 236, 768, 544]),
            ([0, 20, 380, 140], [620, 640, 380, 140]),
            ([0, 20, 300, 120], [700, 660, 300, 120]),
        ] {
            assert_eq!(panel_bounds(Some(old), layout, area), Some(expected));
            assert_eq!(panel_bounds(None, layout, area), None);
        }
    }
}
