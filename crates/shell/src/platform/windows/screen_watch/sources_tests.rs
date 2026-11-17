use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use async_channel::Receiver;
use kumochi_shell_core::platform::screen_watch::{Changed, Sources};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::HSTRING;

use super::{
    Armed, Ending, MOUSE_WATCH, Mouse, Native, PRESSES, PresenceEvent, Registration, ScreenSources,
    Shared, Win32, window,
};
use crate::input::{Edge, Event, Key, TalkHeld};

/// Windows, scripted: every call is logged, and a call named in `fail` fails once. A failed
/// `registration` is one that couldn't be read.
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
        if !self.call("registration") {
            return Registration::Unreadable;
        }
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
        let disarmed = self.call("disarm");
        if disarmed {
            *self.armed.borrow_mut() = None;
        }
        disarmed
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
fn start_refuses_a_registration_it_can_t_read_and_takes_nothing() {
    // It may be another component's.
    let (mut sources, fake, _wakes) = sources();
    fake.fail.borrow_mut().push("registration");
    let bit = Changed::default();
    assert!(!sources.start(bit.clone(), TalkHeld::default()));
    assert_eq!(fake.calls(), ["registration"]);
    assert!(!hears(&bit));
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(fake.calls(), START, "nothing was owed");
}

#[test]
fn a_registration_that_can_t_be_read_stays_owed_until_a_read_settles_it() {
    let (mut sources, fake, _wakes) = sources();
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    fake.calls();
    // The read fails at stop. Nothing says the registration is gone, so it isn't removed
    // unverified and it is still owed.
    fake.fail.borrow_mut().push("registration");
    sources.stop();
    assert_eq!(fake.calls(), ["disarm", "registration", "unhook"]);
    assert_eq!(fake.registration.get(), Some(Registration::Ours));

    // It fails again at the next start: nothing is armed over it.
    fake.fail.borrow_mut().push("registration");
    let bit = Changed::default();
    assert!(!sources.start(bit.clone(), TalkHeld::default()));
    assert_eq!(fake.calls(), ["registration"]);
    assert!(fake.armed.borrow().is_none());
    assert!(!hears(&bit));

    // Then it reads as ours: the removal is retried, and that start goes on.
    let bit = Changed::default();
    assert!(sources.start(bit.clone(), TalkHeld::default()));
    let mut calls = vec!["registration", "remove"];
    calls.extend(START);
    assert_eq!(fake.calls(), calls);
    assert!(hears(&bit));
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

fn watching(bit: &Changed, at: u32) -> Armed {
    Armed {
        changed: bit.clone(),
        at,
        woke: false,
    }
}

fn armed(at: u32) -> (Mouse, Changed) {
    let bit = Changed::default();
    let mouse = Mouse {
        armed: Some(watching(&bit, at)),
        ..Mouse::default()
    };
    (mouse, bit)
}

const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const WHEEL: u32 = 0x400;

#[test]
fn a_record_before_the_arm_sets_nothing() {
    let mut mouse = Mouse::default();
    assert!(!mouse.record(10, LEFT_DOWN, || panic!("nothing is armed")));
    assert!(!mouse.unread());
}

#[test]
fn a_move_ends_at_the_mask_and_a_press_counts_only_over_another_process() {
    let (mut mouse, bit) = armed(100);
    for flags in [0, LEFT_UP, !PRESSES] {
        assert!(!mouse.record(100, flags, || panic!("a move reads no pointer")));
    }
    assert!(!mouse.record(100, LEFT_DOWN, || false), "over Kumochi");
    assert!(mouse.record(100, LEFT_DOWN, || true));
    assert!(
        !mouse.record(101, LEFT_DOWN, || true),
        "one wake for an arm"
    );
    assert!(!bit.set());
    for flags in [4, 16, 64, 256, WHEEL, 0x800] {
        let (mut mouse, _bit) = armed(100);
        assert!(mouse.record(100, flags, || true), "{flags:#x}");
    }
}

#[test]
fn an_arm_wakes_once_however_often_its_bit_is_read_and_set_again() {
    let (mut mouse, bit) = armed(100);
    assert!(mouse.record(100, LEFT_DOWN, || true));
    // The presenter read the bit and stopped the watch; its disarm is still queued behind
    // these records.
    bit.clear();
    assert!(!mouse.record(101, WHEEL, || true), "no second wake");
    assert!(!bit.set(), "the bit was set all the same");
    bit.clear();
    assert!(!mouse.unread());
    assert!(!bit.set());
    // Nor does the thread's end owe a second one.
    bit.clear();
    assert!(!mouse.lost([]));
    assert!(!bit.set(), "the thread's end set it");

    // A bit the keyboard half set first owes the mouse's wake at its own first set.
    let (mut mouse, bit) = armed(100);
    assert!(bit.set());
    assert!(!mouse.record(100, LEFT_DOWN, || true));
    bit.clear();
    assert!(mouse.record(101, LEFT_DOWN, || true));
}

#[test]
fn a_record_between_stop_and_the_disarm_sets_only_the_stopped_watch_s_bit() {
    // The UI thread stopped the first watch; its disarm is still queued behind this record.
    let (mut mouse, first) = armed(100);
    assert!(mouse.record(150, LEFT_DOWN, || true));
    assert!(!first.set(), "the stopped watch's bit, which nobody reads");
    // The disarm, then the next release's arm with a bit of its own, and a record that was
    // queued before that arm.
    mouse.armed = None;
    let second = Changed::default();
    mouse.armed = Some(watching(&second, 200));
    assert!(!mouse.record(150, LEFT_DOWN, || panic!("the watch before's")));
    assert!(second.set(), "the new watch's bit stays clear");
}

#[test]
fn a_restart_drops_a_record_queued_from_before_it_and_keeps_one_from_its_own_tick() {
    let (mut mouse, bit) = armed(200);
    assert!(!mouse.record(199, LEFT_DOWN, || panic!("the watch before's")));
    assert!(!mouse.record(150, WHEEL, || panic!("the watch before's")));
    assert!(mouse.record(200, LEFT_DOWN, || true), "the arm's own tick");
    assert!(!bit.set());
    // Ticks wrap after 49.7 days.
    let (mut mouse, _bit) = armed(u32::MAX - 5);
    assert!(!mouse.record(u32::MAX - 6, LEFT_DOWN, || panic!("earlier")));
    assert!(mouse.record(3, LEFT_DOWN, || true), "later, past the wrap");
}

#[test]
fn an_unreadable_record_counts_as_a_change_once() {
    let (mut mouse, bit) = armed(100);
    assert!(mouse.unread());
    assert!(!mouse.unread());
    assert!(!bit.set());
}

/// The mouse thread ends as `mouse_thread` ends it, by its guard. The test's own thread stands
/// for the mouse thread, with no window made.
fn end(shared: &Shared) {
    let class = HSTRING::new();
    drop(Ending {
        shared,
        class: &class,
        made: None,
    });
}

/// This thread as a mouse thread whose wakes can be counted.
fn mouse_thread(shared: &Arc<Shared>) -> Receiver<PresenceEvent> {
    let (events, wakes) = async_channel::unbounded();
    MOUSE_WATCH.with_borrow_mut(|mouse| {
        mouse.events = Some(events);
        mouse.shared = Some(Arc::clone(shared));
    });
    wakes
}

#[test]
fn quit_before_and_during_publication_ends_the_thread_with_nothing_published() {
    let shared = Shared::default();
    assert_eq!(shared.quit(), None, "nothing to close yet");
    assert!(
        !shared.publish(7),
        "the thread destroys its window and ends"
    );
    assert_eq!(shared.window(), None);
    end(&shared);
    assert!(shared.exited.load(Ordering::SeqCst));

    // Quit comes once the window is published, and an arm was posted in between.
    let shared = Arc::new(Shared::default());
    let wakes = mouse_thread(&shared);
    assert!(shared.publish(7));
    assert_eq!(shared.window(), Some(7));
    let bit = Changed::default();
    shared.pending().push_back(bit.clone());
    assert_eq!(shared.quit(), Some(7), "a published window is closed");
    end(&shared);
    assert_eq!(shared.window(), None);
    assert!(shared.exited.load(Ordering::SeqCst));
    assert!(shared.pending().is_empty());
    assert!(
        !bit.set(),
        "the arm that was never dispatched counts as changed"
    );
    assert_eq!(wakes.try_recv(), Ok(PresenceEvent::ScreenWoke));
    assert!(wakes.is_empty());
}

#[test]
fn a_mouse_thread_that_ends_invalidates_its_armed_watch_once() {
    let shared = Arc::new(Shared::default());
    let wakes = mouse_thread(&shared);
    assert!(shared.publish(7));
    let bit = Changed::default();
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = Some(watching(&bit, 100)));
    end(&shared);
    assert!(!bit.set(), "a watch with no mouse source counts as changed");
    assert_eq!(wakes.try_recv(), Ok(PresenceEvent::ScreenWoke));
    assert!(wakes.is_empty(), "one wake");
    MOUSE_WATCH.with_borrow(|mouse| assert!(mouse.armed.is_none()));
    assert_eq!(shared.window(), None);
    assert!(shared.exited.load(Ordering::SeqCst));

    // No watch can be armed afterwards, and a refused arm leaves no bit waiting.
    let native = Win32 {
        shared: Arc::clone(&shared),
        hook: None,
    };
    assert!(!native.arm(Changed::default()));
    assert!(shared.pending().is_empty());
}

#[test]
fn a_mouse_thread_that_ends_invalidates_an_arm_it_never_dispatched() {
    let shared = Arc::new(Shared::default());
    let wakes = mouse_thread(&shared);
    assert!(shared.publish(7));
    // The watch before is still armed, stopped; the next watch's arm is posted and waiting.
    let (stopped, waiting) = (Changed::default(), Changed::default());
    assert!(stopped.set());
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = Some(watching(&stopped, 100)));
    shared.pending().push_back(waiting.clone());
    end(&shared);
    assert!(!waiting.set(), "the waiting watch counts as changed");
    assert_eq!(wakes.try_recv(), Ok(PresenceEvent::ScreenWoke));
    assert!(wakes.is_empty(), "one wake");
    assert!(shared.pending().is_empty(), "and its bit waits no longer");
}

#[test]
fn each_arm_takes_the_oldest_waiting_bit() {
    // Two releases in a row: arm, disarm and arm are all queued before the first is handled.
    let shared = Arc::new(Shared::default());
    let _wakes = mouse_thread(&shared);
    let (first, second) = (Changed::default(), Changed::default());
    shared.pending().push_back(first.clone());
    shared.pending().push_back(second.clone());
    let record = || MOUSE_WATCH.with_borrow_mut(|mouse| mouse.record(300, LEFT_DOWN, || true));
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.arm(100));
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.armed = None);
    MOUSE_WATCH.with_borrow_mut(|mouse| mouse.arm(200));
    assert!(record());
    assert!(!second.set(), "the second arm's bit was set");
    assert!(first.set(), "and the first's was not");
    assert!(shared.pending().is_empty());
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

/// What Windows does with a mouse registration whose target window is destroyed: the worker
/// ends with its registration standing, and the registrations are read once its window is
/// really gone. Registers the mouse's Raw Input and a foreground hook for milliseconds, so it
/// runs only in an approved desktop session, and alone, since the registration is the
/// process's: `cargo test -p kumochi-shell -- --ignored --exact` with this test's path.
#[test]
#[ignore = "registers the mouse's Raw Input and a foreground hook briefly; run only in an approved session"]
fn a_mouse_registration_ends_with_the_worker_s_window() {
    let wait = |ready: &dyn Fn() -> bool| {
        let end = Instant::now() + Duration::from_secs(5);
        while !ready() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        ready()
    };
    let (events, _wakes) = async_channel::unbounded();
    let mut sources = ScreenSources::new(events);
    let shared = Arc::clone(&sources.native.shared);
    assert!(wait(&|| shared.window().is_some()), "the mouse window");
    let target = window(shared.window().expect("published"));
    assert_eq!(
        sources.native.registration(),
        Registration::None,
        "the mouse must be unregistered before this fixture runs"
    );
    assert!(sources.start(Changed::default(), TalkHeld::default()));
    assert_eq!(sources.native.registration(), Registration::Ours);

    // The worker's window closes with the registration still made. The `exited` flag is set
    // as the thread's end begins, so the window itself is asked for.
    sources.native.quit();
    // SAFETY: IsWindow only checks the handle; it never dereferences or changes the window.
    let gone = || !unsafe { IsWindow(Some(target)) }.as_bool();
    assert!(wait(&gone), "the worker's window was destroyed");
    assert_eq!(shared.window(), None);
    // Nothing is published now, so a registration that outlived its window would read as
    // another component's.
    assert_eq!(
        sources.native.registration(),
        Registration::None,
        "Windows dropped the registration with its window"
    );
    sources.stop();
    assert!(sources.native.hook.is_none());
    assert!(
        !sources.start(Changed::default(), TalkHeld::default()),
        "no thread, no watch"
    );
    assert_eq!(sources.native.registration(), Registration::None);
}
