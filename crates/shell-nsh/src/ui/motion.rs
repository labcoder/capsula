//! The app's springs (design spec 1.5). Each is a response and a damping ratio, the two numbers
//! `NativeShell`'s `Spring::new(response_ms).damping_ratio(ratio)` takes. The response is how long
//! one swing would take undamped, the scale the motion is felt at, not when it stops; a ratio of 1
//! doesn't overshoot, and below 1 the motion passes its target and comes back.

use std::f32::consts::TAU;

use nsh_core::element::Spring;

/// Opening a surface: 0.34 s, 0.80.
pub(crate) const OPEN: Spring = spring(0.34, 0.80);
/// Expanding one: 0.40 s, 0.80.
pub(crate) const EXPAND: Spring = spring(0.40, 0.80);
/// The window: 0.44 s, 0.84.
pub(crate) const WINDOW: Spring = spring(0.44, 0.84);
/// Kumo's body: 0.30 s, 0.50.
pub(crate) const KUMO: Spring = spring(0.30, 0.50);

/// `Spring::new(response).damping_ratio(ratio)`, with the response in seconds: the pin's two
/// aren't `const fn`s. A mass of 1, so the stiffness is ω² and the damping 2·ratio·ω.
const fn spring(response: f32, ratio: f32) -> Spring {
    let omega = TAU / response;
    Spring {
        stiffness: omega * omega,
        damping: 2.0 * ratio * omega,
        mass: 1.0,
        velocity: None,
        frame_rate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_spring_is_nativeshells_of_its_response_and_ratio() {
        for (spring, response_ms, ratio) in [
            (OPEN, 340.0, 0.80),
            (EXPAND, 400.0, 0.80),
            (WINDOW, 440.0, 0.84),
            (KUMO, 300.0, 0.50),
        ] {
            let pin = Spring::new(response_ms).damping_ratio(ratio);
            // The pin takes the damping through a square root, so f32 rounding can differ.
            let close = |ours: f32, theirs: f32| (ours - theirs).abs() <= theirs * 1e-5;
            assert!(
                close(spring.stiffness, pin.stiffness) && close(spring.damping, pin.damping),
                "{response_ms} ms: {spring:?} against {pin:?}"
            );
            let rest = Spring {
                stiffness: pin.stiffness,
                damping: pin.damping,
                ..spring
            };
            assert_eq!(rest, pin, "{response_ms} ms");
        }
    }
}
