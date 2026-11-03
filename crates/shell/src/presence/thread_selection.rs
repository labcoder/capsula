//! The keyboard highlight follows a changed view, not unrelated page notifications.

use kumochi_protocol::message::ThreadId;

use crate::threads::Threads;

#[derive(Default)]
pub(super) struct Selection {
    pub(super) selected: Option<ThreadId>,
    opened: Option<ThreadId>,
}

impl Selection {
    pub(super) fn select(&mut self, ix: Option<usize>, state: &Threads) {
        self.selected = ix.and_then(|ix| state.rows.get(ix)).map(|row| row.id);
    }

    pub(super) fn observe(&mut self, state: &Threads) -> Option<usize> {
        if self.opened != state.open {
            self.opened = state.open;
            self.selected = self.opened;
            return state.selected_index();
        }
        let ix = self
            .selected
            .and_then(|thread| state.rows.iter().position(|row| row.id == thread));
        self.selected = ix.map(|ix| state.rows[ix].id);
        ix
    }
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::message::{ThreadKind, ThreadRow};

    use super::*;

    fn state() -> Threads {
        let mut state = Threads::default();
        state.rows = [1, 2, 3]
            .map(|id| ThreadRow {
                id: ThreadId(id),
                title: String::new(),
                updated_ms: 0,
                kind: ThreadKind::Thread,
                choice: None,
            })
            .to_vec();
        state.open = Some(ThreadId(1));
        state
    }

    #[test]
    fn a_run_notification_keeps_the_arrowed_highlight_and_enter_target() {
        let mut state = state();
        let mut selection = Selection::default();
        assert_eq!(selection.observe(&state), Some(0));
        selection.select(Some(1), &state);
        assert_eq!(selection.selected, Some(ThreadId(2)));
        state.run_state(ThreadId(3));
        assert_eq!(state.open, Some(ThreadId(1)));
        assert_eq!(selection.observe(&state), Some(1));
        assert_eq!(selection.selected, Some(ThreadId(2)));
    }

    #[test]
    fn changed_views_and_hide_resync_the_highlight() {
        let mut state = state();
        let mut selection = Selection::default();
        assert_eq!(selection.observe(&state), Some(0));
        selection.select(Some(1), &state);
        state.select(Some(ThreadId(3)));
        assert_eq!(selection.observe(&state), Some(2));
        assert_eq!(selection.selected, Some(ThreadId(3)));
        state.hide();
        assert_eq!(selection.observe(&state), None);
        assert_eq!(selection.selected, None);
        state.show();
        assert_eq!(selection.observe(&state), None);
    }

    #[test]
    fn page_changes_preserve_the_highlight_by_id_or_clear_a_missing_row() {
        let mut state = state();
        let mut selection = Selection::default();
        assert_eq!(selection.observe(&state), Some(0));
        selection.select(Some(1), &state);
        state.rows.swap(0, 1);
        assert_eq!(selection.observe(&state), Some(0));
        assert_eq!(selection.selected, Some(ThreadId(2)));
        state.rows.retain(|row| row.id != ThreadId(2));
        assert_eq!(selection.observe(&state), None);
        assert_eq!(selection.selected, None);
    }
}
