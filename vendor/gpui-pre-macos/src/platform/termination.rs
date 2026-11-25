// Added for Kumochi under Apache-2.0: opt-in AppKit termination negotiation.
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use std::{
    cell::RefCell,
    fmt,
    rc::{Rc, Weak},
};

type Handler = Box<dyn FnMut(TerminationRequest)>;
type MainJob = Box<dyn FnOnce()>;
type MainQueue = Box<dyn Fn(MainJob)>;

/// One main-thread-only reply to an opt-in native termination request.
///
/// Dropping an unanswered token queues rejection. Neither reply nor Drop calls AppKit
/// inline. A stale/disposed token cannot approve another request. This is not a shutdown
/// receipt: the application must retain its cleanup owners until they actually retire.
#[must_use = "retain until cleanup completes, or drop to reject termination"]
pub struct TerminationRequest {
    control: Weak<Control>,
    epoch: Option<u64>,
}

impl fmt::Debug for TerminationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TerminationRequest { .. }")
    }
}

impl TerminationRequest {
    /// Queues a decision on the main executor, consuming this token.
    ///
    /// YES is permitted only after the application's shared cleanup reports Closed.
    /// NO does not cancel cleanup or create an app-owned Quit intent. AppKit may have
    /// canceled independently; local epochs are not an OS cancellation receipt.
    pub fn reply(mut self, allow: bool) {
        if let Some(epoch) = self.epoch.take()
            && let Some(control) = self.control.upgrade()
        {
            control.queue_reply(epoch, allow);
        }
    }
}

impl Drop for TerminationRequest {
    fn drop(&mut self) {
        if let Some(epoch) = self.epoch.take()
            && let Some(control) = self.control.upgrade()
        {
            control.queue_reply(epoch, false);
        }
    }
}

/// Approves the app's own termination ahead of time, once its cleanup has finished.
///
/// A later request then answers Now with no queued delivery. GPUI's `quit()` sends
/// `terminate:` from a main-queue block, and AppKit waits for a Later reply inside that block,
/// so a reply delivered by a later job on the same serial queue never arrives. Requests already
/// pending keep their own answer, and disposal still refuses.
#[derive(Clone)]
pub struct TerminationApproval {
    control: Weak<Control>,
}

impl fmt::Debug for TerminationApproval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TerminationApproval { .. }")
    }
}

impl TerminationApproval {
    pub(super) fn new(control: &Rc<Control>) -> Self {
        Self {
            control: Rc::downgrade(control),
        }
    }

    /// Lets every later request terminate at once. Returns whether a request is already in
    /// flight: admitted and still awaiting the app's answer, or with a yes queued. Once cleanup
    /// has finished the app approves that request and it ends the app, so the app must not also
    /// quit for that exit.
    #[must_use]
    pub fn approve(&self) -> bool {
        let Some(control) = self.control.upgrade() else {
            return false;
        };
        let mut state = control.state.borrow_mut();
        state.approved_ahead = true;
        matches!(state.phase, Phase::Pending(_) | Phase::Queued(_, true))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Idle,
    Pending(u64),
    Queued(u64, bool),
    Rejecting(u64),
    Approved,
    Disposed,
}

struct State {
    phase: Phase,
    last_epoch: u64,
    disposing: bool,
    delivering: bool,
    approved_ahead: bool,
}

impl State {
    fn admit(&mut self) -> (NSApplicationTerminateReply, Option<u64>) {
        match self.phase {
            Phase::Idle if !self.disposing && self.approved_ahead => {
                self.phase = Phase::Approved;
                (NSApplicationTerminateReply::TerminateNow, None)
            }
            Phase::Idle if !self.disposing => {
                let Some(epoch) = self.last_epoch.checked_add(1) else {
                    return (NSApplicationTerminateReply::TerminateCancel, None);
                };
                self.last_epoch = epoch;
                self.phase = Phase::Pending(epoch);
                (NSApplicationTerminateReply::TerminateLater, Some(epoch))
            }
            Phase::Pending(_) | Phase::Queued(_, _) => {
                (NSApplicationTerminateReply::TerminateLater, None)
            }
            Phase::Approved => (NSApplicationTerminateReply::TerminateNow, None),
            _ => (NSApplicationTerminateReply::TerminateCancel, None),
        }
    }

    fn queue_reply(&mut self, epoch: u64, allow: bool) -> bool {
        if self.phase != Phase::Pending(epoch) {
            return false;
        }
        self.phase = Phase::Queued(epoch, allow && !self.disposing);
        true
    }

    fn begin_settlement(&mut self, epoch: u64, has_application: bool) -> Option<bool> {
        let Phase::Queued(current, allow) = self.phase else {
            return None;
        };
        if current != epoch {
            return None;
        }
        self.disposing |= !has_application;
        let allow = allow && !self.disposing;
        self.phase = if allow {
            Phase::Approved
        } else {
            Phase::Rejecting(epoch)
        };
        Some(allow)
    }

    fn finish_settlement(&mut self, epoch: u64) {
        if self.phase == Phase::Rejecting(epoch) {
            self.phase = if self.disposing {
                Phase::Disposed
            } else {
                Phase::Idle
            };
        }
    }

    fn dispose(&mut self) -> Option<u64> {
        self.disposing = true;
        match self.phase {
            Phase::Pending(epoch) => {
                self.phase = Phase::Queued(epoch, false);
                Some(epoch)
            }
            Phase::Queued(epoch, _) => {
                self.phase = Phase::Queued(epoch, false);
                None
            }
            Phase::Rejecting(_) => None,
            _ => {
                self.phase = Phase::Disposed;
                None
            }
        }
    }
}

pub(super) struct Control {
    state: RefCell<State>,
    handler: RefCell<Option<Handler>>,
    application: RefCell<Option<Retained<NSApplication>>>,
    queue: MainQueue,
}

impl Control {
    pub(super) fn new(queue: MainQueue, handler: Handler) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(State {
                phase: Phase::Idle,
                last_epoch: 0,
                disposing: false,
                delivering: false,
                approved_ahead: false,
            }),
            handler: RefCell::new(Some(handler)),
            application: RefCell::new(None),
            queue,
        })
    }

    pub(super) fn bind(&self, application: Retained<NSApplication>) {
        if self.state.borrow().phase == Phase::Idle && self.state.borrow().last_epoch == 0 {
            let mut original = self.application.borrow_mut();
            if original.is_none() {
                *original = Some(application);
            }
        }
    }

    pub(super) fn should_terminate(
        self: &Rc<Self>,
        application: *mut AnyObject,
    ) -> NSApplicationTerminateReply {
        let matches = self.application.borrow().as_ref().is_some_and(|original| {
            std::ptr::eq(
                std::ptr::from_ref(&**original).cast::<AnyObject>(),
                application,
            )
        });
        if !matches {
            return NSApplicationTerminateReply::TerminateCancel;
        }
        self.admit()
    }

    fn admit(self: &Rc<Self>) -> NSApplicationTerminateReply {
        let (reply, epoch) = self.state.borrow_mut().admit();
        if let Some(epoch) = epoch {
            let control = Rc::clone(self);
            (self.queue)(Box::new(move || control.deliver(epoch)));
        }
        reply
    }

    fn deliver(self: &Rc<Self>, epoch: u64) {
        if self.state.borrow().phase != Phase::Pending(epoch) {
            return;
        }
        self.state.borrow_mut().delivering = true;
        let mut delivery = Delivery {
            control: Rc::clone(self),
            handler: self.handler.borrow_mut().take(),
        };
        let request = TerminationRequest {
            control: Rc::downgrade(self),
            epoch: Some(epoch),
        };
        if let Some(handler) = delivery.handler.as_mut() {
            handler(request);
        } else {
            drop(request);
        }
    }

    fn queue_reply(self: &Rc<Self>, epoch: u64, allow: bool) {
        let schedule = {
            let mut state = self.state.borrow_mut();
            state.queue_reply(epoch, allow) && !state.delivering
        };
        if schedule {
            self.schedule_settlement(epoch);
        }
    }

    fn schedule_settlement(self: &Rc<Self>, epoch: u64) {
        let control = Rc::clone(self);
        (self.queue)(Box::new(move || control.settle(epoch)));
    }

    fn settle(&self, epoch: u64) {
        if self.state.borrow().delivering {
            return;
        }
        let application = self.application.borrow().clone();
        let allow = self
            .state
            .borrow_mut()
            .begin_settlement(epoch, application.is_some());
        let Some(allow) = allow else {
            return;
        };
        if let Some(application) = application {
            application.replyToApplicationShouldTerminate(allow);
        }
        self.state.borrow_mut().finish_settlement(epoch);
        if self.state.borrow().disposing {
            let application = self.application.borrow_mut().take();
            drop(application);
        }
    }

    pub(super) fn dispose(self: &Rc<Self>) {
        let epoch = {
            let mut state = self.state.borrow_mut();
            state.dispose().filter(|_| !state.delivering)
        };
        let handler = self.handler.borrow_mut().take();
        drop(handler);
        if let Some(epoch) = epoch {
            self.schedule_settlement(epoch);
        }
        if self.state.borrow().phase == Phase::Disposed {
            let application = self.application.borrow_mut().take();
            drop(application);
        }
    }
}

struct Delivery {
    control: Rc<Control>,
    handler: Option<Handler>,
}

impl Drop for Delivery {
    fn drop(&mut self) {
        let (disposing, epoch) = {
            let mut state = self.control.state.borrow_mut();
            state.delivering = false;
            if std::thread::panicking() {
                let _settlement = state.dispose();
            }
            let epoch = match state.phase {
                Phase::Queued(epoch, _) => Some(epoch),
                _ => None,
            };
            (state.disposing, epoch)
        };
        if !disposing {
            *self.control.handler.borrow_mut() = self.handler.take();
        }
        if let Some(epoch) = epoch {
            self.control.schedule_settlement(epoch);
        }
    }
}

#[cfg(test)]
mod tests;
