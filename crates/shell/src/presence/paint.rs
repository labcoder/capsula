//! Paints the outline from its row spans: one border quad and one fill quad per device-pixel
//! row. GPUI snaps each quad to device pixels, so the painted pixels are exactly the hit
//! mask's. Nothing here allocates; the edge is hard rather than antialiased, by design.

use gpui_kit::accesskit::{Node, Role};
use gpui_kit::{
    A11ySubtreeBuilder, App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Pixels, Style, Window, fill, point, px, relative, size,
};

use super::observer::{self, Drawn, Surface};
use super::outline::Outline;

/// The dock's body, filling the dock window, whose top-left is `origin` on screen.
#[derive(Clone, Copy, Debug)]
pub(super) struct OutlinePaint {
    pub(super) outline: Outline,
    pub(super) origin: [i32; 2],
    pub(super) fill: Hsla,
    pub(super) border: Hsla,
    /// The mask state `outline` came from, for `--gate-observer`.
    pub(super) drawn: Drawn,
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
        cx: &mut App,
    ) {
        observer::painted(cx, Surface::Dock, self.drawn);
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

/// `--gate-observer` only: a zero-size, out-of-flow element whose paint stamps the hover
/// panel's frame, so the panel's paint is recorded without a closure of ours.
#[derive(Clone, Copy, Debug)]
pub(super) struct PanelStamp;

impl IntoElement for PanelStamp {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PanelStamp {
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
            position: gpui_kit::Position::Absolute,
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
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        _: &mut Window,
        cx: &mut App,
    ) {
        observer::painted(cx, Surface::Panel, Drawn::default());
    }
}

/// The status panel's Stop button, unchanged: every phase goes to the button, and with
/// `--gate-observer` its painted bounds are recorded as the harness's click target.
#[derive(Debug)]
pub(super) struct Target<E>(pub(super) E);

impl<E: Element> IntoElement for Target<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: Element> Element for Target<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.0.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.0.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.0.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.0
            .prepaint(id, inspector_id, bounds, layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0
            .paint(id, inspector_id, bounds, layout, prepaint, window, cx);
        observer::target(window, cx, bounds);
    }

    fn a11y_role(&self) -> Option<Role> {
        self.0.a11y_role()
    }

    fn write_a11y_info(&self, node: &mut Node) {
        self.0.write_a11y_info(node);
    }

    fn a11y_synthetic_children(
        &mut self,
        prepaint: &mut Self::PrepaintState,
        builder: &mut A11ySubtreeBuilder<'_>,
    ) {
        self.0.a11y_synthetic_children(prepaint, builder);
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
