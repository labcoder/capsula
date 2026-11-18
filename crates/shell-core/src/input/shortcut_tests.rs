use super::*;
use crate::input::{Event, Gesture, Key};
use std::time::Duration;

const CTRL: Role = Role::Modifier(Modifier::Control);
const ALT: Role = Role::Modifier(Modifier::Alt);
const NAMED: NamedKey = NamedKey::Letter(b'K');
const K: Role = Role::Named(NAMED);
const NONE: [Option<Effect>; 3] = [None; 3];
const LISTENING: [Option<Effect>; 3] = [Some(Effect::Listening), None, None];
const TAP: [Option<Effect>; 3] = [Some(Effect::Resting), None, None];

fn id(n: u16) -> PhysicalKey {
    PhysicalKey::new(n).unwrap()
}

fn at(t: Instant, ms: u64) -> Instant {
    t + Duration::from_millis(ms)
}

#[test]
fn release_at_299_is_a_tap_but_300_starts_and_ends_the_hold() {
    let t = Instant::now();
    for (ms, released) in [
        (299, TAP),
        (
            300,
            [
                Some(Effect::HoldStarted),
                Some(Effect::HoldEnded),
                Some(Effect::Resting),
            ],
        ),
    ] {
        let make = |key| Event::Key {
            key,
            edge: Edge::Down,
        };
        let mut legacy = Gesture::default();
        assert_eq!(legacy.handle(make(Key::LeftControl), t), NONE);
        assert_eq!(legacy.handle(make(Key::LeftAlt), t), LISTENING);
        assert_eq!(legacy.deadline(), Some(at(t, 300)));
        assert_eq!(
            legacy.handle(
                Event::Key {
                    key: Key::LeftAlt,
                    edge: Edge::Up
                },
                at(t, ms)
            ),
            released
        );

        let mut custom = Shortcut::new(Chord::new(&[Modifier::Control], NAMED).unwrap());
        assert_eq!(custom.key(id(1), CTRL, Edge::Down, t), NONE);
        assert_eq!(custom.key(id(2), K, Edge::Down, t), LISTENING);
        assert_eq!(custom.deadline(), Some(at(t, 300)));
        assert_eq!(custom.key(id(2), K, Edge::Up, at(t, ms)), released);
    }
}

#[test]
fn only_a_fresh_trigger_starts_the_exact_custom_chord() {
    let t = Instant::now();
    let chord = Chord::new(&[Modifier::Control, Modifier::Alt], NAMED).unwrap();
    let mut shortcut = Shortcut::new(chord);
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Down, t), NONE);
    assert_eq!(shortcut.key(id(2), ALT, Edge::Down, t), NONE);
    assert_eq!(shortcut.deadline(), None);
    assert_eq!(shortcut.timer(at(t, 301)), NONE);
    assert_eq!(shortcut.key(id(3), K, Edge::Down, at(t, 301)), LISTENING);
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Up, at(t, 302)), TAP);
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Down, at(t, 303)), NONE);
    assert_eq!(shortcut.key(id(3), K, Edge::Down, at(t, 304)), NONE);
    assert_eq!(shortcut.key(id(3), K, Edge::Up, at(t, 305)), NONE);
    assert_eq!(shortcut.key(id(3), K, Edge::Down, at(t, 306)), LISTENING);
    assert_eq!(
        shortcut.key(id(3), K, Edge::Up, at(t, 307)),
        [Some(Effect::Resting), Some(Effect::OpenInput), None]
    );
}

#[test]
fn other_devices_and_lost_input_do_not_supply_missing_releases() {
    let t = Instant::now();
    let mut shortcut = Shortcut::new(Chord::new(&[Modifier::Control], NAMED).unwrap());
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Down, t), NONE);
    assert_eq!(shortcut.key(id(2), K, Edge::Down, t), LISTENING);
    assert_eq!(shortcut.key(id(3), CTRL, Edge::Down, at(t, 301)), TAP);
    assert_eq!(
        shortcut.key(id(4), Role::Other, Edge::Down, at(t, 302)),
        NONE
    );
    assert_eq!(shortcut.key(id(2), K, Edge::Up, at(t, 303)), NONE);
    assert_eq!(shortcut.key(id(2), K, Edge::Down, at(t, 304)), NONE);
    assert_eq!(shortcut.lose_input(), NONE);
    for key in [1, 2, 3, 4] {
        assert_eq!(
            shortcut.key(id(key), Role::Other, Edge::Up, at(t, 305)),
            NONE
        );
    }
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Down, at(t, 306)), NONE);
    assert_eq!(shortcut.key(id(2), K, Edge::Down, at(t, 307)), NONE);
    shortcut.trusted_neutral();
    assert_eq!(shortcut.key(id(1), CTRL, Edge::Down, at(t, 308)), NONE);
    assert_eq!(shortcut.key(id(2), K, Edge::Down, at(t, 309)), LISTENING);
}

#[test]
fn fixture_trial_confirms_once_only_after_a_completed_test_and_neutral() {
    let t = Instant::now();
    let a = Chord::new(&[Modifier::Control], NAMED).unwrap();
    let b = Chord::new(&[], NamedKey::Function(24)).unwrap();
    let mut trial = Trial::Capture;
    trial.capture(a);
    trial.capture(b);
    trial.preview(id(1), CTRL, Edge::Down, t);
    assert_eq!(trial.confirm(), None);
    trial.trusted_neutral();
    trial.preview(id(1), CTRL, Edge::Down, t);
    trial.preview(id(2), K, Edge::Down, t);
    trial.preview(id(2), K, Edge::Up, at(t, 299));
    assert_eq!(trial.confirm(), None);
    trial.trusted_neutral();
    assert_eq!(trial.confirm(), Some(a));
    trial.capture(b);
    trial.trusted_neutral();
    assert_eq!(trial.confirm(), None);

    let mut trial = Trial::Capture;
    trial.capture(b);
    trial.end();
    trial.trusted_neutral();
    trial.preview(id(3), Role::Named(NamedKey::Function(24)), Edge::Down, t);
    trial.preview(id(3), Role::Named(NamedKey::Function(24)), Edge::Up, t);
    assert_eq!(trial.confirm(), None);
}

#[test]
fn fixture_chord_shape_and_input_diagnostics_are_redacted() {
    assert_eq!(Chord::new(&[], NAMED), None);
    assert_eq!(Chord::new(&[Modifier::Shift], NAMED), None);
    assert_eq!(Chord::new(&[], NamedKey::Function(25)), None);
    assert!(Chord::new(&[], NamedKey::Function(1)).is_some());
    assert_eq!(
        format!("{:?}", Chord::new(&[Modifier::Super], NAMED).unwrap()),
        "Chord(..)"
    );
    let event = Event::Key {
        key: Key::Other(id(973)),
        edge: Edge::Down,
    };
    assert_eq!(format!("{event:?}"), "Key { key: Key(..), edge: Edge(..) }");
    assert_eq!(format!("{:?}", id(973)), "PhysicalKey(..)");
    assert_eq!(format!("{:?}", Gesture::default()), "Gesture(..)");
}
