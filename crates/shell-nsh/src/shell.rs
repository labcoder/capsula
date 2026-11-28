//! What the shell knows of the link and decides from it. No `NativeShell` name appears here, so
//! it is tested without the framework; `platform` runs it on the engine thread.

use std::cell::Cell;
use std::time::Instant;

use async_channel::Receiver;
use kumochi_protocol::message::{AudioSession, ControlToBrain, Narration};
use kumochi_shell_core::link::{
    self, DrawingText, LinkEvent, LinkHandle, LinkState, Session, batch, scene_naming,
};
use kumochi_shell_core::presenter::{Naming, Refuser};

use super::backstop::{Backstop, Order};

/// What the root does after an event.
#[derive(Debug)]
pub(crate) enum Act {
    /// A connection is open, so the start or attempt limit is off.
    Opened,
    /// The connection was lost and the link is trying again: with no flag, re-arm the limit.
    Reconnecting,
    /// The link won't come back, or never started: quit.
    Quit,
    /// The mailbox refused a scene refusal, so the connection is sealed here: end it off the
    /// engine thread, and the link reconnects.
    End(LinkHandle, Session),
}

/// The link as this shell follows it, owned by the root's pump alone.
#[derive(Debug)]
pub(crate) struct Shell {
    link: LinkHandle,
    /// The open connection; every grant and refusal goes through it.
    session: Option<Session>,
    scenes: Refuser,
    /// Asked to quit: nobody can press Retry on a shell with no window, so this is final.
    quit: bool,
}

impl Shell {
    pub(crate) fn new(link: LinkHandle) -> Self {
        Self {
            link,
            session: None,
            scenes: Refuser::default(),
            quit: false,
        }
    }

    /// Applies one event, in the order the link sent it, and says what the root must do.
    pub(crate) fn apply(&mut self, event: LinkEvent) -> Option<Act> {
        if self.quit {
            return None;
        }
        match event {
            LinkEvent::Connected(session) => {
                self.scenes.connected(session.generation());
                let open = session.open();
                self.session = Some(session);
                open.then_some(Act::Opened)
            }
            LinkEvent::State(state) => {
                self.session = None;
                self.scenes.disconnected();
                match state {
                    LinkState::Failed | LinkState::OutputUnavailable => self.ask_quit(),
                    LinkState::Reconnecting => Some(Act::Reconnecting),
                    LinkState::Connecting | LinkState::Connected => None,
                }
            }
            LinkEvent::Data {
                generation,
                message,
                bytes,
            } => self.frame(generation, scene_naming(&message), bytes),
            LinkEvent::SceneData {
                generation,
                frame,
                bytes,
            } => self.frame(generation, scene_naming(&frame), bytes),
            // Control lists owe no credit, settings and account state come unasked on connect, and
            // the rest answer flows this shell never starts.
            _ => None,
        }
    }

    /// The event channel ended, so the link is gone: quit, unless a failure already asked.
    pub(crate) fn closed(&mut self) -> Option<Act> {
        self.ask_quit()
    }

    fn ask_quit(&mut self) -> Option<Act> {
        (!std::mem::replace(&mut self.quit, true)).then_some(Act::Quit)
    }

    /// One delivered frame: its credit on its own connection, then a refusal of the scene a call
    /// draws on. A refusal the mailbox won't take seals the connection, as in the GPUI shell.
    fn frame(&mut self, generation: u64, naming: Option<Naming<'_>>, bytes: u32) -> Option<Act> {
        let disposal = self
            .scenes
            .data(generation, naming.unwrap_or(Naming::Other), bytes)?;
        let session = self.session.as_ref()?;
        session.grant(disposal.credit);
        let scene = disposal.refuse?;
        if self
            .link
            .send_control(session, ControlToBrain::SceneFailed { scene })
        {
            return None;
        }
        let session = self.session.take()?;
        session.close();
        self.scenes.disconnected();
        Some(Act::End(self.link.clone(), session))
    }
}

/// Drains `events` in order for as long as the link sends them, telling `act` what each asks,
/// then asks for quit when the channel ends. `link` is `None` when it never started.
pub(crate) async fn pump(
    link: Option<LinkHandle>,
    events: Receiver<LinkEvent>,
    mut act: impl FnMut(Act),
) {
    let Some(link) = link else {
        act(Act::Quit);
        // Only once the quit has armed the grace, since a log write can block.
        tracing::error!("can't start the link");
        return;
    };
    let mut shell = Shell::new(link);
    while let Ok(first) = events.recv().await {
        let mut ready = batch(first, &events);
        for event in ready.drain(..) {
            if let Some(next) = shell.apply(event) {
                act(next);
            }
        }
        ready.attribute(ready.capacity() * size_of::<LinkEvent>());
    }
    if let Some(next) = shell.closed() {
        act(next);
    }
}

/// Starts the scene dropper, then the link on `Legacy` with no audio output announced, so the
/// brain makes no speech for it. `None` when either can't start, and the pump logs it after
/// asking for quit; nothing then holds the sender, so no clone of it outlives the link.
pub(crate) fn started() -> (Option<LinkHandle>, Receiver<LinkEvent>) {
    // Unbounded and always drained: a Legacy admission can poll a full receiver forever.
    let (sender, events) = async_channel::unbounded();
    let link = kumochi_scene::deferred::start()
        .and_then(|()| link::prepare(sender, AudioSession::Legacy, Narration::Plain))
        .and_then(|(link, start)| start.spawn(|_| DrawingText::default()).map(|()| link))
        .ok();
    (link, events)
}

/// The one quit: every way out asks it, and only the first ask goes on.
#[derive(Debug)]
pub(crate) struct Quit {
    backstop: Backstop,
    link: Option<LinkHandle>,
    asked: Cell<bool>,
}

/// What a begun quit waits for before `NativeShell`'s `quit()`.
#[derive(Debug)]
pub(crate) enum Begun {
    /// `finish_quit` on this link, off the engine thread, since it can take 13 s.
    Finish(LinkHandle),
    /// Nothing: quit now.
    Now,
}

impl Quit {
    pub(crate) fn new(backstop: Backstop, link: Option<LinkHandle>) -> Self {
        Self {
            backstop,
            link,
            asked: Cell::new(false),
        }
    }

    /// The first ask arms the backstop's grace, then marks Quit on the link; later asks get
    /// `None`.
    pub(crate) fn begin(&self) -> Option<Begun> {
        if self.asked.replace(true) {
            return None;
        }
        self.backstop.tell(Order::Quitting(Instant::now()));
        Some(match &self.link {
            Some(link) if link.begin_quit() => Begun::Finish(link.clone()),
            _ => Begun::Now,
        })
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
