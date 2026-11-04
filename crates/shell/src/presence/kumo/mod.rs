//! Kumo's body, shaded live on the CPU into straight-alpha images that GPUI draws. Plain Rust:
//! no GPUI, no platform code. How it works: docs/development/kumo.md.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the dock draws Kumo from OSC-179's B2; until then only the tests call it"
    )
)]

mod coverage;
mod expression;
mod frame;
mod look;
mod math;
mod neck;
mod raster;
mod shape;
