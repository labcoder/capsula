use std::fmt;

use kumochi_protocol::message::{
    AssistantName, ControlToBrain, NameError, SettingsOp, ThreadId, ThreadKind, ThreadOp,
    ThreadTitle,
};

use super::{Threads, row_label};
use crate::strings::Text;

pub enum Rename {
    Blocked,
    Close,
    Send(ControlToBrain),
}

impl fmt::Debug for Rename {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blocked => "Rename::Blocked",
            Self::Close => "Rename::Close",
            Self::Send(_) => "Rename::Send",
        })
    }
}

impl Threads {
    pub fn prepare_rename(&self, thread: ThreadId, text: &str) -> Result<Rename, Text> {
        if self.open != Some(thread) {
            return Ok(Rename::Close);
        }
        if !self.can_rename(thread) {
            return Ok(Rename::Blocked);
        }
        let Some(row) = self.row(thread) else {
            return Ok(Rename::Close);
        };
        let message = match row.kind {
            ThreadKind::Main => AssistantName::new(text)
                .map(|name| ControlToBrain::Settings(SettingsOp::SetAssistantName(name))),
            ThreadKind::Thread => ThreadTitle::new(text)
                .map(|title| ControlToBrain::ThreadOp(ThreadOp::Rename { thread, title })),
        }
        .map_err(|error| match error {
            NameError::Empty => Text::RenameEmpty,
            NameError::TooLong if row.kind == ThreadKind::Main => Text::RenameNameTooLong,
            NameError::TooLong => Text::RenameTooLong,
            NameError::NotOneLine => Text::RenameOneLine,
        })?;
        Ok(if text.trim() == row_label(row, self.settings.as_ref()) {
            Rename::Close
        } else {
            Rename::Send(message)
        })
    }
}

#[cfg(test)]
mod tests {
    use kumochi_protocol::{constants::DEFAULT_ASSISTANT_NAME, message::ThreadRow};

    use super::*;

    const MAIN: ThreadId = ThreadId(4);
    const TASK: ThreadId = ThreadId(5);

    fn editing(thread: ThreadId) -> Threads {
        let mut state = Threads::default();
        state.connected();
        state.rows = [
            (MAIN, ThreadKind::Main, ""),
            (TASK, ThreadKind::Thread, "Birthday"),
        ]
        .map(|(id, kind, title)| ThreadRow {
            id,
            title: title.into(),
            updated_ms: 0,
            kind,
            choice: None,
            task: None,
        })
        .to_vec();
        state.show_for(Some(thread));
        state
    }

    fn rename_message(thread: ThreadId, text: &str) -> Result<Option<ControlToBrain>, Text> {
        editing(thread).prepare_rename(thread, text).map(|rename| {
            if let Rename::Send(message) = rename {
                Some(message)
            } else {
                None
            }
        })
    }

    #[test]
    fn unchanged_names_and_titles_send_no_rename() {
        assert!(matches!(
            editing(MAIN).prepare_rename(MAIN, &format!(" {DEFAULT_ASSISTANT_NAME} ")),
            Ok(Rename::Close)
        ));
        assert!(matches!(
            editing(TASK).prepare_rename(TASK, " Birthday "),
            Ok(Rename::Close)
        ));
        assert!(matches!(
            editing(MAIN).prepare_rename(MAIN, "   "),
            Err(Text::RenameEmpty)
        ));
    }

    #[test]
    fn rename_uses_the_kind_and_validates_before_sending() {
        assert_eq!(
            rename_message(MAIN, " Cloud "),
            Ok(Some(ControlToBrain::Settings(
                SettingsOp::SetAssistantName(AssistantName::new("Cloud").expect("valid name"))
            )))
        );
        assert_eq!(
            rename_message(TASK, " Celebration "),
            Ok(Some(ControlToBrain::ThreadOp(ThreadOp::Rename {
                thread: TASK,
                title: ThreadTitle::new("Celebration").expect("valid title")
            })))
        );
        assert_eq!(rename_message(MAIN, ""), Err(Text::RenameEmpty));
        assert_eq!(
            rename_message(MAIN, &"a".repeat(25)),
            Err(Text::RenameNameTooLong)
        );
        assert_eq!(
            rename_message(TASK, &"a".repeat(81)),
            Err(Text::RenameTooLong)
        );
        assert_eq!(rename_message(TASK, "two\nlines"), Err(Text::RenameOneLine));
    }

    #[test]
    fn assistant_names_and_task_titles_have_different_character_limits() {
        let name = "🌥".repeat(24);
        assert_eq!(
            rename_message(MAIN, &name),
            Ok(Some(ControlToBrain::Settings(
                SettingsOp::SetAssistantName(AssistantName::new(&name).expect("24 characters"))
            )))
        );
        for title in ["🌥".repeat(80), "a".repeat(25)] {
            assert_eq!(
                rename_message(TASK, &title),
                Ok(Some(ControlToBrain::ThreadOp(ThreadOp::Rename {
                    thread: TASK,
                    title: ThreadTitle::new(&title).expect("valid task title")
                })))
            );
        }
        assert_eq!(rename_message(MAIN, "Cloud\u{7}"), Err(Text::RenameOneLine));
    }

    #[test]
    fn a_stale_editor_cannot_rename_a_thread_that_is_still_in_the_list() {
        let mut state = editing(TASK);
        state.select(Some(MAIN));
        assert!(matches!(
            state.prepare_rename(TASK, "Valid new title"),
            Ok(Rename::Close)
        ));
    }

    #[test]
    fn a_blocked_editor_stays_open_but_a_missing_row_closes_it() {
        let mut state = editing(TASK);
        state.disconnected();
        assert!(matches!(
            state.prepare_rename(TASK, "Valid new title"),
            Ok(Rename::Blocked)
        ));
        state.connected();
        state.deleting.push(TASK);
        assert!(matches!(
            state.prepare_rename(TASK, "Valid new title"),
            Ok(Rename::Blocked)
        ));
        state.deleting.clear();
        state.rows.retain(|row| row.id != TASK);
        assert!(matches!(
            state.prepare_rename(TASK, "Valid new title"),
            Ok(Rename::Close)
        ));
    }

    #[test]
    fn a_prepared_rename_does_not_debug_print_the_title() {
        let title = "private rename sentinel";
        let result = format!("{:?}", editing(TASK).prepare_rename(TASK, title));
        assert_eq!(result, "Ok(Rename::Send)");
        assert!(!result.contains(title));
        assert!(!result.contains(&format!("{:?}", title.as_bytes())));
    }
}
