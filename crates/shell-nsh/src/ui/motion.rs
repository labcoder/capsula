//! The app's springs (design spec 1.5). Each is a response and a damping ratio, the two numbers
//! `NativeShell`'s `Spring::new(response_ms).damping_ratio(ratio)` takes. The response is how long
//! one swing would take undamped, the scale the motion is felt at, not when it stops; a ratio of 1
//! doesn't overshoot, and below 1 the motion passes its target and comes back.

use nsh_core::element::Spring;

/// Opening a surface: 0.34 s, 0.80.
pub(crate) fn open() -> Spring {
    Spring::new(340.0).damping_ratio(0.80)
}

/// Expanding one: 0.40 s, 0.80.
pub(crate) fn expand() -> Spring {
    Spring::new(400.0).damping_ratio(0.80)
}

/// The window: 0.44 s, 0.84.
pub(crate) fn window() -> Spring {
    Spring::new(440.0).damping_ratio(0.84)
}

/// Kumo's body: 0.30 s, 0.50.
pub(crate) fn kumo() -> Spring {
    Spring::new(300.0).damping_ratio(0.50)
}
