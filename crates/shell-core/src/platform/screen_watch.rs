//! What may have changed the screen while marks point at it, as the platform reports it: one
//! bit. The Windows sources live in the shell's platform module, and the rules that use the bit
//! in `presenter/watch.rs`. How it fits: docs/development/scene.md, "Screen scenes in the
//! shell".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::input::TalkHeld;

/// The watch's one bit: something may have changed the screen. Which key went down, where the
/// pointer was and which window came forward never reach it.
#[derive(Clone, Debug, Default)]
pub struct Changed(Arc<AtomicBool>);

impl Changed {
    /// A source saw one of its events. True when this set the bit, so a source wakes the UI
    /// once for a change and not for every event after it. A fixed amount of work and no
    /// allocation, so an input handler may call it.
    pub fn set(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Reads the bit and clears it. A clear bit is only read, so the frame that finds nothing
    /// changed writes nothing.
    pub(crate) fn take(&self) -> bool {
        self.0.load(Ordering::Acquire) && self.0.swap(false, Ordering::AcqRel)
    }
}

/// The platform's event sources, held only while a watch listens. Between `start` and `stop`
/// each of these sets the bit, and nothing else does:
///
/// - a key going down while the foreground window belongs to another process;
/// - a mouse button going down, or the wheel turning (never a pointer move: the user is likely
///   moving towards the thing);
/// - another process's window coming to the front;
/// - the session locking, the desktop switching, or the computer suspending;
/// - the display's resolution, scaling or arrangement changing.
///
/// A source whose [`Changed::set`] set the bit wakes the UI thread, which calls
/// [`Presenter::source_woke`](crate::presenter::Presenter::source_woke). The wake carries
/// nothing and only has the bit read. Every release starts the sources again on a bit of its
/// own, so a wake still queued from the watch before, or a handler of it still running,
/// changes nothing. A source never calls
/// [`Presenter::screen_changed`](crate::presenter::Presenter::screen_changed), which sets the
/// bit for an event the shell itself knows of.
pub trait Sources {
    /// Starts every source on `changed`, this watch's own bit. `held` names the talk keys
    /// still down from the hold just released: until each comes up, its repeats are the
    /// hold's and set nothing. False when one couldn't start; the implementation has then
    /// stopped the others, and the caller shows no marks.
    fn start(&mut self, changed: Changed, held: TalkHeld) -> bool;

    /// Stops every source, so nothing sets the bit afterwards.
    fn stop(&mut self);
}
