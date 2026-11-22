//! The UI thread's input sequence, and the scopes in which the native bridge forwards a button
//! to GPUI. Plain state, no operating-system calls: Windows' window procedure calls in today,
//! and macOS's event handling will make the same calls. How it fits: docs/development/kumo.md,
//! "The native bridge".

use std::cell::Cell;

/// A mouse button, as the native bridge reads it and the drag owner compares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// One per UI thread. Every value it stamps is new, so two stamps never tie.
#[derive(Debug, Default)]
pub struct Input {
    last: Cell<u64>,
    /// The stamp of the press being forwarded, if any.
    press: Cell<Option<u64>>,
    /// The button whose release is being forwarded, if any.
    released: Cell<Option<Button>>,
    /// How many release forwards are still running.
    releasing: Cell<u32>,
    /// A drained capture loss waits for the outermost release forward to return.
    reconcile: Cell<bool>,
}

thread_local! {
    static INPUT: Input = Input::default();
}

/// This thread's [`Input`].
pub fn input<R>(f: impl FnOnce(&Input) -> R) -> R {
    INPUT.with(f)
}

impl Input {
    fn next(&self) -> u64 {
        let seq = self.last.get() + 1;
        self.last.set(seq);
        seq
    }

    /// Forwards a button press inside `forward`, with a new stamp that [`Input::pressed`] reads
    /// meanwhile. A press forwarded inside it gets its own stamp and gives this one back.
    pub fn press<R>(&self, forward: impl FnOnce() -> R) -> R {
        let outer = self.press.replace(Some(self.next()));
        let result = forward();
        self.press.set(outer);
        result
    }

    /// The stamp of the press being forwarded.
    pub fn pressed(&self) -> Option<u64> {
        self.press.get()
    }

    /// Forwards `button`'s release inside `forward`. Returns its result, and whether a capture
    /// loss drained meanwhile is now due its reconcile: only the outermost release returns
    /// `true`, once.
    pub fn release<R>(&self, button: Button, forward: impl FnOnce() -> R) -> (R, bool) {
        let outer = self.released.replace(Some(button));
        let depth = self.releasing.get();
        self.releasing.set(depth + 1);
        let result = forward();
        self.releasing.set(depth);
        self.released.set(outer);
        (result, depth == 0 && self.reconcile.replace(false))
    }

    /// Capture changed now: its stamp, and the button whose forwarded release caused it.
    pub fn capture_changed(&self) -> (u64, Option<Button>) {
        (self.next(), self.released.get())
    }

    /// A cancel mode now: its stamp. Never a release's, whatever is being forwarded.
    pub fn cancelled(&self) -> u64 {
        self.next()
    }

    /// A capture loss was drained. Returns whether its reconcile may be scheduled now; otherwise
    /// a release is still being forwarded, and the outermost one reports it due as it returns.
    pub fn reconcile_after_release(&self) -> bool {
        if self.releasing.get() == 0 {
            return true;
        }
        self.reconcile.set(true);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{Button, Input};

    const LEFT: Button = Button::Left;

    #[test]
    fn stamps_never_repeat_across_presses_and_losses() {
        let input = Input::default();
        let first = input.press(|| input.pressed()).expect("forwarding a press");
        let (loss, _) = input.capture_changed();
        let cancel = input.cancelled();
        let second = input.press(|| input.pressed()).expect("forwarding a press");
        assert_eq!([first, loss, cancel, second], [1, 2, 3, 4]);
        assert_eq!(input.pressed(), None, "no press is being forwarded");
    }

    #[test]
    fn a_nested_press_keeps_the_outer_stamp() {
        let input = Input::default();
        input.press(|| {
            let outer = input.pressed().expect("outer press stamped");
            input.press(|| {
                let inner = input.pressed().expect("inner press stamped");
                assert!(inner > outer);
            });
            assert_eq!(input.pressed(), Some(outer));
        });
        assert_eq!(input.pressed(), None);
    }

    #[test]
    fn only_a_capture_change_during_a_release_names_the_button() {
        let input = Input::default();
        assert_eq!(input.capture_changed().1, None);
        let ((during, cancel), _) = input.release(LEFT, || {
            let during = input.capture_changed().1;
            let cancel = input.cancelled();
            (during, cancel)
        });
        assert_eq!(during, Some(LEFT));
        assert_eq!(cancel, 3, "a cancel mode gets its own stamp, and no button");
        assert_eq!(
            input.capture_changed().1,
            None,
            "restored after the forward"
        );
    }

    #[test]
    fn a_loss_drained_with_no_release_running_reconciles_now() {
        let input = Input::default();
        assert!(input.reconcile_after_release());
        assert!(!input.release(LEFT, || {}).1, "nothing was held for it");
    }

    #[test]
    fn a_loss_drained_inside_nested_releases_is_due_once_when_the_outermost_returns() {
        let input = Input::default();
        let (inner_due, outer_due) = input.release(LEFT, || {
            assert!(
                !input.reconcile_after_release(),
                "not before the release returns"
            );
            input
                .release(Button::Right, || {
                    assert_eq!(input.capture_changed().1, Some(Button::Right));
                })
                .1
        });
        assert!(!inner_due, "an inner release doesn't schedule it");
        assert!(outer_due);
        assert!(!input.release(LEFT, || {}).1, "scheduled exactly once");
    }
}
