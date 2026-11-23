//! Composer visibility, submission and notice policy, without a window or input widget.

use kumochi_protocol::message::Content;

use crate::link::{LinkHandle, Session};
use crate::runs::{Refused, Runs};
use crate::strings::Text;

/// Why a typing surface was asked to hide.
#[derive(Clone, Copy, Debug)]
pub enum Hide {
    /// An explicit close.
    Dismiss,
    /// The window reported losing activation.
    Deactivated,
}

impl Hide {
    /// Only an explicit dismissal can give focus back to the previous window.
    pub fn restores_foreground(self) -> bool {
        matches!(self, Self::Dismiss)
    }
}

/// The shared visibility and activation state of a panel.
#[derive(Clone, Copy, Debug, Default)]
pub struct Typing {
    /// Whether the surface is visible.
    pub visible: bool,
    active_seen: bool,
}

impl Typing {
    /// Shows the surface and reports whether it was hidden.
    pub fn open(&mut self) -> bool {
        let opening = !self.visible;
        if opening {
            self.active_seen = false;
        }
        self.visible = true;
        opening
    }

    /// Records that the visible window has been activated.
    pub fn activated(&mut self) {
        if self.visible {
            self.active_seen = true;
        }
    }

    /// Reports whether the caller should hide its window. Deactivation before the first
    /// activation of this opening isn't a dismissal.
    pub fn close(&mut self, reason: Hide) -> bool {
        if !self.visible || matches!(reason, Hide::Deactivated) && !self.active_seen {
            return false;
        }
        self.visible = false;
        self.active_seen = false;
        true
    }
}

/// What pressing Enter can do before the caller cancels voice or reads the draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The caller can cancel voice and attempt the submit.
    Proceed,
    /// No eligible session or notice.
    Ignore,
    /// The session cannot accept a draft now.
    Refused(Text),
}

/// Checks the current session without dismissing voice or touching the draft.
pub fn admit(link: &LinkHandle, session: Option<&Session>) -> Admission {
    if link.admits_text(session) {
        Admission::Proceed
    } else if session.is_some() {
        Admission::Refused(Text::TurnBusy)
    } else {
        Admission::Ignore
    }
}

/// The notice changes from a typed submission or retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submission {
    /// The draft was queued; prior notices can be cleared.
    Sent,
    /// No draft was sent; prior notices stay.
    Blank,
    /// Show the local refusal without replacing the shared error.
    Refused(Text),
}

impl From<Result<(), Refused>> for Submission {
    fn from(result: Result<(), Refused>) -> Self {
        match result {
            Ok(()) => Self::Sent,
            Err(Refused::Blank) => Self::Blank,
            Err(Refused::Busy) => Self::Refused(Text::TurnBusy),
            Err(Refused::Offline) => Self::Refused(Text::TurnOffline),
            Err(Refused::TooLong) => Self::Refused(Text::TurnTooLong),
        }
    }
}

impl Submission {
    /// The retained error can also be shown in other views, so a local refusal never replaces it.
    pub fn update_notice<E>(self, error: &mut Option<E>, refusal: &mut Option<Text>) {
        match self {
            Self::Sent => {
                *error = None;
                *refusal = None;
            }
            Self::Blank => {}
            Self::Refused(text) => *refusal = Some(text),
        }
    }
}

/// Submits through the same pair used for admission. Only `Sent` lets the caller clear the draft.
pub fn submit(
    runs: &mut Runs,
    link: &LinkHandle,
    session: Option<&Session>,
    draft: &str,
) -> Submission {
    runs.submit_with_audio(draft, link.audio_session(), |turn| {
        session.is_some_and(|session| link.send_data(session, turn))
    })
    .into()
}

/// The one notice the composer may show.
pub enum Notice<R> {
    /// A local submission refusal, without the retained error's controls.
    Refusal(Text),
    /// A retained error for this composer, including its recovery controls.
    Recovery(R),
    /// No refusal or retained error; the shell can show its ordinary status.
    Fallback,
}

/// A local refusal takes precedence over the retained error for this surface only.
pub fn notice<R>(refusal: Option<Text>, recovery: Option<R>) -> Notice<R> {
    if let Some(refusal) = refusal {
        Notice::Refusal(refusal)
    } else {
        recovery.map_or(Notice::Fallback, Notice::Recovery)
    }
}

/// Recovery may fill an empty draft, never replace one, even one containing only spaces.
pub fn restore(draft: &str, unsent: Content) -> Option<Content> {
    draft.is_empty().then_some(unsent)
}

#[cfg(test)]
mod tests;
