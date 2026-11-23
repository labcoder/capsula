use super::{DockIntent, DockPosition, Pending, Settings, SettingsOp, Slot};

impl Settings {
    pub fn dock_position_choice(&self) -> Option<DockPosition> {
        self.offline_position
            .map(DockIntent::position)
            .or_else(|| {
                self.pending
                    .iter()
                    .rev()
                    .find_map(|pending| match (pending.slot, pending.failed) {
                        (Slot::Dock(position), false) => Some(position),
                        _ => None,
                    })
            })
            .or(self.view.as_ref().map(|view| view.dock_position))
    }

    pub fn send_dock_position(
        &mut self,
        position: DockPosition,
        send: impl FnOnce(SettingsOp) -> bool,
    ) -> bool {
        self.offline_position = Some(DockIntent::Unsent(position));
        self.flush_offline_position(send)
    }

    pub fn flush_offline_position(&mut self, send: impl FnOnce(SettingsOp) -> bool) -> bool {
        let (Some(intent), Some(view)) = (self.offline_position, self.view.as_ref()) else {
            return false;
        };
        let position = intent.position();
        if matches!(intent, DockIntent::Replay(_)) && view.dock_position == position {
            self.offline_position = None;
            return false;
        }
        if !send(SettingsOp::SetDockPosition(position)) {
            return false;
        }
        self.offline_position = None;
        self.pending.push_back(Pending {
            slot: Slot::Dock(position),
            failed: false,
            reset_draft: false,
        });
        true
    }
}
