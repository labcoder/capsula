//! Paint-phase records: the stamp each dock and panel frame leaves, and the status panel's
//! Stop button as the gate harness's click target.

use gpui_kit::accesskit::{Node, Role};
use gpui_kit::{
    A11ySubtreeBuilder, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Style, Window,
};

use super::diagnostics;
use super::observer::{self, Drawn, Surface};

/// A zero-size, out-of-flow element whose paint stamps its surface's frame for `--gate-observer`,
/// naming the mask state it draws from, without a closure of ours. A dock frame is also counted
/// for the settled-interval paint check.
#[derive(Clone, Copy, Debug)]
pub(super) struct Stamp(pub(super) Surface, pub(super) Drawn);

impl IntoElement for Stamp {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Stamp {
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
        diagnostics::painted(cx);
        observer::painted(cx, self.0, self.1);
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

/// The stamp a surface's frame leaves.
#[expect(
    clippy::unnecessary_wraps,
    reason = "a build without the recorder has no stamp"
)]
pub(super) fn stamp(surface: Surface, drawn: Drawn) -> Option<Stamp> {
    Some(Stamp(surface, drawn))
}

/// The Stop button, recorded as the harness's click target.
pub(super) fn target<E>(element: E) -> Target<E> {
    Target(element)
}
