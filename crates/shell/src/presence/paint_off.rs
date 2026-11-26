//! A build without the `gate-recorder` feature leaves no paint records: no frame stamps and
//! no click target.

use gpui_kit::Div;

use super::observer::{Drawn, Surface};

pub(super) fn stamp(_: Surface, _: Drawn) -> Option<Div> {
    None
}

pub(super) fn target<E>(element: E) -> E {
    element
}
