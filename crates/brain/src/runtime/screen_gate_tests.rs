use kumochi_protocol::message::{
    ConnectionKind, Effort, ErrorKey, ModelChoice, ModelInfo, ModelName, ModelSlug, ScreenHolds,
    ScreenOff, UserError,
};

use super::claude;
use crate::coordinator::{Availability, PictureRoutes, Ready};

fn slug(text: &str) -> ModelSlug {
    ModelSlug::new(text).expect("slug")
}

fn choice(connection: ConnectionKind) -> ModelChoice {
    ModelChoice {
        model: slug("seen"),
        effort: Effort::High,
        connection,
    }
}

/// A ready route listing `seen` and `blind`, with `images` the models its fact says see.
fn ready(images: &[&str]) -> Availability {
    let info = |text: &str, name: &str| ModelInfo {
        slug: slug(text),
        display_name: ModelName::new(name).expect("name"),
        supported_efforts: None,
    };
    let mut ready = Ready::fixture();
    ready.models = vec![info("seen", "Seen"), info("blind", "Blind")];
    ready.images = images.iter().map(|text| slug(text)).collect();
    Availability::Ready(ready)
}

fn unavailable() -> Availability {
    Availability::Unavailable(
        UserError::new(ErrorKey::ModelUnavailable, Vec::new(), Vec::new()).expect("error"),
    )
}

fn on() -> ScreenHolds {
    ScreenHolds::On {
        model: ModelName::new("Seen").expect("name"),
    }
}

const BOTH: PictureRoutes = PictureRoutes::new(true, true);

#[test]
fn every_build_ships_both_claude_switches_off() {
    assert_eq!(PictureRoutes::SHIPPED, PictureRoutes::new(false, false));
    assert!(!PictureRoutes::SHIPPED.allows(ConnectionKind::AnthropicKey));
    assert!(!PictureRoutes::SHIPPED.allows(ConnectionKind::ClaudeCode));
    assert!(PictureRoutes::SHIPPED.allows(ConnectionKind::ChatGpt));
}

#[test]
fn a_shipped_claude_route_never_shows_a_model_that_reports_pictures() {
    for kind in [ConnectionKind::AnthropicKey, ConnectionKind::ClaudeCode] {
        assert_eq!(
            claude(
                &ready(&["seen"]),
                &choice(kind),
                PictureRoutes::SHIPPED,
                true,
                true
            ),
            ScreenHolds::Off(ScreenOff::ModelCantSee),
        );
    }
}

#[test]
fn each_claude_switch_opens_only_its_own_route() {
    let key = PictureRoutes::new(true, false);
    let code = PictureRoutes::new(false, true);
    let gate = |kind, routes| claude(&ready(&["seen"]), &choice(kind), routes, true, true);
    assert_eq!(gate(ConnectionKind::AnthropicKey, key), on());
    assert_eq!(
        gate(ConnectionKind::ClaudeCode, key),
        ScreenHolds::Off(ScreenOff::ModelCantSee)
    );
    assert_eq!(gate(ConnectionKind::ClaudeCode, code), on());
    assert_eq!(
        gate(ConnectionKind::AnthropicKey, code),
        ScreenHolds::Off(ScreenOff::ModelCantSee)
    );
}

#[test]
fn the_claude_gate_checks_capture_then_the_setting_then_the_route_then_the_model() {
    let kind = ConnectionKind::AnthropicKey;
    let cases = [
        (
            ready(&["seen"]),
            false,
            true,
            ScreenHolds::Off(ScreenOff::Unsupported),
        ),
        (
            unavailable(),
            false,
            false,
            ScreenHolds::Off(ScreenOff::Unsupported),
        ),
        (
            ready(&["seen"]),
            true,
            false,
            ScreenHolds::Off(ScreenOff::TurnedOff),
        ),
        (
            unavailable(),
            true,
            false,
            ScreenHolds::Off(ScreenOff::TurnedOff),
        ),
        (
            unavailable(),
            true,
            true,
            ScreenHolds::Off(ScreenOff::NotReady),
        ),
        (
            Availability::Finding,
            true,
            true,
            ScreenHolds::Off(ScreenOff::NotReady),
        ),
        (
            ready(&[]),
            true,
            true,
            ScreenHolds::Off(ScreenOff::ModelCantSee),
        ),
        (
            ready(&["blind"]),
            true,
            true,
            ScreenHolds::Off(ScreenOff::ModelCantSee),
        ),
        (ready(&["seen"]), true, true, on()),
    ];
    for (index, (availability, capture, enabled, expected)) in cases.into_iter().enumerate() {
        assert_eq!(
            claude(&availability, &choice(kind), BOTH, capture, enabled),
            expected,
            "case {index}",
        );
    }
}

#[test]
fn a_picture_fact_for_an_unlisted_model_shows_nothing() {
    let mut chosen = choice(ConnectionKind::ClaudeCode);
    chosen.model = slug("gone");
    assert_eq!(
        claude(&ready(&["gone"]), &chosen, BOTH, true, true),
        ScreenHolds::Off(ScreenOff::ModelCantSee),
    );
}
