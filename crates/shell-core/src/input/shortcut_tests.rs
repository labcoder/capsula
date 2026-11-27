use super::{Event, Key, Modifier, Reduction, Shortcut};
use crate::input::{Edge, Effect, Gesture, PhysicalKey, TalkHeld};
use kumochi_protocol::message::ShortcutChord;
use std::time::{Duration, Instant};

const CTRL: Key = Key::Modifier(Modifier::LeftControl);
const ALT: Key = Key::Modifier(Modifier::LeftAlt);
const WIN: Key = Key::Modifier(Modifier::RightWin);
const F6: Key = Key::Function(6);
const NO_EFFECTS: [Option<Effect>; 3] = [None; 3];
const LISTENING: [Option<Effect>; 3] = [Some(Effect::Listening), None, None];
const RESTING: [Option<Effect>; 3] = [Some(Effect::Resting), None, None];
const HOLD_END: [Option<Effect>; 3] = [Some(Effect::HoldEnded), Some(Effect::Resting), None];
const HOLD_ON_RELEASE: [Option<Effect>; 3] = [
    Some(Effect::HoldStarted),
    Some(Effect::HoldEnded),
    Some(Effect::Resting),
];

fn id(n: u16) -> PhysicalKey {
    PhysicalKey::new(n).unwrap()
}

fn held(keys: &[u16]) -> TalkHeld<4> {
    let mut slots = [None; 4];
    for (slot, key) in slots.iter_mut().zip(keys) {
        *slot = Some(id(*key));
    }
    TalkHeld(slots)
}

fn key(key: Key, owner: u16, edge: Edge, down: &[u16]) -> Event {
    Event::Key {
        id: id(owner),
        key,
        edge,
        held: held(down),
    }
}

fn at(t: Instant, ms: u64) -> Instant {
    t + Duration::from_millis(ms)
}

fn armed(chord: ShortcutChord, t: Instant) -> Shortcut {
    let mut shortcut = Shortcut::new(chord, 1);
    assert_eq!(shortcut.reduce(1, Event::Neutral, t), Reduction::default());
    shortcut
}

fn ctrl_f6(t: Instant) -> Shortcut {
    let mut shortcut = armed(ShortcutChord::new(1, Some(6)).unwrap(), t);
    assert_eq!(
        shortcut.reduce(1, key(CTRL, 1, Edge::Down, &[1]), t),
        Reduction::default()
    );
    assert_eq!(
        shortcut
            .reduce(1, key(F6, 2, Edge::Down, &[2, 1]), t)
            .effects,
        LISTENING
    );
    shortcut
}

#[test]
fn custom_release_at_299_is_a_tap_but_300_starts_and_ends_the_hold() {
    let t = Instant::now();
    for (ms, effects) in [(299, RESTING), (300, HOLD_ON_RELEASE)] {
        let mut shortcut = ctrl_f6(t);
        assert_eq!(shortcut.deadline(), Some(at(t, 300)));
        assert_eq!(
            shortcut.reduce(1, key(F6, 2, Edge::Up, &[1]), at(t, ms)),
            Reduction {
                effects,
                ..Reduction::default()
            }
        );
    }
}

#[test]
fn only_a_fresh_trigger_starts_the_exact_custom_chord() {
    let t = Instant::now();
    let chord = ShortcutChord::new(1 | 4, Some(6)).unwrap();
    let mut early = armed(chord, t);
    let rejected = early.reduce(1, key(F6, 3, Edge::Down, &[3]), t);
    assert!(rejected.cancelled);
    assert_eq!(rejected.effects, NO_EFFECTS);
    assert_eq!(early.deadline(), None);

    for order in [[(CTRL, 1), (ALT, 2)], [(ALT, 2), (CTRL, 1)]] {
        let mut shortcut = armed(chord, t);
        let mut press = |event, ms| shortcut.reduce(1, event, at(t, ms));
        assert_eq!(
            press(key(order[0].0, order[0].1, Edge::Down, &[order[0].1]), 0).effects,
            NO_EFFECTS
        );
        assert_eq!(
            press(key(order[1].0, order[1].1, Edge::Down, &[1, 2]), 0).effects,
            NO_EFFECTS
        );
        assert_eq!(
            press(key(F6, 3, Edge::Down, &[1, 2, 3]), 0).effects,
            LISTENING
        );
        assert_eq!(press(key(CTRL, 1, Edge::Up, &[2, 3]), 1).effects, RESTING);
        assert_eq!(
            press(key(CTRL, 1, Edge::Down, &[1, 2, 3]), 2).effects,
            NO_EFFECTS
        );
        assert_eq!(
            press(key(F6, 3, Edge::Down, &[1, 2, 3]), 3).repeat,
            Some(id(3))
        );
        assert_eq!(press(key(F6, 3, Edge::Up, &[1, 2]), 4).effects, NO_EFFECTS);
        assert_eq!(
            press(key(F6, 3, Edge::Down, &[1, 2, 3]), 5).effects,
            LISTENING
        );
        assert_eq!(
            press(key(F6, 3, Edge::Up, &[1, 2]), 6).effects,
            [Some(Effect::Resting), Some(Effect::OpenInput), None]
        );
    }
}

#[test]
fn other_devices_and_lost_input_do_not_supply_missing_releases() {
    let t = Instant::now();
    for rejected in [key(F6, 514, Edge::Up, &[1]), Event::Uncertain] {
        let mut shortcut = ctrl_f6(t);
        let due = at(t, 300);
        assert_eq!(
            shortcut
                .reduce(
                    1,
                    Event::Deadline {
                        due,
                        held: held(&[1, 2])
                    },
                    due
                )
                .effects,
            [Some(Effect::HoldStarted), None, None]
        );
        assert_eq!(
            shortcut.reduce(1, rejected, at(t, 301)),
            Reduction {
                effects: HOLD_END,
                cancelled: true,
                repeat: None
            }
        );
        for event in [
            Event::Uncertain,
            key(F6, 2, Edge::Up, &[1]),
            Event::Neutral,
            key(CTRL, 1, Edge::Down, &[1]),
            key(F6, 2, Edge::Down, &[1, 2]),
        ] {
            assert_eq!(shortcut.reduce(1, event, at(t, 302)), Reduction::default());
        }
        assert_eq!(shortcut.deadline(), None);
        assert_eq!(
            shortcut.reduce(2, Event::Neutral, at(t, 303)),
            Reduction::default()
        );
        assert_eq!(
            shortcut
                .reduce(2, key(CTRL, 1, Edge::Down, &[1]), at(t, 304))
                .effects,
            NO_EFFECTS
        );
        assert_eq!(
            shortcut.reduce(1, Event::Uncertain, at(t, 305)),
            Reduction::default()
        );
        assert_eq!(
            shortcut.reduce(1, key(F6, 2, Edge::Down, &[1, 2]), at(t, 305)),
            Reduction::default()
        );
        assert_eq!(
            shortcut
                .reduce(2, key(F6, 2, Edge::Down, &[1, 2]), at(t, 306))
                .effects,
            LISTENING
        );
    }
}

#[test]
fn uncertified_match_or_deadline_cannot_commit() {
    let t = Instant::now();
    let chord = ShortcutChord::new(1 | 128, None).unwrap();
    for evidence in [Event::Uncertain, key(CTRL, 1, Edge::Down, &[1])] {
        let mut shortcut = armed(chord, t);
        assert_eq!(
            shortcut.reduce(1, key(WIN, 2, Edge::Down, &[2]), t).effects,
            NO_EFFECTS
        );
        assert_eq!(
            shortcut.reduce(1, evidence, at(t, 1)),
            Reduction {
                cancelled: true,
                ..Reduction::default()
            }
        );
        assert_eq!(shortcut.deadline(), None);
        assert_eq!(
            shortcut.reduce(1, key(Key::Other, 3, Edge::Down, &[1, 2]), at(t, 350)),
            Reduction::default()
        );
    }
    for proof in [&[1][..], &[1, 1][..], &[1, 514][..]] {
        let mut shortcut = ctrl_f6(t);
        let due = at(t, 300);
        assert_eq!(
            shortcut.reduce(
                1,
                Event::Deadline {
                    due,
                    held: held(proof)
                },
                due
            ),
            Reduction {
                effects: RESTING,
                cancelled: true,
                repeat: None
            }
        );
        assert_eq!(
            shortcut.reduce(
                1,
                Event::Deadline {
                    due,
                    held: held(&[1, 2])
                },
                at(t, 301)
            ),
            Reduction::default()
        );
    }
}

#[test]
fn modifier_only_chord_is_sided_and_four_keys_can_own_a_hold() {
    let t = Instant::now();
    let chord = ShortcutChord::new(1 | 2 | 64 | 128, None).unwrap();
    let mut shortcut = armed(chord, t);
    let left_win = Key::Modifier(Modifier::LeftWin);
    let right_ctrl = Key::Modifier(Modifier::RightControl);
    for (key, owner, down) in [
        (WIN, 4, &[4][..]),
        (CTRL, 1, &[1, 4]),
        (left_win, 3, &[1, 3, 4]),
    ] {
        assert_eq!(
            shortcut.reduce(1, self::key(key, owner, Edge::Down, down), t),
            Reduction::default()
        );
    }
    assert_eq!(
        shortcut
            .reduce(1, key(right_ctrl, 2, Edge::Down, &[1, 2, 3, 4]), t)
            .effects,
        LISTENING
    );
    assert_eq!(
        shortcut
            .reduce(1, key(right_ctrl, 2, Edge::Up, &[1, 3, 4]), at(t, 300))
            .effects,
        HOLD_ON_RELEASE
    );
}

#[test]
fn only_still_held_owners_can_supply_repeat_exemptions() {
    for (owner, proof) in [(515, &[1, 2, 3][..]), (3, &[1, 2][..])] {
        let t = Instant::now();
        let chord = ShortcutChord::new(1 | 4, Some(6)).unwrap();
        let mut shortcut = armed(chord, t);
        for event in [
            key(CTRL, 1, Edge::Down, &[1]),
            key(ALT, 2, Edge::Down, &[1, 2]),
            key(F6, 3, Edge::Down, &[1, 2, 3]),
        ] {
            shortcut.reduce(1, event, t);
        }
        let release = shortcut.reduce(1, key(CTRL, 1, Edge::Up, &[2, 3]), at(t, 300));
        assert_eq!(release.effects, HOLD_ON_RELEASE);
        assert_eq!(release.repeat, None);
        let remaining = shortcut.held().0;
        assert_eq!(remaining.iter().flatten().count(), 2);
        assert!(remaining.contains(&Some(id(2))));
        assert!(remaining.contains(&Some(id(3))));
        assert_eq!(
            shortcut
                .reduce(1, key(F6, 3, Edge::Down, &[2, 3]), at(t, 301))
                .repeat,
            Some(id(3))
        );
        assert_eq!(
            shortcut.reduce(1, key(CTRL, 1, Edge::Down, &[1, 2, 3]), at(t, 302)),
            Reduction::default()
        );
        assert_eq!(
            shortcut.reduce(1, key(F6, owner, Edge::Down, proof), at(t, 303)),
            Reduction {
                cancelled: true,
                ..Reduction::default()
            }
        );
    }
}

#[test]
fn queued_neutral_and_keys_cannot_arm_a_replacement() {
    let t = Instant::now();
    let mut shortcut = Shortcut::new(ShortcutChord::new(1, Some(6)).unwrap(), 8);
    let pair = [
        key(CTRL, 1, Edge::Down, &[1]),
        key(F6, 2, Edge::Down, &[1, 2]),
    ];
    for event in [Event::Neutral, pair[0], pair[1]] {
        assert_eq!(shortcut.reduce(7, event, t), Reduction::default());
    }
    assert_eq!(shortcut.deadline(), None);
    for (event, effects) in [
        (Event::Neutral, NO_EFFECTS),
        (pair[0], NO_EFFECTS),
        (pair[1], LISTENING),
    ] {
        assert_eq!(shortcut.reduce(8, event, at(t, 1)).effects, effects);
    }
}

#[test]
fn custom_tap_interval_is_strict_and_a_stale_deadline_cannot_hold() {
    let t = Instant::now();
    for (ms, open) in [(299, Some(Effect::OpenInput)), (300, None)] {
        let mut shortcut = ctrl_f6(t);
        let old = at(t, 300);
        assert_eq!(
            shortcut
                .reduce(1, key(F6, 2, Edge::Up, &[1]), at(t, 1))
                .effects,
            RESTING
        );
        assert_eq!(
            shortcut
                .reduce(1, key(F6, 2, Edge::Down, &[1, 2]), at(t, 150))
                .effects,
            LISTENING
        );
        let released = at(t, 1 + ms);
        assert_eq!(
            shortcut.reduce(
                1,
                Event::Deadline {
                    due: old,
                    held: held(&[])
                },
                released
            ),
            Reduction::default()
        );
        assert_eq!(
            shortcut
                .reduce(1, key(F6, 2, Edge::Up, &[1]), released)
                .effects,
            [Some(Effect::Resting), open, None]
        );
    }
}

#[test]
fn fixture_chord_shape_and_input_diagnostics_are_redacted() {
    for (mask, function) in [(1, None), (1 | 16, Some(6)), (0, Some(1))] {
        assert_eq!(
            ShortcutChord::new(mask, function),
            Err(kumochi_protocol::message::InvalidShortcutChord)
        );
    }
    let mut gesture = Gesture::default();
    let legacy = crate::input::Event::Key {
        key: crate::input::Key::Other(id(973)),
        edge: Edge::Down,
    };
    assert_eq!(gesture.handle(legacy, Instant::now()), NO_EFFECTS);
    let custom = key(Key::Function(24), 973, Edge::Down, &[973]);
    for debug in [
        format!("{legacy:?}"),
        format!("{gesture:?}"),
        format!("{custom:?}"),
    ] {
        for sentinel in ["973", "Other", "8192", "Function", "24"] {
            assert!(!debug.contains(sentinel), "{debug}");
        }
    }
}

#[test]
fn altgr_apparent_left_ctrl_cancels_when_right_alt_arrives() {
    let t = Instant::now();
    let chord = ShortcutChord::new(1 | 128, None).unwrap();
    for right_alt in [key(Key::Other, 3, Edge::Down, &[1, 2]), Event::Uncertain] {
        let mut shortcut = armed(chord, t);
        assert_eq!(
            shortcut.reduce(1, key(WIN, 2, Edge::Down, &[2]), t).effects,
            NO_EFFECTS
        );
        assert_eq!(
            shortcut
                .reduce(1, key(CTRL, 1, Edge::Down, &[1, 2]), at(t, 1))
                .effects,
            LISTENING
        );
        assert_eq!(
            shortcut.reduce(1, right_alt, at(t, 2)),
            Reduction {
                effects: RESTING,
                cancelled: true,
                repeat: None
            }
        );
        assert_eq!(shortcut.deadline(), None);
        for event in [
            key(CTRL, 1, Edge::Up, &[2]),
            Event::Deadline {
                due: at(t, 301),
                held: held(&[1, 2]),
            },
            key(CTRL, 1, Edge::Down, &[1, 2]),
            key(CTRL, 1, Edge::Up, &[2]),
        ] {
            assert_eq!(shortcut.reduce(1, event, at(t, 301)), Reduction::default());
        }
    }
}

#[test]
fn esc_cancels_a_custom_tap_or_hold_and_its_releases_cannot_finish_it() {
    let t = Instant::now();
    let due = at(t, 300);
    for esc in [key(Key::Other, 9, Edge::Down, &[1, 2]), Event::Uncertain] {
        for (holding, effects) in [(false, RESTING), (true, HOLD_END)] {
            let mut shortcut = ctrl_f6(t);
            if holding {
                assert_eq!(
                    shortcut
                        .reduce(
                            1,
                            Event::Deadline {
                                due,
                                held: held(&[1, 2])
                            },
                            due
                        )
                        .effects,
                    [Some(Effect::HoldStarted), None, None]
                );
            }
            assert_eq!(
                shortcut.reduce(1, esc, at(t, 301)),
                Reduction {
                    effects,
                    cancelled: true,
                    repeat: None
                }
            );
            for event in [
                key(F6, 2, Edge::Up, &[1]),
                key(F6, 2, Edge::Down, &[1, 2]),
                key(F6, 2, Edge::Up, &[1]),
                key(CTRL, 1, Edge::Up, &[]),
            ] {
                assert_eq!(shortcut.reduce(1, event, at(t, 302)), Reduction::default());
            }
            assert_eq!(shortcut.deadline(), None);
        }
    }
}
