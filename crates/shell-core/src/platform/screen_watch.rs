//! What may have changed the screen while marks point at it, as the platform reports it: one
//! bit. The Windows sources live in the shell's platform module, and the rules that use the bit
//! in `presenter/watch.rs`. How it fits: docs/development/scene.md, "Screen scenes in the
//! shell".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The watch's one bit: something may have changed the screen. Which key went down, where the
/// pointer was and which window came forward never reach it.
#[derive(Clone, Debug, Default)]
pub struct Changed(Arc<AtomicBool>);

impl Changed {
    /// A source saw one of its events. A fixed amount of work and no allocation, so an input
    /// handler may call it.
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Reads the bit and clears it.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
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
/// After setting the bit a source wakes the UI thread, which calls
/// [`Presenter::source_woke`](crate::presenter::Presenter::source_woke). The wake carries
/// nothing and only has the bit read, so a wake still queued when the next release clears the
/// bit changes nothing. A source never calls
/// [`Presenter::screen_changed`](crate::presenter::Presenter::screen_changed), which sets the
/// bit for an event the shell itself knows of.
pub trait Sources {
    /// Starts every source. False when one couldn't start; the implementation has then stopped
    /// the others, and the caller shows no marks.
    fn start(&mut self, changed: Changed) -> bool;

    /// Stops every source, so nothing sets the bit afterwards.
    fn stop(&mut self);
}
