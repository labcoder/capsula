//! Pure, single-use decisions for the idle opener's one tagged bootstrap click on the fixture's
//! EDIT. They authorize only that click's move and button batch; keyboard input stays behind the
//! lease's quiet second.

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use windows::Win32::Foundation::POINT;

use super::Receipt;
use super::gate::away;
use super::session::FRESH;

/// Whether Rez stayed away across arming the native guard: the last input read before and after
/// registering it is the same tick, at least `away_for` before `now`.
pub(crate) fn away_across_guard(before: u32, after: u32, now: u32, away_for: Duration) -> bool {
    before == after && away(before, now, away_for)
}

/// A window by handle value, and its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) window: usize,
    pub(crate) pid: u32,
}

/// The away admission, read just before the native prepare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Idle {
    pub(crate) last_input: u32,
    pub(crate) now: u32,
    pub(crate) away: Duration,
    /// The input desktop is Default and this thread's own.
    pub(crate) desktop: bool,
    pub(crate) in_time: bool,
}

/// One look at the desktop just before an activation step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Look {
    /// The fixed reason the native guard or current input state disallows the move.
    pub(crate) foreign: Option<&'static str>,
    /// The input desktop is Default and this thread's own.
    pub(crate) desktop: bool,
    pub(crate) in_time: bool,
    /// How long the look took to read.
    pub(crate) age: Duration,
    /// `WindowFromPoint` at the click point.
    pub(crate) at_point: Owner,
    pub(crate) cursor: POINT,
    /// The foreground thread's mouse capture.
    pub(crate) capture: Option<Owner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Unused,
    Prepared,
    Moved,
    Clicking,
    Clicked,
    Confirmed,
    Aborted,
}

/// Prepared, Moved, Clicking, Clicked, Confirmed in that order, or Aborted for good.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Activation {
    root: Owner,
    edit: Owner,
    point: POINT,
    stage: Stage,
}

impl Activation {
    pub(crate) fn new(root: Owner, edit: Owner) -> Self {
        Self {
            root,
            edit,
            point: POINT::default(),
            stage: Stage::Unused,
        }
    }

    /// Admits the one activation, at `point` inside the EDIT, once Rez has been away for
    /// `idle.away`. Its first call uses it up, refused or not.
    pub(crate) fn prepare(&mut self, idle: Idle, point: POINT) -> Result<()> {
        self.point = point;
        let admitted = idle.desktop && idle.in_time && away(idle.last_input, idle.now, idle.away);
        self.advance(Stage::Unused, admitted.then_some(Stage::Prepared))
    }

    /// Authorizes the tagged move to the point.
    pub(crate) fn may_move(&mut self, look: Look) -> Result<()> {
        let refusal = if self.stage == Stage::Prepared {
            self.obstruction(look)
        } else {
            Some("activation_state")
        };
        self.advance(Stage::Prepared, refusal.is_none().then_some(Stage::Moved))
            .with_context(|| {
                format!(
                    "fixture activation move refused: {}",
                    refusal.unwrap_or("activation_state")
                )
            })
    }

    /// Authorizes the one down/up batch, with the pointer exactly on the point.
    pub(crate) fn may_click(&mut self, look: Look) -> Result<()> {
        let ready = self.calm(look) && look.cursor == self.point;
        self.advance(Stage::Moved, ready.then_some(Stage::Clicking))
    }

    /// `SendInput` accepted `accepted` of the batch's two inputs; anything short refuses.
    pub(crate) fn clicked(&mut self, accepted: usize) -> Result<()> {
        self.advance(Stage::Clicking, (accepted == 2).then_some(Stage::Clicked))
    }

    /// The EDIT recorded exactly this tagged click and the fixture's own root is foreground.
    pub(crate) fn confirm(
        &mut self,
        look: Look,
        receipt: Receipt,
        foreground: Owner,
    ) -> Result<()> {
        let received = receipt.count == 1 && receipt.point == self.point;
        let root = foreground == self.root;
        let confirmed = self.calm(look) && look.cursor == self.point && received && root;
        self.advance(Stage::Clicked, confirmed.then_some(Stage::Confirmed))
    }

    /// Whether to keep waiting for the fixture's root to come to the front: all else is calm, the
    /// pointer is on its point, and the front is empty or another window of the fixture's process.
    pub(crate) fn awaits_root(&self, look: Look, foreground: Owner) -> bool {
        self.calm(look)
            && look.cursor == self.point
            && foreground != self.root
            && (foreground.pid == 0 || foreground.pid == self.root.pid)
    }

    /// Whether a half-acknowledged down may be released: only on this desktop, from a fresh look,
    /// toward the still-owned EDIT, with the pointer on its point and no capture but the EDIT's.
    /// Foreign activity and the deadline don't block this cleanup.
    pub(crate) fn may_release(&self, look: Look) -> bool {
        look.desktop
            && look.age <= FRESH
            && look.at_point == self.edit
            && look.cursor == self.point
            && look.capture.is_none_or(|capture| capture == self.edit)
    }

    pub(crate) fn abort(&mut self) {
        self.stage = Stage::Aborted;
    }

    fn calm(&self, look: Look) -> bool {
        self.obstruction(look).is_none()
    }

    fn obstruction(&self, look: Look) -> Option<&'static str> {
        if look.foreign.is_some() {
            look.foreign
        } else if !look.desktop {
            Some("desktop")
        } else if !look.in_time {
            Some("deadline")
        } else if look.age > FRESH {
            Some("stale_look")
        } else if look.at_point != self.edit {
            Some("edit_not_at_point")
        } else if look.capture.is_some() {
            Some("mouse_capture")
        } else {
            None
        }
    }

    fn advance(&mut self, from: Stage, next: Option<Stage>) -> Result<()> {
        self.stage = match next {
            Some(next) if self.stage == from => next,
            _ => Stage::Aborted,
        };
        ensure!(
            self.stage != Stage::Aborted,
            "the fixture activation was refused after {from:?}; no retry"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Owner = Owner {
        window: 0x100,
        pid: 40,
    };
    const EDIT: Owner = Owner {
        window: 0x200,
        pid: 40,
    };
    const FOREIGN: Owner = Owner {
        window: 0x300,
        pid: 77,
    };
    const AT: POINT = POINT { x: 300, y: 260 };
    const AWAY: Idle = Idle {
        last_input: 1_000,
        now: 601_000,
        away: Duration::from_secs(600),
        desktop: true,
        in_time: true,
    };
    const CALM: Look = Look {
        foreign: None,
        desktop: true,
        in_time: true,
        age: Duration::from_millis(1),
        at_point: EDIT,
        cursor: AT,
        capture: None,
    };
    const RECEIVED: Receipt = Receipt {
        count: 1,
        qpc: 9,
        point: AT,
    };

    /// Steps 0 to 3: move, click, its full acknowledgement, confirmation.
    fn step(activation: &mut Activation, stage: usize, look: Look) -> Result<()> {
        match stage {
            0 => activation.may_move(look),
            1 => activation.may_click(look),
            2 => activation.clicked(2),
            _ => activation.confirm(look, RECEIVED, ROOT),
        }
    }

    /// A prepared activation that took every calm step before `stage`.
    fn upto(stage: usize) -> Activation {
        let mut activation = Activation::new(ROOT, EDIT);
        activation.prepare(AWAY, AT).unwrap();
        for earlier in 0..stage {
            step(&mut activation, earlier, CALM).unwrap();
        }
        activation
    }

    fn assert_refused(stage: usize, look: Look, case: &str) {
        let mut activation = upto(stage);
        assert!(step(&mut activation, stage, look).is_err(), "{case}");
        assert_eq!(activation.stage, Stage::Aborted, "{case}");
        for any in 0..4 {
            assert!(
                step(&mut activation, any, CALM).is_err(),
                "{case}: calm step {any} after the refusal"
            );
        }
        assert!(
            activation.prepare(AWAY, AT).is_err(),
            "{case}: prepare again"
        );
    }

    #[test]
    fn one_calm_tagged_click_confirms_the_activation() {
        let activation = upto(4);
        assert_eq!(activation.stage, Stage::Confirmed);
    }

    #[test]
    fn prepare_admits_from_exactly_ten_minutes_away_across_the_tick_wrap() {
        for (last_input, now, stage) in [
            (1_000, 601_000, Stage::Prepared),
            (1_000, 600_999, Stage::Aborted),
            (u32::MAX - 9, 599_990, Stage::Prepared),
            (u32::MAX - 9, 599_989, Stage::Aborted),
        ] {
            let mut activation = Activation::new(ROOT, EDIT);
            let idle = Idle {
                last_input,
                now,
                ..AWAY
            };
            assert_eq!(
                activation.prepare(idle, AT).is_ok(),
                stage == Stage::Prepared,
                "{now}"
            );
            assert_eq!(activation.stage, stage, "{now}");
        }
    }

    #[test]
    fn prepare_refuses_off_the_default_desktop_or_past_the_deadline_for_good() {
        for idle in [
            Idle {
                desktop: false,
                ..AWAY
            },
            Idle {
                in_time: false,
                ..AWAY
            },
        ] {
            let mut activation = Activation::new(ROOT, EDIT);
            assert!(activation.prepare(idle, AT).is_err(), "{idle:?}");
            assert!(activation.may_move(CALM).is_err(), "{idle:?}: then a move");
        }
    }

    #[test]
    fn an_activation_is_used_up_by_its_first_attempt_even_a_refused_one() {
        let mut activation = Activation::new(ROOT, EDIT);
        let early = Idle {
            now: 600_999,
            ..AWAY
        };
        assert!(activation.prepare(early, AT).is_err(), "not away yet");
        assert!(activation.prepare(AWAY, AT).is_err(), "away now, but used");
        assert!(activation.may_move(CALM).is_err(), "nothing to move for");

        let mut confirmed = upto(4);
        assert!(
            confirmed.prepare(AWAY, AT).is_err(),
            "a confirmed one is used"
        );
        assert!(
            confirmed.confirm(CALM, RECEIVED, ROOT).is_err(),
            "confirmed once"
        );
    }

    #[test]
    fn the_confirm_waits_for_the_root_only_over_an_empty_or_own_front_while_calm() {
        let activation = upto(3);
        let null = Owner { window: 0, pid: 0 };
        assert!(activation.awaits_root(CALM, null), "an empty front");
        assert!(activation.awaits_root(CALM, EDIT), "the fixture's own EDIT");
        assert!(!activation.awaits_root(CALM, ROOT), "the root is in front");
        assert!(!activation.awaits_root(CALM, FOREIGN), "a foreign front");
        for (case, look) in [
            (
                "input",
                Look {
                    foreign: Some("held_input"),
                    ..CALM
                },
            ),
            (
                "a moved pointer",
                Look {
                    cursor: POINT { x: 301, y: 260 },
                    ..CALM
                },
            ),
            (
                "a changed desktop",
                Look {
                    desktop: false,
                    ..CALM
                },
            ),
            (
                "the deadline",
                Look {
                    in_time: false,
                    ..CALM
                },
            ),
        ] {
            assert!(!activation.awaits_root(look, null), "{case}");
        }
    }

    #[test]
    fn only_the_exact_edit_under_the_point_may_be_moved_to_or_clicked() {
        for stage in [0, 1] {
            for (case, at_point) in [
                ("the fixture root, same process", ROOT),
                ("a foreign window", FOREIGN),
                (
                    "the EDIT's handle in another process",
                    Owner { pid: 77, ..EDIT },
                ),
            ] {
                assert_refused(stage, Look { at_point, ..CALM }, case);
            }
            for capture in [EDIT, FOREIGN] {
                let look = Look {
                    capture: Some(capture),
                    ..CALM
                };
                assert_refused(stage, look, "a mouse capture");
            }
        }
    }

    #[test]
    fn a_refused_move_names_the_failed_predicate_and_stays_refused() {
        for (reason, look) in [
            (
                "held_input",
                Look {
                    foreign: Some("held_input"),
                    ..CALM
                },
            ),
            (
                "desktop",
                Look {
                    desktop: false,
                    ..CALM
                },
            ),
            (
                "deadline",
                Look {
                    in_time: false,
                    ..CALM
                },
            ),
            (
                "stale_look",
                Look {
                    age: FRESH + Duration::from_micros(1),
                    ..CALM
                },
            ),
            (
                "edit_not_at_point",
                Look {
                    at_point: ROOT,
                    ..CALM
                },
            ),
            (
                "mouse_capture",
                Look {
                    capture: Some(EDIT),
                    ..CALM
                },
            ),
        ] {
            let mut activation = upto(0);
            let error = activation.may_move(look).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("fixture activation move refused: {reason}")
            );
            assert_eq!(activation.stage, Stage::Aborted);
            assert_eq!(
                activation.may_move(CALM).unwrap_err().to_string(),
                "fixture activation move refused: activation_state"
            );
        }
    }

    #[test]
    fn the_click_needs_the_pointer_exactly_on_its_point() {
        for cursor in [POINT { x: 301, y: 260 }, POINT { x: 300, y: 259 }] {
            assert_refused(1, Look { cursor, ..CALM }, "pointer off the point");
        }
        let elsewhere = Look {
            cursor: POINT { x: 5, y: 5 },
            ..CALM
        };
        assert!(
            upto(0).may_move(elsewhere).is_ok(),
            "the move starts anywhere"
        );
    }

    #[test]
    fn a_look_may_be_up_to_twenty_milliseconds_old() {
        for stage in [0, 1, 3] {
            for micros in [19_999, 20_000] {
                let look = Look {
                    age: Duration::from_micros(micros),
                    ..CALM
                };
                assert!(
                    step(&mut upto(stage), stage, look).is_ok(),
                    "{stage} {micros}"
                );
            }
            let stale = Look {
                age: Duration::from_micros(20_001),
                ..CALM
            };
            assert_refused(stage, stale, "one microsecond stale");
        }
    }

    #[test]
    fn foreign_input_a_desktop_change_or_the_deadline_at_any_step_is_terminal() {
        for stage in [0, 1, 3] {
            for look in [
                Look {
                    foreign: Some("held_input"),
                    ..CALM
                },
                Look {
                    desktop: false,
                    ..CALM
                },
                Look {
                    in_time: false,
                    ..CALM
                },
            ] {
                assert_refused(stage, look, &format!("{look:?}"));
            }
        }
    }

    #[test]
    fn confirmation_needs_the_one_tagged_receipt_and_the_exact_root_foreground() {
        for (case, receipt, foreground) in [
            (
                "no receipt",
                Receipt {
                    count: 0,
                    ..RECEIVED
                },
                ROOT,
            ),
            (
                "two receipts",
                Receipt {
                    count: 2,
                    ..RECEIVED
                },
                ROOT,
            ),
            (
                "receipt elsewhere",
                Receipt {
                    point: POINT { x: 301, y: 260 },
                    ..RECEIVED
                },
                ROOT,
            ),
            ("the EDIT foreground", RECEIVED, EDIT),
            ("a foreign foreground", RECEIVED, FOREIGN),
            (
                "the root's handle in another process",
                RECEIVED,
                Owner { pid: 77, ..ROOT },
            ),
        ] {
            let mut activation = upto(3);
            assert!(
                activation.confirm(CALM, receipt, foreground).is_err(),
                "{case}"
            );
            assert!(
                activation.confirm(CALM, RECEIVED, ROOT).is_err(),
                "{case}: then a valid confirmation"
            );
        }
    }

    #[test]
    fn a_partially_acknowledged_click_never_confirms() {
        for accepted in [0, 1, 3] {
            let mut activation = upto(2);
            assert!(activation.clicked(accepted).is_err(), "{accepted}");
            assert!(
                activation.clicked(2).is_err(),
                "{accepted}: then a full batch"
            );
            assert!(
                activation.confirm(CALM, RECEIVED, ROOT).is_err(),
                "{accepted}: then a valid confirmation"
            );
        }
    }

    #[test]
    fn a_half_acknowledged_down_is_released_only_toward_the_owned_edit() {
        let mut activation = upto(2);
        assert!(activation.clicked(1).is_err(), "half acknowledged");
        for (case, look) in [
            ("calm", CALM),
            (
                "the EDIT holds capture",
                Look {
                    capture: Some(EDIT),
                    ..CALM
                },
            ),
            (
                "the owner moved in",
                Look {
                    foreign: Some("held_input"),
                    ..CALM
                },
            ),
            (
                "past the deadline",
                Look {
                    in_time: false,
                    ..CALM
                },
            ),
            (
                "exactly twenty milliseconds old",
                Look {
                    age: Duration::from_millis(20),
                    ..CALM
                },
            ),
        ] {
            assert!(activation.may_release(look), "{case}");
        }
        for (case, look) in [
            (
                "foreign capture",
                Look {
                    capture: Some(FOREIGN),
                    ..CALM
                },
            ),
            (
                "same-process window at the point",
                Look {
                    at_point: ROOT,
                    ..CALM
                },
            ),
            (
                "foreign window at the point",
                Look {
                    at_point: FOREIGN,
                    ..CALM
                },
            ),
            (
                "pointer off the point",
                Look {
                    cursor: POINT { x: 300, y: 261 },
                    ..CALM
                },
            ),
            (
                "another input desktop",
                Look {
                    desktop: false,
                    ..CALM
                },
            ),
            (
                "one microsecond stale",
                Look {
                    age: Duration::from_micros(20_001),
                    ..CALM
                },
            ),
        ] {
            assert!(!activation.may_release(look), "{case}");
        }
    }

    #[test]
    fn the_away_admission_must_survive_arming_the_guard() {
        let ten = Duration::from_secs(600);
        for (before, after, now, admitted) in [
            (1_000, 1_000, 601_000, true),
            (1_000, 1_001, 601_000, false),
            (1_000, 600_999, 601_000, false),
            (1_000, 1_000, 600_999, false),
            (u32::MAX - 9, u32::MAX - 9, 599_990, true),
            (u32::MAX - 9, 3, 599_990, false),
        ] {
            assert_eq!(
                away_across_guard(before, after, now, ten),
                admitted,
                "{before} {after} {now}"
            );
        }
    }
}
