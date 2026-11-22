use crate::presence::geometry::{Layout, Position};
use kumochi_scene::geom::{Rect, Vec2};

pub(in crate::presence) fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
    Rect {
        origin: Vec2::new(x, y),
        size: Vec2::new(width, height),
    }
}

pub(in crate::presence) const AREA: [i32; 4] = [0, 0, 2560, 1400];

pub(in crate::presence) const PEEK_STAGE: Rect = Rect {
    origin: Vec2::new(32.0, 177.0),
    size: Vec2::new(2496.0, 1191.0),
};

pub(in crate::presence) const TYPING_STAGE: Rect = Rect {
    origin: Vec2::new(32.0, 597.0),
    size: Vec2::new(2496.0, 771.0),
};

pub(in crate::presence) fn top_center(area: [i32; 4]) -> Layout {
    Position::TopCenter.layout(area, [58, 21], [380, 124], 0)
}
