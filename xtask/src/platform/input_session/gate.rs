//! Pure state for an approved input session: the fixture's Ready press, the owner's feel
//! verdicts, and the lease that opens injection only after Ready and one quiet second.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// How long input must stay quiet after Ready before injection opens.
pub(crate) const QUIET: Duration = Duration::from_secs(1);

/// Where one button message came from, as the fixture's thread saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A hardware mouse, per the input message source, delivered from the input queue.
    Mouse,
    /// `SendInput`, ours or anyone's.
    Injected,
    /// Sent or unattributed messages, keyboard, touch, pen or touchpad.
    Other,
}

/// What the fixture saw of one button message on Ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseSnapshot {
    pub(crate) origin: Origin,
    /// The input state shows the left button down.
    pub(crate) held: bool,
    /// The message's point is inside the button.
    pub(crate) inside: bool,
    /// The button no longer holds capture; only meaningful after a release.
    pub(crate) capture_released: bool,
}

/// The fixture's Ready button: only a physical press and release inside it counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Press {
    down: bool,
}

impl Press {
    /// A button-down on Ready.
    pub(crate) fn down(&mut self, down: MouseSnapshot) {
        self.down = down.origin == Origin::Mouse && down.held && down.inside;
    }

    /// A button-up on Ready, after the button handled it. True means Ready.
    pub(crate) fn up(&mut self, up: MouseSnapshot) -> bool {
        std::mem::take(&mut self.down)
            && up.origin == Origin::Mouse
            && !up.held
            && up.inside
            && up.capture_released
    }
}

/// The owner's verdict on one gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Unrated,
    Good,
    Off,
    Broken,
}

impl Verdict {
    pub(crate) const ALL: [Self; 4] = [Self::Unrated, Self::Good, Self::Off, Self::Broken];

    fn decode(code: usize) -> Result<Self> {
        match Self::ALL.get(code) {
            Some(verdict) => Ok(*verdict),
            None => bail!("unknown feel verdict code {code}"),
        }
    }
}

/// The owner's feel verdicts, frozen when Ready was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Feel {
    pub(crate) hold: Verdict,
    pub(crate) double_tap: Verdict,
}

impl Feel {
    /// Packs the two selection indices, four bits each, as the fixture stores them.
    pub(crate) fn encode(selections: [isize; 2]) -> Result<usize> {
        let mut code = 0;
        for (index, selection) in selections.into_iter().enumerate() {
            let selection = usize::try_from(selection)?;
            Verdict::decode(selection)?;
            code |= selection << (index * 4);
        }
        Ok(code)
    }

    pub(crate) fn decode(code: usize) -> Result<Self> {
        if code >> 8 != 0 {
            bail!("feel code has unknown fields");
        }
        let verdict = |index: usize| Verdict::decode((code >> (index * 4)) & 0xf);
        Ok(Self {
            hold: verdict(0)?,
            double_tap: verdict(1)?,
        })
    }
}

/// Whether injection may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Before Ready: the owner's own input is allowed, injection never is.
    Waiting,
    /// Ready was pressed at the last-input time `input`; input must stay quiet until `since` +
    /// [`QUIET`].
    Quiet {
        since: Instant,
        input: u32,
    },
    Open,
    /// Permanent: nothing reopens it.
    Aborted,
}

/// Whether `GetLastInputInfo`'s tick `last_input` is at least `away` before `now`, across the
/// 49.7-day tick wrap.
pub(crate) fn away(last_input: u32, now: u32, away: Duration) -> bool {
    u128::from(now.wrapping_sub(last_input)) >= away.as_millis()
}

/// One look at the desktop during the quiet second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quiet {
    /// A session lock or a failed guard; unlike ordinary input, this is permanent.
    pub(crate) aborted: bool,
    /// The native guard saw physical input during this Ready attempt.
    pub(crate) interrupted: bool,
    /// `GetLastInputInfo` now.
    pub(crate) last_input: u32,
    /// The fixture is foreground with its EDIT focused, no key or button is held, and nothing
    /// holds capture, a menu or a move loop.
    pub(crate) settled: bool,
}

/// The fixture's session lease: its gate, and which product process it currently trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lease {
    fixture: u32,
    product: Option<u32>,
    gate: Gate,
    /// Armed by the idle opener's own click, so any spoiled quiet second is terminal.
    idle: bool,
}

impl Lease {
    pub(crate) fn new(fixture: u32) -> Self {
        Self {
            fixture,
            product: None,
            gate: Gate::Waiting,
            idle: false,
        }
    }

    pub(crate) fn gate(&self) -> Gate {
        self.gate
    }

    pub(crate) fn injectable(&self) -> bool {
        self.gate == Gate::Open
    }

    /// Whether `pid` is the fixture or the product bound now.
    pub(crate) fn owns(&self, pid: u32) -> bool {
        pid == self.fixture || Some(pid) == self.product
    }

    /// Binds a restarted product, or none; the gate carries over unchanged.
    pub(crate) fn rebind(&mut self, product: Option<u32>) {
        self.product = product;
    }

    /// The fixture reported Ready, armed when the last input was at `input`.
    pub(crate) fn ready(&mut self, input: u32, now: Instant) {
        self.gate = match self.gate {
            Gate::Waiting => Gate::Quiet { since: now, input },
            _ => Gate::Aborted,
        };
    }

    /// The idle opener's click armed the fixture at `input`; unlike Ready, it never rearms.
    pub(crate) fn activated(&mut self, input: u32, now: Instant) {
        self.idle = true;
        self.ready(input, now);
    }

    /// Steps the quiet second; guard failures and post-open input always abort permanently.
    pub(crate) fn observe(&mut self, quiet: Quiet, now: Instant) {
        if quiet.aborted || (self.gate == Gate::Open && quiet.interrupted) {
            self.abort();
            return;
        }
        let Gate::Quiet { since, input } = self.gate else {
            return;
        };
        self.gate = if quiet.interrupted || !quiet.settled || quiet.last_input != input {
            if self.idle {
                Gate::Aborted
            } else {
                Gate::Waiting
            }
        } else if now.duration_since(since) >= QUIET {
            Gate::Open
        } else {
            self.gate
        };
    }

    pub(crate) fn abort(&mut self) {
        self.gate = Gate::Aborted;
    }
}

/// How long the foreground may be empty: activation handed to another process's window completes
/// asynchronously, so the front reads NULL until that window's thread accepts it.
pub(crate) const NULL_GRACE: Duration = Duration::from_millis(100);

/// The guard's view of an empty foreground, and the graces it has allowed since last taken.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NullForeground {
    since: Option<Instant>,
    /// The open grace was already counted when the phase it began in was taken.
    counted: bool,
    graces: u32,
    longest: Duration,
}

impl NullForeground {
    /// Allows pid 0 for at most [`NULL_GRACE`] from its first sample. Only an owned pid ends the
    /// grace; any other pid fails at once, straight after a NULL too.
    pub(crate) fn observe(&mut self, pid: u32, lease: &Lease, now: Instant) -> Result<()> {
        if pid == 0 {
            let seen = now.duration_since(*self.since.get_or_insert(now));
            if seen > NULL_GRACE {
                bail!(
                    "foreground left the owned fixture/product processes (foreground_pid=0 owned=false null_ms={})",
                    seen.as_millis()
                );
            }
            return Ok(());
        }
        if !lease.owns(pid) {
            bail!(
                "foreground left the owned fixture/product processes (foreground_pid={pid} owned=false)"
            );
        }
        if let Some(since) = self.since.take()
            && !std::mem::take(&mut self.counted)
        {
            self.graces += 1;
            self.longest = self.longest.max(now.duration_since(since));
        }
        Ok(())
    }

    /// The graces begun since the last call and the longest of them, then resets both. A grace
    /// still open at `now` counts here, up to `now`, and not again when it ends.
    pub(crate) fn take_used(&mut self, now: Instant) -> (u32, Duration) {
        if let Some(since) = self.since
            && !self.counted
        {
            self.counted = true;
            self.graces += 1;
            self.longest = self.longest.max(now.duration_since(since));
        }
        (
            std::mem::take(&mut self.graces),
            std::mem::take(&mut self.longest),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_fields(report: &str, fields: &[&str]) {
        for field in fields {
            assert!(
                report
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '=')
                    .any(|token| token == *field),
                "missing {field}: {report}"
            );
        }
    }

    #[test]
    fn a_null_foreground_is_allowed_for_exactly_the_grace_and_only_an_owned_pid_ends_it() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let lease = Lease::new(7);
        let mut null = NullForeground::default();
        null.observe(7, &lease, at(0)).unwrap();
        assert_eq!(null.take_used(at(0)), (0, Duration::ZERO));

        null.observe(0, &lease, at(10)).unwrap();
        null.observe(0, &lease, at(110)).unwrap();
        null.observe(7, &lease, at(110)).unwrap();
        null.observe(0, &lease, at(200)).unwrap();
        null.observe(0, &lease, at(290)).unwrap();
        null.observe(7, &lease, at(290)).unwrap();
        assert_eq!(null.take_used(at(290)), (2, Duration::from_millis(100)));
        assert_eq!(null.take_used(at(290)), (0, Duration::ZERO));

        null.observe(0, &lease, at(400)).unwrap();
        let past = null.observe(0, &lease, at(501)).unwrap_err().to_string();
        assert_fields(&past, &["foreground_pid=0", "owned=false", "null_ms=101"]);
    }

    #[test]
    fn a_foreign_pid_fails_at_once_even_straight_after_a_null() {
        let start = Instant::now();
        let lease = Lease::new(7);
        let mut null = NullForeground::default();
        null.observe(0, &lease, start).unwrap();
        let foreign = null
            .observe(42, &lease, start + Duration::from_millis(1))
            .unwrap_err()
            .to_string();
        assert_fields(&foreign, &["foreground_pid=42", "owned=false"]);
        assert_eq!(
            null.take_used(start + Duration::from_millis(1)),
            (1, Duration::from_millis(1)),
            "the foreign pid ended nothing; the open grace counts at the take"
        );
    }

    #[test]
    fn a_grace_open_at_a_take_counts_there_once_and_still_ends_at_its_limit() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let lease = Lease::new(7);
        let mut null = NullForeground::default();
        null.observe(0, &lease, at(0)).unwrap();
        assert_eq!(null.take_used(at(40)), (1, Duration::from_millis(40)));
        null.observe(7, &lease, at(60)).unwrap();
        assert_eq!(
            null.take_used(at(60)),
            (0, Duration::ZERO),
            "the grace isn't counted again when it ends"
        );

        null.observe(0, &lease, at(100)).unwrap();
        assert_eq!(null.take_used(at(150)), (1, Duration::from_millis(50)));
        null.observe(0, &lease, at(200)).unwrap();
        assert!(null.observe(0, &lease, at(201)).is_err());
    }

    const CALM: Quiet = Quiet {
        aborted: false,
        interrupted: false,
        last_input: 7,
        settled: true,
    };

    fn opened() -> (Lease, Instant) {
        let start = Instant::now();
        let mut lease = Lease::new(10);
        lease.ready(7, start);
        lease.observe(CALM, start + QUIET);
        (lease, start)
    }

    #[test]
    fn input_before_ready_is_allowed_but_never_injectable() {
        let start = Instant::now();
        let mut lease = Lease::new(10);
        for (input, settled) in [(1, false), (2, true), (3, false)] {
            let busy = Quiet {
                aborted: false,
                interrupted: true,
                last_input: input,
                settled,
            };
            lease.observe(busy, start + QUIET * input);
            assert_eq!(lease.gate(), Gate::Waiting);
            assert!(!lease.injectable());
        }
    }

    const DOWN: MouseSnapshot = MouseSnapshot {
        origin: Origin::Mouse,
        held: true,
        inside: true,
        capture_released: false,
    };
    const UP: MouseSnapshot = MouseSnapshot {
        origin: Origin::Mouse,
        held: false,
        inside: true,
        capture_released: true,
    };

    fn ready(down: MouseSnapshot, up: MouseSnapshot) -> bool {
        let mut press = Press::default();
        press.down(down);
        press.up(up)
    }

    #[test]
    fn only_a_physical_press_and_release_inside_ready_counts() {
        assert!(ready(DOWN, UP));
        let rejected = [
            (
                "synthetic down",
                MouseSnapshot {
                    origin: Origin::Injected,
                    ..DOWN
                },
                UP,
            ),
            (
                "posted down",
                MouseSnapshot {
                    origin: Origin::Other,
                    ..DOWN
                },
                UP,
            ),
            (
                "down not in the input state",
                MouseSnapshot {
                    held: false,
                    ..DOWN
                },
                UP,
            ),
            (
                "down outside, dragged in",
                MouseSnapshot {
                    inside: false,
                    ..DOWN
                },
                UP,
            ),
            (
                "synthetic up",
                DOWN,
                MouseSnapshot {
                    origin: Origin::Injected,
                    ..UP
                },
            ),
            (
                "posted up",
                DOWN,
                MouseSnapshot {
                    origin: Origin::Other,
                    ..UP
                },
            ),
            (
                "button still held",
                DOWN,
                MouseSnapshot { held: true, ..UP },
            ),
            (
                "released outside",
                DOWN,
                MouseSnapshot {
                    inside: false,
                    ..UP
                },
            ),
            (
                "capture not released",
                DOWN,
                MouseSnapshot {
                    capture_released: false,
                    ..UP
                },
            ),
        ];
        for (case, down, up) in rejected {
            assert!(!ready(down, up), "{case}");
        }
    }

    #[test]
    fn a_release_without_its_press_or_a_second_release_is_not_ready() {
        let mut press = Press::default();
        assert!(!press.up(UP), "BM_CLICK or Enter: no press");
        press.down(DOWN);
        assert!(press.up(UP));
        assert!(!press.up(UP), "one press, one Ready");
    }

    #[test]
    fn injection_opens_exactly_at_one_quiet_second_after_ready() {
        let start = Instant::now();
        let mut lease = Lease::new(10);
        lease.ready(7, start);
        lease.observe(
            CALM,
            (start + QUIET)
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        );
        assert!(!lease.injectable(), "one millisecond short");
        assert_eq!(
            lease.gate(),
            Gate::Quiet {
                since: start,
                input: 7
            }
        );
        lease.observe(CALM, start + QUIET);
        assert!(lease.injectable());
    }

    #[test]
    fn input_or_unsettled_look_requires_another_ready_and_a_full_quiet_second() {
        let start = Instant::now();
        for spoiled in [
            Quiet {
                last_input: 8,
                ..CALM
            },
            Quiet {
                settled: false,
                ..CALM
            },
            Quiet {
                interrupted: true,
                ..CALM
            },
        ] {
            let mut lease = Lease::new(10);
            lease.ready(7, start);
            lease.observe(spoiled, start + Duration::from_millis(500));
            lease.observe(CALM, start + QUIET);
            assert_eq!(lease.gate(), Gate::Waiting, "{spoiled:?}");
            assert!(!lease.injectable());
            lease.ready(7, start + QUIET * 2);
            lease.observe(CALM, start + QUIET * 2 + Duration::from_millis(999));
            assert!(!lease.injectable());
            lease.observe(CALM, start + QUIET * 3);
            assert!(lease.injectable());
        }
    }

    #[test]
    fn an_idle_activation_opens_only_after_its_quiet_second_and_never_rearms() {
        let start = Instant::now();
        let mut lease = Lease::new(10);
        lease.activated(7, start);
        assert_eq!(
            lease.gate(),
            Gate::Quiet {
                since: start,
                input: 7
            }
        );
        lease.observe(CALM, start + Duration::from_millis(999));
        assert!(!lease.injectable(), "one millisecond short");
        lease.observe(CALM, start + QUIET);
        assert!(lease.injectable());
        for spoiled in [
            Quiet {
                last_input: 8,
                ..CALM
            },
            Quiet {
                settled: false,
                ..CALM
            },
            Quiet {
                interrupted: true,
                ..CALM
            },
        ] {
            let mut lease = Lease::new(10);
            lease.activated(7, start);
            lease.observe(spoiled, start + Duration::from_millis(500));
            assert_eq!(lease.gate(), Gate::Aborted, "{spoiled:?}");
            lease.ready(7, start + QUIET);
            lease.activated(7, start + QUIET);
            lease.observe(CALM, start + QUIET * 3);
            assert_eq!(lease.gate(), Gate::Aborted, "{spoiled:?}: no new arm");
        }
    }

    #[test]
    fn a_session_guard_abort_during_quiet_is_permanent() {
        let start = Instant::now();
        let mut lease = Lease::new(10);
        lease.ready(7, start);
        lease.observe(
            Quiet {
                aborted: true,
                ..CALM
            },
            start,
        );
        lease.ready(7, start + QUIET);
        assert_eq!(lease.gate(), Gate::Aborted);
    }

    #[test]
    fn an_open_lease_never_rearms_from_a_quiet_observation() {
        let (mut lease, start) = opened();
        lease.observe(
            Quiet {
                interrupted: true,
                ..CALM
            },
            start + QUIET * 2,
        );
        assert_eq!(lease.gate(), Gate::Aborted);
        lease.ready(7, start + QUIET * 3);
        lease.observe(CALM, start + QUIET * 3);
        assert_eq!(
            lease.gate(),
            Gate::Aborted,
            "post-open input aborts permanently"
        );
    }

    #[test]
    fn an_abort_after_opening_is_permanent() {
        let (mut lease, start) = opened();
        lease.abort();
        lease.observe(CALM, start + QUIET * 2);
        lease.ready(7, start + QUIET * 2);
        assert_eq!(lease.gate(), Gate::Aborted);
        assert!(!lease.injectable());
    }

    #[test]
    fn a_restarted_product_rebinds_without_a_new_ready() {
        let (mut lease, _) = opened();
        lease.rebind(Some(20));
        assert!(lease.owns(10) && lease.owns(20));
        lease.rebind(None);
        assert!(lease.injectable(), "the lease outlives the product");
        assert!(!lease.owns(20));
        lease.rebind(Some(30));
        assert!(lease.injectable());
        assert!(
            lease.owns(30) && !lease.owns(20),
            "the old product is no longer trusted"
        );
        lease.abort();
        lease.rebind(Some(40));
        assert_eq!(
            lease.gate(),
            Gate::Aborted,
            "rebinding never revives an abort"
        );
    }

    #[test]
    fn feel_codes_unpack_per_gesture_and_reject_unknown_values() {
        assert_eq!(
            Feel::decode(0x12).unwrap(),
            Feel {
                hold: Verdict::Off,
                double_tap: Verdict::Good,
            }
        );
        assert_eq!(
            Feel::decode(0).unwrap(),
            Feel {
                hold: Verdict::Unrated,
                double_tap: Verdict::Unrated,
            }
        );
        assert_eq!(Feel::encode([2, 1]).unwrap(), 0x12);
        assert!(Feel::decode(0x4).is_err());
        assert!(Feel::decode(0x100).is_err());
    }

    #[test]
    fn ready_requires_both_explicit_choices_including_explicit_unrated() {
        for missing in [[-1, -1], [-1, 0], [0, -1]] {
            assert!(Feel::encode(missing).is_err(), "{missing:?}");
        }
        assert_eq!(Feel::encode([0, 0]).unwrap(), 0);
        assert!(Feel::encode([0, 4]).is_err());
    }
}
