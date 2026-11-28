//! The app's own design layer: the theme's tokens and the springs, which views name instead of
//! writing raw colours, lengths or timings.

#[expect(
    dead_code,
    reason = "nothing reads a spring until the first surface lands"
)]
pub(crate) mod motion;
pub(crate) mod theme;
