//! The screen watch's Windows sources: what may have changed the screen since a hold's
//! release. Each sets the watch's one bit and wakes the UI for it: at most once from this
//! thread's half while the watch listens, and at most once from the mouse thread for each
//! arm. Which key it was, where the pointer was and which window came forward stay in this
//! module.
//!
//! Keys, the window in front, the session and the display are heard on the UI thread: by the
//! proxy's keyboard and message arms, which call in here, and by one foreground hook. The mouse
//! is heard on a thread of its own (`sources.rs`), so a mouse that reports thousands of
//! times a second puts nothing on the UI's queue. Only the bit and a wake with nothing in it
//! cross between the two.

use std::cell::RefCell;

use async_channel::Sender;
use kumochi_shell_core::platform::screen_watch::Changed;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::presence::{PresenceEvent, process_of, send};
use crate::input::{Edge, Event, PhysicalKey, TalkHeld};

mod sources;

pub(crate) use sources::ScreenSources;

thread_local! {
    /// The UI thread's half of the running watch. The proxy's arms and the foreground hook
    /// have no context of their own.
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

#[derive(Default)]
struct Ui {
    /// The running watch's bit and where its wake goes; none while stopped.
    armed: Option<(Changed, Sender<PresenceEvent>)>,
    /// The talk keys still down from the hold that was released.
    held: TalkHeld,
}

impl Ui {
    /// Something may have changed the screen. Setting the bit queues a wake, which happens
    /// once while a watch listens: the presenter's read of a set bit stops the watch.
    fn change(&self) {
        if let Some((changed, events)) = &self.armed
            && changed.set()
        {
            send(events, PresenceEvent::ScreenWoke);
        }
    }

    /// One keyboard or lifecycle event, as the adapter reduced it: a talk key or another key,
    /// down or up, or a reset. `foreign` says whether the window in front is another
    /// process's. A key going down there counts, unless it is a repeat of a talk key the
    /// released hold still had down.
    fn key(&mut self, event: Event, modifier: Option<PhysicalKey>, foreign: impl FnOnce() -> bool) {
        match event {
            // Before anything is asked about the window in front, so a talk key that comes up
            // over Kumochi leaves no stale bit.
            Event::Key { edge: Edge::Up, .. } => {
                if let Some(id) = modifier {
                    self.held.release(id);
                }
            }
            Event::Key { edge: Edge::Down, .. } => {
                let repeat = modifier.is_some_and(|id| self.held.contains(id));
                if !repeat && self.armed.is_some() && foreign() {
                    self.change();
                }
            }
            // Keys were lost, or the session, the desktop or the power state changed.
            Event::Reset => {
                self.held = TalkHeld::default();
                self.change();
            }
            _ => {}
        }
    }
}

/// A keyboard or lifecycle event the proxy heard, already reduced by the keyboard adapter.
pub(super) fn heard(event: Event, modifier: Option<PhysicalKey>) {
    UI.with_borrow_mut(|ui| {
        ui.key(event, modifier, || {
            // SAFETY: a plain query with no arguments.
            let front = unsafe { GetForegroundWindow() };
            foreign(front)
        });
    });
}

/// `heard`, with another process's window in front.
#[cfg(test)]
fn heard_for_test(event: Event) {
    UI.with_borrow_mut(|ui| ui.key(event, fixture_modifier(event), || true));
}

#[cfg(test)]
fn fixture_modifier(event: Event) -> Option<PhysicalKey> {
    match event {
        Event::Key { key: crate::input::Key::LeftControl, .. } => PhysicalKey::new(0x1D),
        Event::Key { key: crate::input::Key::LeftAlt, .. } => PhysicalKey::new(0x38),
        _ => None,
    }
}

/// The display or its scaling changed, or an input record couldn't be read: an answer a
/// source can't get counts as a change.
pub(super) fn changed() {
    UI.with_borrow(Ui::change);
}

/// Whether `window` is another process's. One that can't be named counts as another's.
fn foreign(window: HWND) -> bool {
    // SAFETY: a plain query with no arguments.
    window.is_invalid() || process_of(window) != unsafe { GetCurrentProcessId() }
}

/// Arms the UI thread's half for a new watch, with the talk keys its hold still had down.
fn arm(changed: Changed, events: Sender<PresenceEvent>, held: TalkHeld) {
    UI.with_borrow_mut(|ui| {
        ui.armed = Some((changed, events));
        ui.held = held;
    });
}

/// Makes the key, foreground and message arms inert at once.
fn disarm() {
    UI.with_borrow_mut(|ui| ui.armed = None);
}

#[cfg(test)]
mod tests {
    use async_channel::Receiver;
    use kumochi_shell_core::platform::screen_watch::Changed;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{WM_INPUT, WM_WTSSESSION_CHANGE};

    use super::{PresenceEvent, UI, arm, changed, disarm, fixture_modifier, heard_for_test};
    use crate::input::{Edge, Event, Key, PhysicalKey, TalkHeld};

    const OTHER: Key = Key::Other(crate::input::PhysicalKey::new(0x1E).unwrap());

    fn watch(held: TalkHeld) -> (Changed, Receiver<PresenceEvent>) {
        let (events, wakes) = async_channel::unbounded();
        let bit = Changed::default();
        arm(bit.clone(), events, held);
        (bit, wakes)
    }

    /// Hears one key with another app's window in front.
    fn key(key: Key, edge: Edge) {
        heard_for_test(Event::Key { key, edge });
    }

    /// The wakes queued so far, and whether the bit is set. The bit is asked by setting it,
    /// so each bit is asked once.
    fn seen(bit: &Changed, wakes: &Receiver<PresenceEvent>) -> (usize, bool) {
        let mut count = 0;
        while let Ok(event) = wakes.try_recv() {
            assert_eq!(event, PresenceEvent::ScreenWoke);
            count += 1;
        }
        (count, !bit.set())
    }

    #[test]
    fn a_held_talk_key_s_repeats_are_the_hold_s_until_it_comes_up() {
        for (held, key_held, key_free) in [
            (
                TalkHeld {
                    control: crate::input::PhysicalKey::new(0x1D),
                    alt: None,
                },
                Key::LeftControl,
                Key::LeftAlt,
            ),
            (
                TalkHeld {
                    control: None,
                    alt: crate::input::PhysicalKey::new(0x38),
                },
                Key::LeftAlt,
                Key::LeftControl,
            ),
        ] {
            let (bit, wakes) = watch(held);
            key(key_held, Edge::Down);
            key(key_held, Edge::Down);
            assert_eq!(wakes.len(), 0, "repeats of the hold's own key");
            key(key_held, Edge::Up);
            key(key_held, Edge::Down);
            assert_eq!(seen(&bit, &wakes), (1, true), "a fresh press of it counts");

            // The talk key that was already up counts from the start.
            let (bit, wakes) = watch(held);
            key(key_free, Edge::Down);
            assert_eq!(seen(&bit, &wakes), (1, true));
            disarm();
        }
    }

    #[test]
    fn a_talk_key_that_comes_up_over_kumochi_leaves_no_stale_bit() {
        let (bit, wakes) = watch(TalkHeld {
            control: crate::input::PhysicalKey::new(0x1D),
            alt: crate::input::PhysicalKey::new(0x38),
        });
        // Kumochi is in front for both key-ups, which is never asked.
        UI.with_borrow_mut(|ui| {
            for key in [Key::LeftAlt, Key::LeftControl] {
                let event = Event::Key { key, edge: Edge::Up };
                ui.key(event, fixture_modifier(event), || panic!("a key-up doesn't ask what is in front"));
            }
        });
        assert_eq!(wakes.len(), 0);
        key(Key::LeftControl, Edge::Down);
        assert_eq!(seen(&bit, &wakes), (1, true));
        disarm();
    }

    #[test]
    fn a_key_counts_only_in_another_process_s_window_and_wakes_once_a_watch() {
        let (bit, wakes) = watch(TalkHeld::default());
        // Kumochi's own window is in front for this one.
        let typed = Event::Key {
            key: OTHER,
            edge: Edge::Down,
        };
        UI.with_borrow_mut(|ui| ui.key(typed, None, || false));
        key(OTHER, Edge::Up);
        assert_eq!(wakes.len(), 0, "typed into Kumochi, and a key-up elsewhere");
        key(OTHER, Edge::Down);
        key(OTHER, Edge::Down);
        changed();
        assert_eq!(seen(&bit, &wakes), (1, true), "one wake for a watch");
        disarm();
    }

    #[test]
    fn a_reset_and_a_display_change_count_whatever_is_in_front() {
        let (bit, wakes) = watch(TalkHeld {
            control: crate::input::PhysicalKey::new(0x1D),
            alt: None,
        });
        UI.with_borrow_mut(|ui| ui.key(Event::Reset, None, || panic!("a reset asks nothing")));
        assert_eq!(seen(&bit, &wakes), (1, true));

        // The reset also gave the held key back: its next press is a fresh one.
        let (events, wakes) = async_channel::unbounded();
        let bit = Changed::default();
        UI.with_borrow_mut(|ui| ui.armed = Some((bit.clone(), events)));
        key(Key::LeftControl, Edge::Down);
        assert_eq!(seen(&bit, &wakes), (1, true));

        let (bit, wakes) = watch(TalkHeld::default());
        changed();
        assert_eq!(seen(&bit, &wakes), (1, true));
        UI.with_borrow_mut(|ui| ui.key(Event::Deadline, None, || panic!("not a key")));
        UI.with_borrow_mut(|ui| ui.key(Event::Escape, None, || panic!("not a key")));
        assert_eq!(wakes.len(), 0);
        disarm();
    }

    #[test]
    fn the_proxy_s_keyboard_arm_reaches_the_watch() {
        let keys = super::super::keyboard::Keys::default();
        let proxy = |message| keys.on_message(HWND::default(), message, WPARAM(0), LPARAM(0));
        // A session change as the proxy receives it: a reset, which counts whatever is in front.
        let (bit, wakes) = watch(TalkHeld::default());
        assert!(!proxy(WM_WTSSESSION_CHANGE));
        assert_eq!(seen(&bit, &wakes), (1, true));
        // A keyboard record that can't be read may have been typed anywhere.
        let (bit, wakes) = watch(TalkHeld::default());
        assert!(!proxy(WM_INPUT));
        assert_eq!(seen(&bit, &wakes), (1, true));
        disarm();
    }

    #[test]
    fn a_stopped_watch_hears_nothing_and_the_next_takes_its_own_keys_and_bit() {
        let (first, first_wakes) = watch(TalkHeld {
            control: crate::input::PhysicalKey::new(0x1D),
            alt: None,
        });
        disarm();
        UI.with_borrow_mut(|ui| {
            ui.key(
                Event::Key {
                    key: OTHER,
                    edge: Edge::Down,
                },
                None,
                || panic!("a stopped watch doesn't ask what is in front"),
            );
        });
        changed();
        UI.with_borrow_mut(|ui| ui.key(Event::Reset, None, || true));
        assert_eq!(first_wakes.len(), 0);

        // The new release had no key down, so the old watch's held key is a fresh press now.
        let (second, second_wakes) = watch(TalkHeld::default());
        key(Key::LeftControl, Edge::Down);
        assert_eq!(seen(&second, &second_wakes), (1, true));
        assert_eq!(seen(&first, &first_wakes), (0, false));
        disarm();
    }

    #[test]
    fn a_foreign_keyboard_and_an_unknown_key_cannot_borrow_the_held_modifier_exemption() {
        let own = PhysicalKey::new(0x1D);
        let held = TalkHeld { control: own, alt: None };
        let event = Event::Key { key: Key::LeftControl, edge: Edge::Down };
        for other in [PhysicalKey::new(512 + 0x1D), None] {
            let (bit, wakes) = watch(held);
            UI.with_borrow_mut(|ui| {
                ui.key(event, own, || panic!("retained physical repeat asks nothing"));
                ui.key(event, other, || true);
            });
            assert_eq!(seen(&bit, &wakes), (1, true));
            disarm();
        }
    }
}
