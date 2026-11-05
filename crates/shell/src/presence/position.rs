use gpui_kit::Context;
use kumochi_protocol::message::{ControlToBrain, DockPosition};

use super::{
    LineFrame, OpenError, PEEK_HEIGHT, PanelVisibility, Placement, Position, Presence, line,
    platform, scaled,
};

impl Presence {
    pub(super) fn pick_dock_position(
        &mut self,
        position: DockPosition,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let (link, session) = (&self.link, &self.session);
        self.settings.update(cx, |state, cx| {
            state.send_dock_position(position, |op| {
                session
                    .as_ref()
                    .is_some_and(|session| link.send_control(session, ControlToBrain::Settings(op)))
            });
            cx.notify();
        });
        self.position(position.into(), cx)
    }

    pub(super) fn restore_dock_position(
        &mut self,
        position: Option<DockPosition>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(position) = position else {
            return Ok(());
        };
        let index = DockPosition::ALL
            .iter()
            .position(|value| *value == position)
            .expect("every position has a tray entry");
        if self.line.position_index() != Some(index) {
            self.place_position(position.into(), cx)?;
        }
        Ok(())
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

    fn place_position(
        &mut self,
        position: Position,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        // shortcut: work-area changes need another position choice; add a monitor watcher later.
        let area = platform::work_area()?;
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
}
