use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{OutputGeneration, RunId, SegmentId};
use zeroize::Zeroizing;

use super::super::STALL;
use super::backend::{self, Power, Reply, Request};
use super::timeline::{Anchor, Engine, Observation, Opened, Phase};
use super::{Admission, Credit, Fault, Packet};
use crate::platform::speaker_cpal::{
    Control, OutputError, OutputEvent, PacketKind, Source, TailAck, Transport,
};

const COMMANDS: usize = 16;
const SERVICE: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(super) enum Command {
    Expect,
    Pause(u64),
    Resume(u64),
    Stop,
}

pub(super) struct Input {
    worker: Thread,
    commands: SyncSender<Command>,
    packets: SyncSender<Packet>,
    busy: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl Input {
    pub(super) fn command(&self, command: Command) -> Result<(), TrySendError<Command>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(command));
        }
        if matches!(command, Command::Stop) {
            self.stop.store(true, Ordering::Release);
        } else {
            self.commands.try_send(command)?;
        }
        self.worker.unpark();
        Ok(())
    }

    pub(super) fn submit(&self, packet: Packet) -> Result<(), TrySendError<Packet>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(packet));
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(TrySendError::Full(packet));
        }
        match self.packets.try_send(packet) {
            Ok(()) => {
                self.worker.unpark();
                Ok(())
            }
            Err(err) => {
                self.busy.store(false, Ordering::Release);
                Err(err)
            }
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

struct Port {
    commands: Receiver<Command>,
    packets: Receiver<Packet>,
    busy: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

struct Physical {
    generation: u64,
    transport: Transport,
    flushed: Option<SegmentId>,
    power: Power,
    retire_deadline: Option<Duration>,
    watchdog: Option<(Duration, u64)>,
}

struct Worker {
    engine: Engine,
    physical: Option<Physical>,
    job: Option<Request>,
    pending_packet: Option<Packet>,
    send: SyncSender<Request>,
    replies: Receiver<Reply>,
    route_closed: bool,
}

pub(super) fn spawn(
    run: RunId,
    generation: OutputGeneration,
    give: impl FnMut(Credit) + Send + 'static,
) -> io::Result<Input> {
    let (commands, pending) = mpsc::sync_channel(COMMANDS);
    let (packets, data) = mpsc::sync_channel(1);
    let busy = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let port = Port {
        commands: pending,
        packets: data,
        busy: Arc::clone(&busy),
        stop: Arc::clone(&stop),
        closed: Arc::clone(&closed),
    };
    let task = thread::Builder::new()
        .name("speaker-owner".into())
        .spawn(move || {
            let origin = Instant::now();
            let Ok((send, replies)) = backend::spawn(thread::current(), origin) else {
                return;
            };
            let worker = Worker {
                engine: Engine::new(run, generation),
                physical: None,
                job: None,
                pending_packet: None,
                send,
                replies,
                route_closed: false,
            };
            worker.serve(&port, origin, give);
        })?;
    let worker = task.thread().clone();
    Ok(Input {
        worker,
        commands,
        packets,
        busy,
        stop,
        closed,
    })
}

impl Worker {
    fn serve(mut self, port: &Port, origin: Instant, mut give: impl FnMut(Credit)) {
        let mut source = Zeroizing::new([0; 480]);
        loop {
            let now = origin.elapsed();
            let closing = port.closed.load(Ordering::Acquire);
            let again = self
                .step(port, &mut source, &mut give, now)
                .unwrap_or_else(|_| {
                    port.closed.store(true, Ordering::Release);
                    self.stop(now, &mut |credit| port.credit(credit, &mut give));
                    true
                });
            if again {
                continue;
            }
            if closing
                && self.physical.as_ref().is_none_or(|route| {
                    route
                        .retire_deadline
                        .is_some_and(|deadline| deadline <= now)
                })
            {
                return;
            }
            if let Some(next) = self.deadline(now) {
                thread::park_timeout(next.saturating_sub(origin.elapsed()));
            } else {
                thread::park();
            }
        }
    }

    fn deadline(&self, now: Duration) -> Option<Duration> {
        self.engine
            .active_deadline(now)
            .into_iter()
            .chain(
                self.physical
                    .iter()
                    .flat_map(|route| [route.retire_deadline, route.watchdog.map(|(at, _)| at)])
                    .flatten()
                    .filter(|&deadline| deadline > now),
            )
            .min()
    }

    fn step(
        &mut self,
        port: &Port,
        source: &mut [i16; 480],
        give: &mut impl FnMut(Credit),
        now: Duration,
    ) -> Result<bool, Fault> {
        self.backend_reply(now);
        self.observations(now);
        self.watch_playback(now);
        self.apply_stop(port, give, now);
        if self.pending_packet.is_none() && self.engine.queue.held.is_none() {
            self.pending_packet = port.packets.try_recv().ok();
        }
        let mut drained = false;
        for _ in 0..COMMANDS {
            match port.commands.try_recv() {
                Ok(Command::Expect) => self.engine.expect(now),
                Ok(Command::Pause(serial)) => {
                    self.engine.pause(serial, now);
                    self.inhibit();
                }
                Ok(Command::Resume(serial)) => {
                    self.engine.resume(serial, now);
                    self.inhibit();
                }
                Ok(Command::Stop) => unreachable!("Stop is published outside the mailbox"),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    drained = true;
                    break;
                }
            }
        }
        if !drained {
            return Ok(true);
        }
        self.apply_stop(port, give, now);
        if let Some(credit) = self.engine.tick(now)? {
            port.credit(credit, give);
        }
        if let Some(packet) = self.pending_packet.take()
            && let Admission::Return(credit) = self.engine.offer(packet, now)?
        {
            port.credit(credit, give);
        }
        self.feed(source, now);
        self.drive(now);
        self.watch_playback(now);
        Ok(false)
    }

    fn backend_reply(&mut self, now: Duration) {
        match self.replies.try_recv() {
            Ok(Reply::Opened(generation, transport)) => {
                let Some(Request::Open(pending, _)) = self.job.take() else {
                    return;
                };
                debug_assert_eq!(
                    generation, pending,
                    "the backend answers its only pending job"
                );
                let outcome = if transport.is_some() {
                    Opened::Physical
                } else {
                    Opened::Unavailable
                };
                let accepted = self.engine.opened(generation, outcome, now);
                if let Some(transport) = transport {
                    let mut route = Physical {
                        generation,
                        transport,
                        flushed: None,
                        power: Power::Playing,
                        retire_deadline: None,
                        watchdog: None,
                    };
                    if !accepted {
                        route.retire(now);
                    }
                    self.physical = Some(route);
                    self.inhibit();
                }
            }
            Ok(Reply::Powered(generation, ok)) => {
                let Some(Request::Power(pending, play)) = self.job.take() else {
                    return;
                };
                debug_assert_eq!(
                    generation, pending,
                    "the backend answers its only pending job"
                );
                if let Some(route) = &mut self.physical {
                    if ok {
                        route.power = play;
                    } else {
                        self.recover(now);
                        if play == Power::Playing
                            && let Some(route) = &mut self.physical
                        {
                            route.retire_deadline = Some(now);
                        }
                    }
                }
            }
            Ok(Reply::Retired(generation)) => {
                let Some(Request::Retire(pending)) = self.job.take() else {
                    return;
                };
                debug_assert_eq!(
                    generation, pending,
                    "the backend answers its only pending job"
                );
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.route_closed = true;
                if let Some(Request::Open(generation, _)) = self.job.take() {
                    self.engine.opened(generation, Opened::Unavailable, now);
                }
                self.recover(now);
            }
        }
    }

    fn observations(&mut self, now: Duration) {
        let Some(route) = &mut self.physical else {
            return;
        };
        if route.retire_deadline.is_some() {
            return;
        }
        match route.transport.take_event() {
            Some(OutputEvent::Lost | OutputEvent::Rerouted) => {
                self.recover(now);
                return;
            }
            Some(OutputEvent::Refill) => self.engine.refill(),
            None => {}
        }
        if route.transport.control().overflowed() {
            self.recover(now);
            return;
        }
        for _ in 0..SERVICE {
            let Some(report) = route.transport.presentation() else {
                break;
            };
            if report.source.run != self.engine.queue.run
                || report.source.generation != route.generation
            {
                continue;
            }
            let Some(at) = report.at else {
                self.recover(now);
                return;
            };
            if self.engine.observe(Anchor {
                run: report.source.run,
                generation: report.source.generation,
                segment: report.source.segment,
                first: report.first,
                through: report.through,
                at,
            }) == Observation::Recover
            {
                self.recover(now);
                return;
            }
        }
    }

    fn watch_playback(&mut self, now: Duration) {
        let Some(route) = &mut self.physical else {
            return;
        };
        if route.retire_deadline.is_some() {
            return;
        }
        let resuming = matches!(self.job, Some(Request::Power(_, Power::Playing)));
        let outstanding =
            !self.engine.inhibited() && self.engine.offered > self.engine.progress.consumed;
        if !resuming && !outstanding {
            route.watchdog = None;
            return;
        }
        let callbacks = route.transport.control().callback_count();
        if let Some((at, observed)) = route.watchdog
            && observed == callbacks
        {
            if at <= now {
                self.recover(now);
            }
        } else {
            route.watchdog = Some((now.saturating_add(STALL), callbacks));
        }
    }

    fn recover(&mut self, now: Duration) {
        if let Some(route) = &mut self.physical {
            self.engine.require_recovery(route.generation);
            route.retire(now);
        }
    }

    fn apply_stop(&mut self, port: &Port, give: &mut impl FnMut(Credit), now: Duration) {
        if !self.engine.queue.stopped
            && (port.stop.load(Ordering::Acquire) || port.closed.load(Ordering::Acquire))
        {
            self.stop(now, &mut |credit| port.credit(credit, give));
        }
    }

    fn stop(&mut self, now: Duration, give: &mut impl FnMut(Credit)) {
        if let Some(Request::Open(_, control)) = &self.job {
            control.stop();
        }
        if let Some(route) = &mut self.physical {
            route.retire(now);
        }
        if let (_, Some(credit)) = self.engine.stop(now) {
            give(credit);
        }
    }

    fn inhibit(&self) {
        if let Some(route) = &self.physical {
            if self.engine.inhibited() {
                route.transport.control().inhibit();
            } else {
                route.transport.control().resume();
            }
        }
    }

    fn feed(&mut self, source: &mut [i16; 480], now: Duration) {
        let Some(route) = &mut self.physical else {
            return;
        };
        for _ in 0..SERVICE {
            let Some(feed) = self.engine.feed(source, route.flushed) else {
                break;
            };
            let kind = if feed.last {
                PacketKind::End
            } else {
                PacketKind::More
            };
            match route.transport.submit(
                &source[..feed.len],
                kind,
                Source {
                    run: self.engine.queue.run,
                    generation: route.generation,
                    segment: feed.segment,
                    at: feed.at,
                },
            ) {
                Ok(()) => {
                    self.engine
                        .staged(feed.at + u64::try_from(feed.len).expect("bounded source feed"));
                    if feed.last {
                        route.flushed = Some(feed.segment);
                    }
                }
                Err(OutputError::Full) => break,
                Err(OutputError::Unavailable | OutputError::PacketTooLarge) => {
                    self.recover(now);
                    break;
                }
            }
        }
    }

    fn drive(&mut self, now: Duration) {
        if let Some(route) = &mut self.physical {
            let generation = route.generation;
            if let Some(deadline) = route.retire_deadline {
                let Some(handed) = route.transport.control().fenced_handoff() else {
                    return;
                };
                self.engine.lost(generation, handed, now);
                let tail = route.transport.control().tail();
                if let TailAck::At(at) = tail {
                    self.engine.acknowledge(generation, Some(at));
                }
                if self.job.is_some() {
                    return;
                }
                if tail == TailAck::Pending && now < deadline && !self.route_closed {
                    if route.power == Power::Paused {
                        self.dispatch(Request::Power(generation, Power::Playing));
                    }
                    return;
                }
                drop(self.physical.take());
                if !self.route_closed {
                    self.dispatch(Request::Retire(generation));
                }
            } else if self.job.is_none() {
                let desired = if self.engine.phase == Phase::Idle {
                    Power::Paused
                } else {
                    Power::Playing
                };
                if desired != route.power {
                    self.dispatch(Request::Power(generation, desired));
                }
            }
        } else if self.job.is_none()
            && !self.route_closed
            && let Some(generation) = self.engine.request(now)
        {
            let control = Arc::new(Control::new(thread::current()));
            self.dispatch(Request::Open(generation, control));
            if self.route_closed {
                self.engine.opened(generation, Opened::Unavailable, now);
            }
        }
    }

    fn dispatch(&mut self, request: Request) {
        if self.send.try_send(request.clone()).is_ok() {
            self.job = Some(request);
        } else {
            self.route_closed = true;
        }
    }
}

impl Physical {
    fn retire(&mut self, now: Duration) {
        self.transport.control().stop();
        self.watchdog = None;
        self.retire_deadline
            .get_or_insert(now.saturating_add(super::super::DRAIN));
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl Port {
    fn credit(&self, credit: Credit, give: &mut impl FnMut(Credit)) {
        self.busy.store(false, Ordering::Release);
        give(credit);
    }
}

#[cfg(test)]
mod tests;
