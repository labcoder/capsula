//! The backstop: a thread of its own that ends the process once the shell outstays its limit,
//! whatever `NativeShell` is doing. A hidden shell must never keep the session's lock forever.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// How long a quit may take before the backstop ends the process: the link's `finish_quit`, 13 s
/// (10 s to connect, 3 s to quit), then 200 ms of `NativeShell`'s quit hooks, and about 1.8 s spare.
// shortcut: unmeasured, so a slow but healthy quit can end with 124; set it from the native run.
pub(crate) const GRACE: Duration = Duration::from_secs(15);

/// How long a shell with no deadline may wait for an open connection, at start and again after a
/// loss: a cold brain launch and one connect attempt [guess].
// shortcut: a guess, so a slow connect can end with 124; measure it in the native session.
pub(crate) const START_LIMIT: Duration = Duration::from_secs(30);

/// When the backstop fires if nothing changes it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Limit {
    /// The flag's deadline plus the grace: only a quit's grace can bring it earlier.
    Deadline(Instant),
    /// No flag: the attempt limit from this instant, which an open connection disarms.
    Start(Instant),
}

impl Limit {
    /// The limit for a shell that took its lock at `now`, given its `--quit-after-ms`.
    pub(crate) fn at_start(now: Instant, quit_after: Option<Duration>) -> Self {
        quit_after.map_or(Self::Start(now), |after| {
            Self::Deadline(now + after + GRACE)
        })
    }
}

/// What the shell tells the backstop, with the instant it was decided.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Order {
    /// The pump applied an open `Connected`.
    Connected(Instant),
    /// The link lost its connection and is trying again.
    Reconnecting(Instant),
    /// A quit was asked.
    Quitting(Instant),
}

/// The backstop's rules as plain state: when it fires, given the orders so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Watch {
    /// The flag's deadline plus the grace.
    Deadline(Instant),
    /// No flag: the start or attempt limit.
    Attempt(Instant),
    /// No flag, and an open connection.
    Connected,
    /// A quit's grace, or the flag's sooner deadline; nothing disarms it.
    Quitting(Instant),
}

impl Watch {
    pub(crate) fn new(limit: Limit, attempt: Duration) -> Self {
        match limit {
            Limit::Deadline(at) => Self::Deadline(at),
            Limit::Start(from) => Self::Attempt(from + attempt),
        }
    }

    /// When it fires; `None` while a connection is open with no flag.
    pub(crate) fn at(self) -> Option<Instant> {
        match self {
            Self::Deadline(at) | Self::Attempt(at) | Self::Quitting(at) => Some(at),
            Self::Connected => None,
        }
    }

    /// Applies `order`. One decided at or after the time it would fire changes nothing, so what
    /// was armed fires however late the thread reads it.
    #[must_use]
    pub(crate) fn heard(self, order: Order, grace: Duration, attempt: Duration) -> Self {
        let (Order::Connected(sent) | Order::Reconnecting(sent) | Order::Quitting(sent)) = order;
        if self.at().is_some_and(|at| sent >= at) {
            return self;
        }
        match (self, order) {
            (Self::Attempt(_), Order::Connected(_)) => Self::Connected,
            (Self::Connected, Order::Reconnecting(sent)) => Self::Attempt(sent + attempt),
            (Self::Deadline(at), Order::Quitting(asked)) => Self::Quitting(at.min(asked + grace)),
            (Self::Attempt(_) | Self::Connected, Order::Quitting(asked)) => {
                Self::Quitting(asked + grace)
            }
            (watch, _) => watch,
        }
    }
}

/// How the shell tells the backstop what happened. Dropping every handle disarms nothing.
#[derive(Clone, Debug)]
pub(crate) struct Backstop(Sender<Order>);

impl Backstop {
    /// Starts the thread that runs `expire` when [`Watch`] says, with `grace` for a quit and
    /// `attempt` for each wait for a connection.
    pub(crate) fn arm(
        limit: Limit,
        grace: Duration,
        attempt: Duration,
        expire: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (orders, heard) = mpsc::channel();
        let watch = Watch::new(limit, attempt);
        thread::Builder::new()
            .name("shell-backstop".to_owned())
            .spawn(move || {
                if follow(watch, &heard, grace, attempt) {
                    expire();
                }
            })?;
        Ok(Self(orders))
    }

    pub(crate) fn tell(&self, order: Order) {
        // Refused only once the thread has fired, and the process is ending then.
        let _fired = self.0.send(order);
    }
}

/// Waits out the watch: true once it must fire, false when it's disarmed and no handle is left
/// to arm it again.
fn follow(mut watch: Watch, heard: &Receiver<Order>, grace: Duration, attempt: Duration) -> bool {
    loop {
        let order = match watch.at() {
            Some(at) => heard.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => heard.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match order {
            Ok(order) => watch = watch.heard(order, grace, attempt),
            Err(RecvTimeoutError::Timeout) => return true,
            // Losing every handle is not a disarm: what is armed still fires.
            Err(RecvTimeoutError::Disconnected) => {
                let Some(at) = watch.at() else { return false };
                thread::sleep(at.saturating_duration_since(Instant::now()));
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE_T: Duration = Duration::from_secs(15);
    const ATTEMPT_T: Duration = Duration::from_secs(30);
    const TICK: Duration = Duration::from_nanos(1);

    fn heard(watch: Watch, orders: &[Order]) -> Watch {
        orders.iter().fold(watch, |watch, order| {
            watch.heard(*order, GRACE_T, ATTEMPT_T)
        })
    }

    fn before(at: Instant) -> Instant {
        at.checked_sub(TICK)
            .expect("the test's instants are past the clock's start")
    }

    fn start(t0: Instant) -> Watch {
        Watch::new(Limit::Start(t0), ATTEMPT_T)
    }

    #[test]
    fn the_limit_at_start_is_the_attempt_or_the_deadline_plus_the_grace() {
        let t0 = Instant::now();
        assert_eq!(
            Watch::new(Limit::at_start(t0, None), START_LIMIT).at(),
            Some(t0 + Duration::from_secs(30)),
            "no flag: 30 s from the lock"
        );
        assert_eq!(
            Watch::new(
                Limit::at_start(t0, Some(Duration::from_secs(2))),
                START_LIMIT
            )
            .at(),
            Some(t0 + Duration::from_secs(17)),
            "a 2 s flag: its deadline plus the 15 s grace"
        );
    }

    #[test]
    fn with_no_flag_the_start_limit_is_armed() {
        let t0 = Instant::now();
        assert_eq!(start(t0).at(), Some(t0 + ATTEMPT_T));
    }

    #[test]
    fn only_a_connection_before_the_limit_disarms_it() {
        let t0 = Instant::now();
        let limit = t0 + ATTEMPT_T;
        assert_eq!(
            heard(start(t0), &[Order::Connected(before(limit))]),
            Watch::Connected
        );
        assert_eq!(
            heard(start(t0), &[Order::Connected(limit)]),
            Watch::Attempt(limit),
            "exactly at the limit it fires"
        );
        assert_eq!(
            heard(start(t0), &[Order::Connected(limit + TICK)]),
            Watch::Attempt(limit),
            "past the limit it fires"
        );
    }

    #[test]
    fn a_reconnect_re_arms_the_attempt_limit_until_the_next_connection() {
        let t0 = Instant::now();
        let lost = t0 + Duration::from_secs(100);
        let again = lost + ATTEMPT_T;
        let reconnecting = [Order::Connected(t0), Order::Reconnecting(lost)];
        assert_eq!(heard(start(t0), &reconnecting), Watch::Attempt(again));
        let reconnected = [
            reconnecting[0],
            reconnecting[1],
            Order::Connected(before(again)),
        ];
        assert_eq!(heard(start(t0), &reconnected), Watch::Connected);
        let late = [reconnecting[0], reconnecting[1], Order::Connected(again)];
        assert_eq!(heard(start(t0), &late), Watch::Attempt(again));
    }

    #[test]
    fn a_connection_or_a_reconnect_never_moves_the_flags_deadline() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(60);
        let flag = Watch::new(Limit::Deadline(deadline), ATTEMPT_T);
        assert_eq!(
            heard(
                flag,
                &[Order::Connected(t0), Order::Reconnecting(t0 + TICK)]
            ),
            Watch::Deadline(deadline)
        );
    }

    #[test]
    fn with_a_flag_a_quit_fires_at_the_sooner_of_its_grace_and_the_deadline() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(60);
        let flag = Watch::new(Limit::Deadline(deadline), ATTEMPT_T);
        let early = t0 + Duration::from_secs(10);
        assert_eq!(
            heard(flag, &[Order::Quitting(early)]),
            Watch::Quitting(early + GRACE_T),
            "the grace from the ask"
        );
        let late = before(deadline);
        assert_eq!(
            heard(flag, &[Order::Quitting(late)]),
            Watch::Quitting(deadline),
            "never later than the deadline"
        );
        assert_eq!(
            heard(flag, &[Order::Quitting(deadline)]),
            Watch::Deadline(deadline),
            "an ask at the deadline is too late to change it"
        );
    }

    #[test]
    fn with_no_flag_a_quit_replaces_the_limit_with_its_grace() {
        let t0 = Instant::now();
        let asked = t0 + Duration::from_secs(29);
        assert_eq!(
            heard(start(t0), &[Order::Quitting(asked)]),
            Watch::Quitting(asked + GRACE_T),
            "a quit at second 29 gets its whole grace"
        );
        let open = t0 + Duration::from_secs(40);
        assert_eq!(
            heard(start(t0), &[Order::Connected(t0), Order::Quitting(open)]),
            Watch::Quitting(open + GRACE_T),
            "a quit after the connection opened arms the grace"
        );
        assert_eq!(
            heard(start(t0), &[Order::Quitting(t0 + ATTEMPT_T)]),
            Watch::Attempt(t0 + ATTEMPT_T),
            "an ask at the limit is too late to change it"
        );
    }

    #[test]
    fn nothing_disturbs_a_quit_in_progress() {
        let t0 = Instant::now();
        let asked = t0 + Duration::from_secs(5);
        let quitting = Watch::Quitting(asked + GRACE_T);
        assert_eq!(heard(start(t0), &[Order::Quitting(asked)]), quitting);
        assert_eq!(
            heard(
                start(t0),
                &[
                    Order::Quitting(asked),
                    Order::Connected(asked + TICK),
                    Order::Reconnecting(asked + 2 * TICK),
                    Order::Quitting(asked + 3 * TICK),
                ]
            ),
            quitting
        );
    }

    fn armed(limit: Limit) -> (Backstop, mpsc::Receiver<Instant>) {
        let (fired, heard) = mpsc::channel();
        let backstop = Backstop::arm(limit, GRACE_T, ATTEMPT_T, move || {
            fired.send(Instant::now()).expect("the test is listening");
        })
        .expect("the backstop starts");
        (backstop, heard)
    }

    #[test]
    fn the_backstop_runs_its_action_once_its_delay_has_passed() {
        let at = Instant::now() + Duration::from_millis(50);
        let (_backstop, heard) = armed(Limit::Deadline(at));
        let ran = heard
            .recv_timeout(Duration::from_secs(30))
            .expect("the deadline fires");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }

    #[test]
    fn losing_every_handle_disarms_nothing() {
        let at = Instant::now() + Duration::from_millis(100);
        let (backstop, heard) = armed(Limit::Deadline(at));
        drop(backstop);
        let ran = heard
            .recv_timeout(Duration::from_secs(30))
            .expect("the deadline still fires");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }
}
