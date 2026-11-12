//! Default entrances and pace for the objects a step puts. An object that names no entrance
//! draws in if it's a stroke and fades in otherwise. Unless the step times its own objects,
//! each starts part-way through the previous one's entrance, and a step that would run past
//! [`STEP_MS`] is compressed to fit. Pure rules over the step's objects in put order; activation
//! turns them into each object's birth timing.

use crate::record::Enter;

/// How far through the previous object's entrance the next one starts: 60%.
pub(crate) const STAGGER: (u64, u64) = (3, 5);

/// The longest a step's default pace may run, in ms.
pub(crate) const STEP_MS: u64 = 4_000;

/// An authored length shrinks the defaults to a quarter at most, so they still visibly enter.
pub(crate) const SQUEEZE_FLOOR: (u64, u64) = (1, 4);

/// One object a step puts, as authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Authored {
    /// Its `enter`, if it names one; an explicit `"none"` is `Some(Enter::None)`.
    pub(crate) enter: Option<Enter>,
    /// Its `dur`, if it names one.
    pub(crate) dur: Option<u32>,
    /// Whether it names a `delay` or an `at_word`.
    pub(crate) timed: bool,
    /// Whether it's a line, arrow, path, edge or series, which draws in by default.
    pub(crate) stroke: bool,
    /// Its kind's entrance length, for an entrance that names no `dur`.
    pub(crate) default_dur: u32,
}

/// How one object enters: how long after its authored origin it starts, with which entrance,
/// for how many ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Paced {
    pub(crate) offset: u64,
    pub(crate) enter: Enter,
    pub(crate) dur: u32,
}

/// Paces a step's objects, in put order. An authored entrance and `dur` are kept exactly, and
/// an explicit `"none"` appears at once. If no object names a `delay` or an `at_word`, each
/// object starts [`STAGGER`] of the way through the previous one's entrance; when the last
/// entrance would end after [`STEP_MS`], every gap and default length shrinks by one ratio
/// until the step fits, or as far as its authored lengths allow; an authored length that
/// would shrink them below [`SQUEEZE_FLOOR`] ends the step late instead.
pub(crate) fn pace(objects: &[Authored]) -> Vec<Paced> {
    let timed = objects.iter().any(|object| object.timed);
    let mut start = 0_u64;
    let mut staged = Vec::with_capacity(objects.len());
    for object in objects {
        let enter = object.enter.unwrap_or(if object.stroke {
            Enter::Draw
        } else {
            Enter::Fade
        });
        let (authored, default) = match (enter, object.dur) {
            (Enter::None, _) => (0, 0),
            (_, Some(dur)) => (dur, 0),
            (_, None) => (0, object.default_dur),
        };
        staged.push((start, enter, authored, default));
        if !timed {
            let length = u64::from(authored) + u64::from(default);
            start = start.saturating_add(length * STAGGER.0 / STAGGER.1);
        }
    }
    let (mut room, mut span) = (1_u64, 1_u64);
    for &(start, _, authored, default) in &staged {
        let scaled = start + u64::from(default);
        let Some(left) = STEP_MS.checked_sub(u64::from(authored)) else {
            continue;
        };
        if !timed
            && scaled > 0
            && (authored == 0 || left * SQUEEZE_FLOOR.1 >= scaled * SQUEEZE_FLOOR.0)
            && u128::from(left) * u128::from(span) < u128::from(room) * u128::from(scaled)
        {
            (room, span) = (left, scaled);
        }
    }
    let scale = |value: u64| {
        u64::try_from(u128::from(value) * u128::from(room) / u128::from(span))
            .expect("a ratio of at most one never lengthens")
    };
    staged
        .into_iter()
        .map(|(start, enter, authored, default)| Paced {
            offset: scale(start),
            enter,
            dur: authored
                + u32::try_from(scale(u64::from(default)))
                    .expect("a ratio of at most one never lengthens"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Authored, Paced, pace};
    use crate::record::Enter;

    fn stroke(default_dur: u32) -> Authored {
        Authored {
            stroke: true,
            ..other(default_dur)
        }
    }
    fn other(default_dur: u32) -> Authored {
        Authored {
            enter: None,
            dur: None,
            timed: false,
            stroke: false,
            default_dur,
        }
    }
    fn paced(offset: u64, enter: Enter, dur: u32) -> Paced {
        Paced { offset, enter, dur }
    }

    #[test]
    fn strokes_draw_in_and_the_rest_fade_in_each_starting_60_percent_through_the_last() {
        assert_eq!(
            pace(&[stroke(600), other(350), other(250)]),
            [
                paced(0, Enter::Draw, 600),
                paced(360, Enter::Fade, 350),
                paced(570, Enter::Fade, 250),
            ]
        );
    }

    #[test]
    fn an_authored_entrance_and_length_are_kept_and_none_appears_at_once() {
        let none = Authored {
            enter: Some(Enter::None),
            ..other(350)
        };
        let pop = Authored {
            enter: Some(Enter::Pop),
            dur: Some(900),
            ..stroke(600)
        };
        let grow = Authored {
            enter: Some(Enter::Grow),
            ..stroke(600)
        };
        assert_eq!(
            pace(&[none, pop, other(250), grow]),
            [
                paced(0, Enter::None, 0),
                paced(0, Enter::Pop, 900),
                paced(540, Enter::Fade, 250),
                paced(690, Enter::Grow, 600),
            ]
        );
    }

    #[test]
    fn a_delay_or_word_anywhere_leaves_the_steps_timing_as_written() {
        let timed = Authored {
            timed: true,
            ..other(350)
        };
        let long = Authored {
            dur: Some(5_000),
            ..stroke(800)
        };
        let defaults: Vec<Authored> = std::iter::repeat_n(stroke(600), 12).collect();
        let mut step = vec![stroke(600), timed, long];
        step.extend(defaults);
        let mut expected = vec![
            paced(0, Enter::Draw, 600),
            paced(0, Enter::Fade, 350),
            paced(0, Enter::Draw, 5_000),
        ];
        expected.extend(std::iter::repeat_n(paced(0, Enter::Draw, 600), 12));
        assert_eq!(pace(&step), expected, "no stagger and no compression");
    }

    #[test]
    fn a_long_step_shrinks_its_gaps_and_default_lengths_by_one_ratio_to_end_within_4_s() {
        let step: Vec<Authored> = std::iter::repeat_n(stroke(600), 12).collect();
        let offsets = [
            0, 315, 631, 947, 1263, 1578, 1894, 2210, 2526, 2842, 3157, 3473,
        ];
        let expected: Vec<Paced> = offsets
            .into_iter()
            .map(|offset| paced(offset, Enter::Draw, 526))
            .collect();
        assert_eq!(pace(&step), expected, "4,560 ms scaled by 4,000/4,560");
    }

    #[test]
    fn an_authored_length_keeps_its_time_while_the_defaults_around_it_shrink() {
        let authored = Authored {
            dur: Some(3_600),
            ..stroke(600)
        };
        assert_eq!(
            pace(&[stroke(600), other(350), authored, other(250)]),
            [
                paced(0, Enter::Draw, 421),
                paced(252, Enter::Fade, 245),
                paced(400, Enter::Draw, 3_600),
                paced(1_915, Enter::Fade, 175),
            ],
            "the authored entrance starts at 400 and ends at exactly 4,000"
        );
    }

    #[test]
    fn an_authored_length_past_4_s_ends_late_without_squeezing_the_rest() {
        let long = Authored {
            dur: Some(5_000),
            ..stroke(600)
        };
        assert_eq!(
            pace(&[stroke(600), long, other(350)]),
            [
                paced(0, Enter::Draw, 600),
                paced(360, Enter::Draw, 5_000),
                paced(3_360, Enter::Fade, 350),
            ]
        );
    }

    #[test]
    fn an_authored_length_shrinks_the_defaults_to_a_quarter_at_most_and_past_that_ends_late() {
        let around = |dur| {
            pace(&[
                stroke(600),
                Authored {
                    dur: Some(dur),
                    ..stroke(600)
                },
                other(350),
            ])
        };
        assert_eq!(
            around(3_910),
            [
                paced(0, Enter::Draw, 150),
                paced(90, Enter::Draw, 3_910),
                paced(676, Enter::Fade, 87),
            ],
            "90 ms left for a 360 ms start is exactly a quarter, ending at 4,000"
        );
        for (dur, last) in [
            (3_911, 2_706),
            (3_999, 2_759),
            (4_000, 2_760),
            (4_001, 2_760),
        ] {
            assert_eq!(
                around(dur),
                [
                    paced(0, Enter::Draw, 600),
                    paced(360, Enter::Draw, dur),
                    paced(last, Enter::Fade, 350),
                ],
                "{dur} ms ends late at the default pace"
            );
        }
    }

    #[test]
    fn a_step_with_no_objects_paces_nothing() {
        assert!(pace(&[]).is_empty());
    }
}
