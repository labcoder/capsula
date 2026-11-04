//! The drag owner: one press-held drag of Kumo out of the peek, from its press to exactly one
//! terminal outcome. GPUI's mouse events, Esc and the native bridge's capture losses come in;
//! moves and the ending go out. Nothing here has effects: only a `Released` ending may, and
//! the caller validates it first. How it fits: docs/development/kumo.md, "The pull".
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "C2a wires the drag to the peek's Kumo")
)]

use gpui_kit::{MouseButton, Pixels, Point};

/// Which drag an outcome belongs to. Each press that starts a drag gets the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Generation(u64);

/// How a drag ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum End {
    /// The initiating button came up here. The only ending that may have effects.
    Released(Point<Pixels>),
    /// Esc.
    Cancelled,
    /// Capture went elsewhere, another button came up, a release was missed, or the window
    /// holding capture closed.
    Lost,
}

/// What reaches the owner while a drag may be live.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Event {
    /// GPUI's `MouseMoveEvent`, with the button it reports held.
    Move {
        at: Point<Pixels>,
        pressed: Option<MouseButton>,
    },
    /// GPUI's `MouseUpEvent`. GPUI has released capture for any button by then.
    Up {
        button: MouseButton,
        at: Point<Pixels>,
    },
    /// `Escape::CancelPull`.
    Escape,
    /// The native bridge's `CaptureLost`: emitted at `at` on the performance counter, with
    /// the button whose release caused it, or `None` for a loss from outside.
    CaptureLost {
        at: i64,
        released: Option<MouseButton>,
    },
    /// The window holding capture closed or deactivated.
    SourceClosed,
}

/// What an event did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Outcome {
    /// It belongs to no live drag, or to an ended one.
    Ignored,
    Moved(Generation, Point<Pixels>),
    Ended(Generation, End),
}

#[derive(Clone, Copy, Debug)]
struct Live {
    generation: Generation,
    button: MouseButton,
    /// The press's time on the performance counter.
    start: i64,
}

#[derive(Debug, Default)]
pub(crate) struct DragOwner {
    last: u64,
    live: Option<Live>,
}

impl DragOwner {
    /// Starts a drag on `button`'s press at `start`. While one is live, a press starts
    /// nothing: its button's release ends the live drag as lost.
    pub(crate) fn press(&mut self, button: MouseButton, start: i64) -> Option<Generation> {
        if self.live.is_some() {
            return None;
        }
        self.last += 1;
        let generation = Generation(self.last);
        self.live = Some(Live {
            generation,
            button,
            start,
        });
        Some(generation)
    }

    pub(crate) fn live(&self) -> Option<Generation> {
        self.live.map(|live| live.generation)
    }

    pub(crate) fn handle(&mut self, event: Event) -> Outcome {
        let Some(live) = self.live else {
            return Outcome::Ignored;
        };
        let end = match event {
            Event::Move {
                at,
                pressed: Some(_),
            } => return Outcome::Moved(live.generation, at),
            Event::Up { button, at } if button == live.button => End::Released(at),
            // A loss emitted before this drag began is an earlier drag's. The initiating
            // button's own release is voluntary: its `Up` is the ending.
            Event::CaptureLost { at, released }
                if at <= live.start || released == Some(live.button) =>
            {
                return Outcome::Ignored;
            }
            Event::Escape => End::Cancelled,
            Event::Move { pressed: None, .. }
            | Event::Up { .. }
            | Event::CaptureLost { .. }
            | Event::SourceClosed => End::Lost,
        };
        self.live = None;
        Outcome::Ended(live.generation, end)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{MouseButton, Pixels, Point, point, px};

    use super::{DragOwner, End, Event, Generation, Outcome};

    const LEFT: MouseButton = MouseButton::Left;
    const RIGHT: MouseButton = MouseButton::Right;

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    fn up(button: MouseButton, x: f32) -> Event {
        Event::Up {
            button,
            at: at(x, 0.0),
        }
    }

    fn lost(at: i64, released: Option<MouseButton>) -> Event {
        Event::CaptureLost { at, released }
    }

    fn dragging(start: i64) -> (DragOwner, Generation) {
        let mut owner = DragOwner::default();
        let generation = owner.press(LEFT, start).expect("no drag yet");
        (owner, generation)
    }

    #[test]
    fn a_release_outside_the_peek_ends_the_drag_there_once() {
        let (mut owner, g) = dragging(10);
        let far = at(2500.0, 1400.0);
        assert_eq!(
            owner.handle(Event::Move {
                at: far,
                pressed: Some(LEFT)
            }),
            Outcome::Moved(g, far)
        );
        assert_eq!(
            owner.handle(Event::Up {
                button: LEFT,
                at: far
            }),
            Outcome::Ended(g, End::Released(far))
        );
        assert_eq!(owner.live(), None);
        assert_eq!(owner.handle(up(LEFT, 0.0)), Outcome::Ignored);
    }

    #[test]
    fn esc_cancels_and_a_delayed_release_or_second_esc_is_ignored() {
        let (mut owner, g) = dragging(10);
        assert_eq!(
            owner.handle(Event::Escape),
            Outcome::Ended(g, End::Cancelled)
        );
        assert_eq!(owner.handle(up(LEFT, 5.0)), Outcome::Ignored);
        assert_eq!(owner.handle(Event::Escape), Outcome::Ignored);
    }

    #[test]
    fn a_second_button_starts_nothing_and_its_release_loses_the_drag() {
        let (mut owner, g) = dragging(10);
        assert_eq!(owner.press(RIGHT, 20), None);
        assert_eq!(owner.handle(up(RIGHT, 5.0)), Outcome::Ended(g, End::Lost));
        assert_eq!(owner.handle(up(LEFT, 5.0)), Outcome::Ignored);
        assert_eq!(owner.handle(lost(30, Some(RIGHT))), Outcome::Ignored);
    }

    #[test]
    fn the_voluntary_capture_change_leaves_the_release_to_end_it() {
        let (mut owner, g) = dragging(10);
        assert_eq!(owner.handle(lost(20, Some(LEFT))), Outcome::Ignored);
        assert_eq!(owner.live(), Some(g));
        assert_eq!(
            owner.handle(up(LEFT, 7.0)),
            Outcome::Ended(g, End::Released(at(7.0, 0.0)))
        );
    }

    #[test]
    fn capture_stolen_from_outside_or_by_another_button_is_lost() {
        let (mut owner, g) = dragging(10);
        assert_eq!(owner.handle(lost(11, None)), Outcome::Ended(g, End::Lost));
        let mut owner = DragOwner::default();
        let g = owner.press(LEFT, 10).expect("no drag yet");
        assert_eq!(
            owner.handle(lost(11, Some(RIGHT))),
            Outcome::Ended(g, End::Lost)
        );
    }

    #[test]
    fn a_loss_matches_only_a_drag_that_began_before_it() {
        let (mut owner, first) = dragging(10);
        assert_eq!(
            owner.handle(up(LEFT, 0.0)),
            Outcome::Ended(first, End::Released(at(0.0, 0.0)))
        );
        let second = owner.press(LEFT, 50).expect("the first ended");
        assert_ne!(first, second);
        // Emitted during the first drag, drained during the second.
        assert_eq!(owner.handle(lost(30, None)), Outcome::Ignored);
        assert_eq!(
            owner.handle(lost(50, None)),
            Outcome::Ignored,
            "at its start"
        );
        assert_eq!(owner.live(), Some(second));
        assert_eq!(
            owner.handle(lost(51, None)),
            Outcome::Ended(second, End::Lost)
        );
    }

    #[test]
    fn a_move_with_no_button_held_is_a_missed_release() {
        let (mut owner, g) = dragging(10);
        assert_eq!(
            owner.handle(Event::Move {
                at: at(1.0, 1.0),
                pressed: None
            }),
            Outcome::Ended(g, End::Lost)
        );
    }

    #[test]
    fn closing_the_source_window_loses_the_drag() {
        let (mut owner, g) = dragging(10);
        assert_eq!(
            owner.handle(Event::SourceClosed),
            Outcome::Ended(g, End::Lost)
        );
        assert_eq!(owner.handle(Event::SourceClosed), Outcome::Ignored);
    }

    #[test]
    fn with_no_drag_every_event_is_ignored() {
        let mut owner = DragOwner::default();
        for event in [
            Event::Move {
                at: at(1.0, 1.0),
                pressed: Some(LEFT),
            },
            up(LEFT, 0.0),
            Event::Escape,
            lost(5, None),
            Event::SourceClosed,
        ] {
            assert_eq!(owner.handle(event), Outcome::Ignored, "{event:?}");
        }
        assert_eq!(owner.press(LEFT, 1), Some(Generation(1)));
    }
}
