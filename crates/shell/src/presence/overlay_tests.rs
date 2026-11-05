use std::sync::Arc;

use gpui_kit::{Bounds, Hsla, Rgba, point, px, rgb, size};
use kumochi_scene::{
    display::Paint,
    geom::{Affine, Rect, Vec2},
    layout::SurfaceGeometry,
    record::Rgb,
    theme::Theme,
};

use super::super::Presence;
use super::{color, label, upright};
use crate::{escape::Panel, presenter::Presenter, runs::Runs};

fn paint(hex: u32, opacity: f32) -> Paint {
    Paint {
        rgb: Rgb::try_from(hex).expect("a packed RGB value"),
        opacity,
    }
}

fn rgba(hex: u32, a: f32) -> Hsla {
    Rgba { a, ..rgb(hex) }.into()
}

const RECT: Rect = Rect {
    origin: Vec2 { x: 1.0, y: 2.0 },
    size: Vec2 { x: 30.0, y: 10.0 },
};

#[test]
fn opacity_multiplies_and_an_override_keeps_the_prim_s_own() {
    assert_eq!(
        color(paint(0x33_66_99, 0.5), None, 0.5),
        rgba(0x33_66_99, 0.25)
    );
    assert_eq!(
        color(paint(0x33_66_99, 0.5), Some(paint(0x00_ff_00, 0.8)), 0.5),
        rgba(0x00_ff_00, 0.2)
    );
    assert_eq!(
        color(paint(0x33_66_99, 1.0), None, 4.0),
        rgba(0x33_66_99, 1.0)
    );
}

#[test]
fn an_upright_whole_rect_is_a_quad_clamped_like_its_outline() {
    let transform = Affine::pose(2.0, 0.0, Vec2::new(10.0, 20.0));
    let ink = rgba(0x33_66_99, 1.0);
    let quad = upright(transform, 1.0, RECT, 8.0, Some(ink), Some((ink, 7.0)))
        .expect("an axis-aligned, fully drawn rect");
    assert_eq!(
        quad.bounds,
        Bounds::new(point(px(12.0), px(24.0)), size(px(60.0), px(20.0)))
    );
    assert_eq!(quad.corner_radii.top_left, px(10.0), "radius 8 clamps to 5");
    assert_eq!(quad.border_widths.top, px(10.0), "width 7 clamps to 5");
    assert_eq!(quad.border_color, ink);
}

#[test]
fn a_turned_mirrored_or_revealing_rect_takes_its_outline_paths() {
    let ink = Some(rgba(0x33_66_99, 1.0));
    for (transform, draw) in [
        (Affine::pose(2.0, 0.01, Vec2::new(10.0, 20.0)), 1.0),
        (Affine([0.0, 2.0, -2.0, 0.0, 100.0, 50.0]), 1.0),
        (Affine([-1.0, 0.0, 0.0, 1.0, 0.0, 0.0]), 1.0),
        (Affine::pose(2.0, 0.0, Vec2::new(10.0, 20.0)), 0.5),
    ] {
        assert!(
            upright(transform, draw, RECT, 8.0, ink, None).is_none(),
            "{transform:?} at {draw}"
        );
    }
}

#[test]
fn a_rotated_label_stays_upright_at_its_turned_anchor_and_scales_uniformly() {
    let quarter_turn = Affine([0.0, 2.0, -2.0, 0.0, 100.0, 50.0]);
    let (anchor, font_size, wrap) = label(quarter_turn, Vec2::new(10.0, 0.0), 12.0, Some(50.0));
    assert_eq!(anchor, point(px(100.0), px(70.0)));
    assert_eq!(font_size, px(24.0));
    assert_eq!(wrap, Some(px(100.0)));
    let (_, _, unwrapped) = label(quarter_turn, Vec2::new(0.0, 0.0), 12.0, None);
    assert_eq!(unwrapped, None);
}

#[test]
fn a_shown_overlay_holds_esc_and_a_hidden_one_does_not() {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let surface = SurfaceGeometry::Overlay {
        logical_size: [1920.0, 1080.0],
    };
    let presenter = Presenter::new(surface, Arc::new(Theme::default()));
    let runs = Runs::default();
    for shown in [false, true, false] {
        presenter.frames().borrow_mut().shown = shown;
        let activity = Presence::activity(&runs, Panel::Closed, &presenter);
        assert_eq!(activity.drawing, shown);
        assert_eq!(activity.holds_global_escape(), shown);
    }
}
