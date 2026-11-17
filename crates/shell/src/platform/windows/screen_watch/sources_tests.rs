use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use async_channel::Receiver;
use kumochi_shell_core::platform::screen_watch::{Changed, Sources};

use super::{Mouse, Native, PRESSES, PresenceEvent, Registration, ScreenSources, Shared};
use crate::input::{Edge, Event, Key, TalkHeld};

/// Windows, scripted: every call is logged, and a call named in `fail` fails once.
#[derive(Clone, Default)]
struct Fake {
    log: Rc<RefCell<Vec<&'static str>>>,
    fail: Rc<RefCell<Vec<&'static str>>>,
    registration: Rc<Cell<Option<Registration>>>,
    armed: Rc<RefCell<Option<Changed>>>,
}

impl Fake {
    fn call(&self, name: &'static str) -> bool {
        self.log.borrow_mut().push(name);
        let mut fail = self.fail.borrow_mut();
        let failed = fail.iter().position(|failing| *failing == name);
        failed.map(|at| fail.remove(at)).is_none()
    }

    /// The calls since the last look.
    fn calls(&self) -> Vec<&'static str> {
        std::mem::take(&mut *self.log.borrow_mut())
    }
}

impl Native for Fake {
    fn registration(&self) -> Registration {
        self.call("registration");
        self.registration.get().unwrap_or(Registration::None)
    }

    fn arm(&self, changed: Changed) -> bool {
        let armed = self.call("arm");
        if armed {
            *self.armed.borrow_mut() = Some(changed);
        }
        armed
    }

    fn disarm(&self) -> bool {
        *self.armed.borrow_mut() = None;
        self.call("disarm")
    }

    fn register(&self) -> bool {
        let registered = self.call("register");
        if registered {
            self.registration.set(Some(Registration::Ours));
        }
        registered
    }

    fn remove(&self) -> bool {
        let removed = self.call("remove");
        if removed {
            self.registration.set(None);
        }
        removed
    }

    fn hook(&mut self) -> bool {
        self.call("hook")
    }

    fn unhook(&mut self) -> bool {
        self.call("unhook")
    }
}

fn sources() -> (ScreenSources<Fake>, Fake, Receiver<PresenceEvent>) {
    let fake = Fake::default();
    let (events, wakes) = async_channel::unbounded();
    (ScreenSources::over(fake.clone(), events), fake, wakes)
}

/// Whether the UI thread's half hears a key in another app, which only an armed watch does.
fn hears(bit: &Changed) -> bool {
    super::super::heard_for_test(Event::Key {
        key: Key::Other(0x1E),
        edge: Edge::Down,
    });
    !bit.set()
}

const START: [&str; 4] = ["registration", "arm", "register", "hook"];

#[test]
fn start_arms_before_it_registers_and_stop_gives_everything_back_once() {
    let (mut sources, fake, wakes) = sources();
    let bit = Changed::default();
    assert!(sources.start(bit.clone(), TalkHeld::default()));
    assert_eq!(fake.calls(), START);
    assert!(
        fake.armed.borrow().is_some(),
        "the mouse thread has the bit"
    );
    assert!(hears(&bit));
    assert_eq!(wakes.try_recv(), Ok(PresenceEvent::ScreenWoke));

    sources.stop();
    assert_eq!(fake.calls(), ["disarm", "registration", "remove", "unhook"]);
    assert_eq!(fake.registration.get(), None);
    sources.stop();
    assert_eq!(fake.calls(), [""; 0], "a second stop does nothing");
    assert!(wakes.is_empty());
}

#[test]
fn start_refuses_over_another_component_s_mouse_registration_and_takes_nothing() {
    let (mut sources, fake, _wakes) = sources();
    fake.registration.set(Some(Registration::Other));
    let bit = Changed::default();
    assert!(!sources.start(bit.clone(), TalkHeld::default()));
    assert_eq!(fake.calls(), ["registration"]);
    assert_eq!(fake.registration.get(), Some(Registration::Other));
    assert!(!hears(&bit));
    sources.stop();
    assert_eq!(fake.calls(), [""; 0]);
}

#[test]
fn a_step_that_fails_undoes_the_steps_before_it() {
    for (failing, calls) in [
        ("arm", vec!["registration", "arm"]),
        (
            "register",
            vec!["registration", "arm", "register", "disarm"],
        ),
        (
            "hook",
            vec![
                "registration",
                "arm",
                "register",
                "hook",
                "registration",
                "remove",
                "disarm",
            ],
        ),
    ] {
        let (mut sources, fake, wakes) = sources();
        fake.fail.borrow_mut().push(failing);
        let bit = Changed::default();
        assert!(
            !sources.start(bit.clone(), TalkHeld::default()),
            "{failing}"
        );
        assert_eq!(fake.calls(), calls, "{failing}");
        assert_eq!(fake.registration.get(), None, "{failing}");
        assert!(fake.armed.borrow().is_none(), "{failing}");
        assert!(!hears(&bit), "{failing}");
        assert!(wakes.is_empty());
        // The failure was that start's alone.
        assert!(sources.start(Changed::default(), TalkHeld::default()));
        assert_eq!(fake.calls(), START, "{failing}");
    }
}

#[test]
fn a_removal_that_fails_twice_keeps_the_next_start_from_arming() {
    let (mut sources, fake, _wakes) = sources();
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    fake.calls();
    fake.fail.borrow_mut().extend(["remove", "remove"]);
    sources.stop();
    assert_eq!(fake.calls(), ["disarm", "registration", "remove", "unhook"]);
    assert_eq!(fake.registration.get(), Some(Registration::Ours));

    let bit = Changed::default();
    assert!(!sources.start(bit.clone(), TalkHeld::default()));
    assert_eq!(
        fake.calls(),
        ["registration", "remove"],
        "nothing armed over it"
    );
    assert!(fake.armed.borrow().is_none());
    assert!(!hears(&bit));

    let bit = Changed::default();
    assert!(sources.start(bit.clone(), TalkHeld::default()));
    let mut calls = vec!["registration", "remove"];
    calls.extend(START);
    assert_eq!(fake.calls(), calls);
    assert!(hears(&bit));
}

#[test]
fn a_registration_another_component_replaced_is_forgotten_not_removed() {
    let (mut sources, fake, _wakes) = sources();
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    fake.fail.borrow_mut().push("remove");
    sources.stop();
    fake.calls();
    fake.registration.set(Some(Registration::Other));
    assert!(!sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(
        fake.calls(),
        ["registration", "registration"],
        "theirs is neither removed nor replaced"
    );
    fake.registration.set(None);
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(fake.calls(), START, "nothing of ours was left to retry");
}

#[test]
fn a_hook_that_cannot_be_removed_keeps_the_next_start_from_arming() {
    let (mut sources, fake, _wakes) = sources();
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    fake.calls();
    fake.fail.borrow_mut().extend(["unhook", "unhook"]);
    sources.stop();
    assert!(!sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(
        fake.calls(),
        ["disarm", "registration", "remove", "unhook", "unhook"]
    );
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    let mut calls = vec!["unhook"];
    calls.extend(START);
    assert_eq!(fake.calls(), calls);
}

#[test]
fn dropping_the_sources_stops_them() {
    let (mut sources, fake, _wakes) = sources();
    let bit = Changed::default();
    assert!(sources.start(bit.clone(), TalkHeld::default()));
    fake.calls();
    drop(sources);
    assert_eq!(fake.calls(), ["disarm", "registration", "remove", "unhook"]);
    assert!(!hears(&bit));
}

fn armed(at: u32) -> (Mouse, Changed) {
    let bit = Changed::default();
    let mouse = Mouse {
        armed: Some((bit.clone(), at)),
        events: None,
    };
    (mouse, bit)
}

const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const WHEEL: u32 = 0x400;

#[test]
fn a_record_before_the_arm_sets_nothing() {
    let mouse = Mouse::default();
    assert!(!mouse.record(10, LEFT_DOWN, || panic!("nothing is armed")));
    assert!(!mouse.unread());
}

#[test]
fn a_move_ends_at_the_mask_and_a_press_counts_only_over_another_process() {
    let (mouse, bit) = armed(100);
    for flags in [0, LEFT_UP, !PRESSES] {
        assert!(!mouse.record(100, flags, || panic!("a move reads no pointer")));
    }
    assert!(!mouse.record(100, LEFT_DOWN, || false), "over Kumochi");
    assert!(mouse.record(100, LEFT_DOWN, || true));
    assert!(
        !mouse.record(101, LEFT_DOWN, || true),
        "one wake for a watch"
    );
    assert!(!bit.set());
    for flags in [4, 16, 64, 256, WHEEL, 0x800] {
        let (mouse, _bit) = armed(100);
        assert!(mouse.record(100, flags, || true), "{flags:#x}");
    }
}

#[test]
fn a_record_between_stop_and_the_disarm_sets_only_the_stopped_watch_s_bit() {
    // The UI thread stopped the first watch; its disarm is still queued behind this record.
    let (mut mouse, first) = armed(100);
    assert!(mouse.record(150, LEFT_DOWN, || true));
    assert!(!first.set(), "the stopped watch's bit, which nobody reads");
    // The disarm, then the next release's arm with a bit of its own.
    mouse.armed = None;
    let second = Changed::default();
    mouse.armed = Some((second.clone(), 200));
    assert!(second.set(), "the new watch's bit was never touched");
}

#[test]
fn a_restart_drops_a_record_queued_from_before_it_and_keeps_one_from_its_own_tick() {
    let (mouse, bit) = armed(200);
    assert!(!mouse.record(199, LEFT_DOWN, || panic!("the watch before's")));
    assert!(!mouse.record(150, WHEEL, || panic!("the watch before's")));
    assert!(mouse.record(200, LEFT_DOWN, || true), "the arm's own tick");
    assert!(!bit.set());
    // Ticks wrap after 49.7 days.
    let (mouse, _bit) = armed(u32::MAX - 5);
    assert!(!mouse.record(u32::MAX - 6, LEFT_DOWN, || panic!("earlier")));
    assert!(mouse.record(3, LEFT_DOWN, || true), "later, past the wrap");
}

#[test]
fn an_unreadable_record_counts_as_a_change_once() {
    let (mouse, bit) = armed(100);
    assert!(mouse.unread());
    assert!(!mouse.unread());
    assert!(!bit.set());
}

#[test]
fn quit_before_publication_ends_the_thread_with_no_window_to_close() {
    let shared = Shared::default();
    assert_eq!(shared.quit(), None, "nothing to close yet");
    assert!(
        !shared.publish(7),
        "the thread destroys its window and ends"
    );
    assert_eq!(shared.window(), None);
    shared.exit();
    assert!(shared.exited.load(Ordering::SeqCst));

    let shared = Shared::default();
    assert!(shared.publish(7));
    assert_eq!(shared.window(), Some(7));
    assert_eq!(shared.quit(), Some(7), "a published window is closed");
    shared.exit();
    assert_eq!(shared.window(), None);
    assert!(shared.exited.load(Ordering::SeqCst));
}

/// Registers the mouse's Raw Input and a foreground hook for milliseconds, so it runs only in
/// an approved desktop session: `cargo test -p kumochi-shell -- --ignored the_real_sources`.
#[test]
#[ignore = "registers the mouse's Raw Input and a foreground hook briefly; run only in an approved session"]
fn the_real_sources_register_the_mouse_to_their_own_window_and_leave_nothing() {
    let wait = |ready: &dyn Fn() -> bool| {
        let end = Instant::now() + Duration::from_secs(5);
        while !ready() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        ready()
    };
    let (events, _wakes) = async_channel::unbounded();
    let mut sources = ScreenSources::new(events);
    let shared = std::sync::Arc::clone(&sources.native.shared);
    assert!(wait(&|| shared.window().is_some()), "the mouse window");
    assert_eq!(
        sources.native.registration(),
        Registration::None,
        "the mouse must be unregistered before this fixture runs"
    );
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(sources.native.registration(), Registration::Ours);
    assert!(sources.native.hook.is_some());
    sources.stop();
    assert_eq!(sources.native.registration(), Registration::None);
    assert!(sources.native.hook.is_none());
    assert!(
        sources.start(Changed::default(), TalkHeld::default()),
        "again"
    );
    drop(sources);
    assert!(
        wait(&|| shared.exited.load(Ordering::SeqCst)),
        "the thread ended"
    );
    assert_eq!(shared.window(), None);
}
