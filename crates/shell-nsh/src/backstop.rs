//! The backstop: a thread of its own that ends the process once the shell outstays its limit,
//! whatever `NativeShell` is doing. A hidden shell must never keep the session's lock forever.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long a quit may take before the backstop ends the process: the link's `finish_quit`, 13 s
/// (10 s to connect, 3 s to quit), then 200 ms of `NativeShell`'s quit hooks, and about 1.8 s spare.
// shortcut: unmeasured, so a slow but healthy quit can end with 124; set it from the native run.
pub(crate) const GRACE: Duration = Duration::from_secs(15);

/// How long a shell started with no deadline may wait for its first open connection: a cold brain
/// launch and one connect attempt [guess].
// shortcut: a guess, so a slow first connect can end with 124; measure it in the native session.
pub(crate) const START_LIMIT: Duration = Duration::from_secs(30);

/// When the backstop fires if nothing changes it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Limit {
    /// The flag's deadline plus the grace: only a quit's grace can bring it earlier.
    Deadline(Instant),
    /// The start limit, which the first open connection disarms.
    Start(Instant),
}

#[derive(Debug)]
enum Order {
    Connected,
    Quitting,
}

/// How the shell tells the backstop what happened. Dropping every handle disarms nothing.
#[derive(Clone, Debug)]
pub(crate) struct Backstop(Sender<Order>);

impl Backstop {
    /// Starts the thread that runs `expire` once `limit` passes, unless an open connection
    /// disarms the start limit first, or `grace` after a quit begins, whichever comes sooner.
    pub(crate) fn arm(
        limit: Limit,
        grace: Duration,
        expire: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (orders, heard) = mpsc::channel();
        thread::Builder::new()
            .name("shell-backstop".to_owned())
            .spawn(move || {
                if watch(limit, grace, &heard) {
                    expire();
                }
            })?;
        Ok(Self(orders))
    }

    /// The pump applied an open `Connected`: the start limit, if it is still armed, is off.
    pub(crate) fn connected(&self) {
        self.tell(Order::Connected);
    }

    /// A quit began: fire `grace` from now at the latest, so a hung quit still ends the process.
    pub(crate) fn quitting(&self) {
        self.tell(Order::Quitting);
    }

    fn tell(&self, order: Order) {
        // Refused only once the thread has fired, and the process is ending then.
        let _fired = self.0.send(order);
    }
}

/// Follows the orders until the backstop must fire (true), or it is disarmed and no handle is left
/// to arm it again (false).
fn watch(limit: Limit, grace: Duration, heard: &Receiver<Order>) -> bool {
    let (mut at, mut disarmable) = match limit {
        Limit::Deadline(at) => (Some(at), false),
        Limit::Start(at) => (Some(at), true),
    };
    loop {
        let order = match at {
            Some(at) => heard.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => heard.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match order {
            Ok(Order::Connected) if disarmable => {
                at = None;
                disarmable = false;
            }
            Ok(Order::Connected) => {}
            Ok(Order::Quitting) => {
                let by = Instant::now() + grace;
                at = Some(at.map_or(by, |at| at.min(by)));
                disarmable = false;
            }
            Err(RecvTimeoutError::Timeout) => return true,
            // Losing every handle is not a disarm: what is armed still fires.
            Err(RecvTimeoutError::Disconnected) => {
                let Some(at) = at else { return false };
                thread::sleep(at.saturating_duration_since(Instant::now()));
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough that a test never waits it out.
    const NEVER: Duration = Duration::from_secs(600);
    /// How long a test waits for a backstop that should fire; far past every limit it arms.
    const PATIENCE: Duration = Duration::from_secs(10);

    fn armed(limit: Limit, grace: Duration) -> (Backstop, mpsc::Receiver<Instant>) {
        let (fired, heard) = mpsc::channel();
        let backstop = Backstop::arm(limit, grace, move || {
            fired.send(Instant::now()).expect("the test is listening");
        })
        .expect("the backstop starts");
        (backstop, heard)
    }

    #[test]
    fn the_backstop_runs_its_action_once_its_delay_has_passed() {
        let at = Instant::now() + Duration::from_millis(50);
        let (_backstop, heard) = armed(Limit::Deadline(at), NEVER);
        let ran = heard.recv_timeout(PATIENCE).expect("the deadline fires");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }

    #[test]
    fn with_no_flag_the_start_limit_fires_when_nothing_connects() {
        let at = Instant::now() + Duration::from_millis(50);
        let (_backstop, heard) = armed(Limit::Start(at), NEVER);
        let ran = heard.recv_timeout(PATIENCE).expect("the start limit fires");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }

    #[test]
    fn the_first_open_connection_disarms_the_start_limit() {
        let (backstop, heard) = armed(
            Limit::Start(Instant::now() + Duration::from_millis(300)),
            NEVER,
        );
        backstop.connected();
        assert_eq!(
            heard.recv_timeout(Duration::from_secs(2)),
            Err(RecvTimeoutError::Timeout),
            "a disarmed start limit doesn't fire"
        );
    }

    #[test]
    fn a_connection_or_a_quit_never_moves_the_flags_deadline_later() {
        let at = Instant::now() + Duration::from_millis(200);
        let (backstop, heard) = armed(Limit::Deadline(at), NEVER);
        backstop.connected();
        backstop.quitting();
        let ran = heard
            .recv_timeout(PATIENCE)
            .expect("the deadline fires on time");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }

    #[test]
    fn a_quit_switches_to_the_grace_whether_or_not_a_connection_opened() {
        for connected_first in [false, true] {
            let (backstop, heard) = armed(
                Limit::Start(Instant::now() + NEVER),
                Duration::from_millis(50),
            );
            if connected_first {
                backstop.connected();
            }
            backstop.quitting();
            // A connection during the quit doesn't disarm its grace.
            backstop.connected();
            assert!(
                heard.recv_timeout(PATIENCE).is_ok(),
                "the grace fires long before the start limit, connected first: {connected_first}"
            );
        }
    }

    #[test]
    fn losing_every_handle_disarms_nothing() {
        let at = Instant::now() + Duration::from_millis(100);
        let (backstop, heard) = armed(Limit::Start(at), NEVER);
        drop(backstop);
        let ran = heard
            .recv_timeout(PATIENCE)
            .expect("the start limit still fires");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }
}
