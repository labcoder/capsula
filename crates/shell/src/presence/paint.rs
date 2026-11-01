//! Paints the outline from its row spans: one border quad and one fill quad per device-pixel
//! row. GPUI snaps each quad to device pixels, so the painted pixels are exactly the hit
//! mask's. Nothing here allocates; the edge is hard rather than antialiased, by design.

use gpui_kit::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window, fill, point, px, relative, size,
};

use super::outline::Outline;

/// The dock's body, filling the dock window, whose top-left is `origin` on screen.
#[derive(Clone, Copy, Debug)]
pub(super) struct OutlinePaint {
    pub(super) outline: Outline,
    pub(super) origin: [i32; 2],
    pub(super) fill: Hsla,
    pub(super) border: Hsla,
}

impl IntoElement for OutlinePaint {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for OutlinePaint {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: size(relative(1.).into(), relative(1.).into()),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let scale = window.scale_factor();
        let inner = self.outline.inset(f64::from(scale));
        let [origin_x, origin_y] = self.origin;
        let [_, top, _, height] = self.outline.bounds();
        for y in top..top + height {
            for (outline, color) in [(&self.outline, self.border), (&inner, self.fill)] {
                let Some([left, right]) = outline.row_span(y) else {
                    continue;
                };
                let row = Bounds::new(
                    bounds.origin
                        + point(
                            logical(left - origin_x, scale),
                            logical(y - origin_y, scale),
                        ),
                    size(logical(right - left, scale), logical(1, scale)),
                );
                window.paint_quad(fill(row, color));
            }
        }
    }
}

/// Physical pixels as GPUI's logical pixels.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "screen coordinates are far below f32's exact integer range"
)]
fn logical(physical: i32, scale: f32) -> Pixels {
    px(physical as f32 / scale)
}
