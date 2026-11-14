//! Where a perched Kumo sits on its host, and the no-room check. Every size converts at the
//! host monitor's scale, and the check covers the whole straight settle from the drop, not only
//! where it ends.

use super::caption::WindowTarget;
use crate::platform::host::PhysicalRect;

/// Logical pixels nothing of Kumo's comes nearer an exclusion than, and the card's gap.
const CLEARANCE: i32 = 4;

/// What placement fits, in logical pixels: the input bounds, which hold the painted ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extents {
    /// The body's side.
    pub body: i32,
    /// The task card's width and height.
    pub card: [i32; 2],
}

/// Where the body and the card end up, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub(crate) body: PhysicalRect,
    pub(crate) card: PhysicalRect,
    /// Above the title bar, rather than just below the caption.
    pub(crate) above: bool,
}

/// Centred above the title bar if there's room, else just below the caption inside the frame,
/// with the card right of the body, else left of it. `from` is the body's top-left at the drop;
/// body and card move together in a straight line from there. `None` is a no-room rejection:
/// no choice keeps them inside the work area and at least the clearance from every exclusion
/// along the whole way.
pub(crate) fn place(target: &WindowTarget, from: [i32; 2], extents: Extents) -> Option<Placement> {
    let physical = |value: i32| {
        let scaled = (i64::from(value) * i64::from(target.monitor_dpi) + 95) / 96;
        i32::try_from(scaled).unwrap_or(i32::MAX)
    };
    let body = physical(extents.body);
    let [card_width, card_height] = extents.card.map(physical);
    let clearance = physical(CLEARANCE);
    let caption = target.caption.rect;
    let left = (caption.left + caption.right - body).div_euclid(2);
    [
        (caption.top - body, true),
        (caption.bottom + clearance, false),
    ]
    .into_iter()
    .filter_map(|(top, above)| {
        let body = sized(left, top, body, body);
        (above || target.frame.contains(body)).then_some((body, above))
    })
    .flat_map(|(body, above)| {
        [body.right + clearance, body.left - clearance - card_width].map(|card_left| Placement {
            body,
            card: sized(card_left, body.top, card_width, card_height),
            above,
        })
    })
    .find(|placement| clear(target, *placement, from, clearance))
}

fn sized(left: i32, top: i32, width: i32, height: i32) -> PhysicalRect {
    PhysicalRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Body and card stay in the work area at both ends, so all the way, and never come within
/// `clearance` of an exclusion anywhere along the move.
fn clear(target: &WindowTarget, placement: Placement, from: [i32; 2], clearance: i32) -> bool {
    let delta = [from[0] - placement.body.left, from[1] - placement.body.top];
    [placement.body, placement.card].into_iter().all(|end| {
        let start = end.offset(delta);
        target.work_area.contains(start)
            && target.work_area.contains(end)
            && target.exclusions.iter().all(|exclusion| {
                let kept = PhysicalRect {
                    left: exclusion.left - clearance,
                    top: exclusion.top - clearance,
                    right: exclusion.right + clearance,
                    bottom: exclusion.bottom + clearance,
                };
                !sweeps(start, end, kept)
            })
    })
}

/// Whether a rectangle moving in a straight line from `start` to `end` (the same size)
/// overlaps `other` at any point: the segment its top-left travels against `other` grown by
/// its size, with half-open edges.
fn sweeps(start: PhysicalRect, end: PhysicalRect, other: PhysicalRect) -> bool {
    let axis = |from: i32, to: i32, low: i32, high: i32| {
        let (from, to, low, high) = (
            f64::from(from),
            f64::from(to),
            f64::from(low),
            f64::from(high),
        );
        let delta = to - from;
        if delta == 0.0 {
            return if low < from && from < high {
                (f64::NEG_INFINITY, f64::INFINITY)
            } else {
                (f64::INFINITY, f64::NEG_INFINITY)
            };
        }
        let (a, b) = ((low - from) / delta, (high - from) / delta);
        (a.min(b), a.max(b))
    };
    let width = start.right - start.left;
    let height = start.bottom - start.top;
    let (x_in, x_out) = axis(start.left, end.left, other.left - width, other.right);
    let (y_in, y_out) = axis(start.top, end.top, other.top - height, other.bottom);
    let (enter, exit) = (x_in.max(y_in), x_out.min(y_out));
    enter < exit && enter < 1.0 && exit > 0.0
}

#[cfg(test)]
mod tests {
    use super::{Extents, Placement, place};
    use crate::platform::host::{HostId, Label, PhysicalRect};
    use crate::presence::perch::caption::{Caption, Exclusions, Source, WindowTarget};

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PhysicalRect {
        PhysicalRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// An 800 × 600 host at 100 % whose caption ends at the inferred strip.
    fn target(work_area: PhysicalRect, frame: PhysicalRect, monitor_dpi: u32) -> WindowTarget {
        let mut exclusions = Exclusions::default();
        exclusions.rects[0] = Some((rect(792, 200, 900, 231), Source::Inferred));
        WindowTarget {
            id: HostId {
                window: 1,
                process: 2,
                thread: 3,
            },
            frame,
            work_area,
            monitor_dpi,
            caption: Caption {
                rect: rect(100, 200, 792, 231),
                source: Source::Inferred,
            },
            exclusions,
            label: Label::new(&[]),
        }
    }

    const KUMO: Extents = Extents {
        body: 56,
        card: [200, 120],
    };
    const SCREEN: PhysicalRect = PhysicalRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    const FRAME: PhysicalRect = PhysicalRect {
        left: 100,
        top: 200,
        right: 900,
        bottom: 800,
    };

    #[test]
    fn room_above_centres_the_body_on_the_title_bar() {
        assert_eq!(
            place(&target(SCREEN, FRAME, 96), [418, 144], KUMO),
            Some(Placement {
                body: rect(418, 144, 474, 200),
                card: rect(478, 144, 678, 264),
                above: true,
            })
        );
    }

    #[test]
    fn with_no_room_above_it_sits_just_below_the_caption() {
        let work = rect(0, 200, 1920, 1040);
        assert_eq!(
            place(&target(work, FRAME, 96), [418, 235], KUMO),
            Some(Placement {
                body: rect(418, 235, 474, 291),
                card: rect(478, 235, 678, 355),
                above: false,
            })
        );
    }

    #[test]
    fn with_room_nowhere_the_drop_is_refused() {
        let work = rect(0, 200, 1920, 1040);
        let short = rect(100, 200, 900, 260);
        assert_eq!(place(&target(work, short, 96), [418, 235], KUMO), None);
    }

    #[test]
    fn a_settle_that_crosses_the_buttons_isnt_safe_even_when_its_end_is() {
        assert_eq!(
            place(&target(SCREEN, FRAME, 96), [880, 260], KUMO),
            Some(Placement {
                body: rect(418, 235, 474, 291),
                card: rect(478, 235, 678, 355),
                above: false,
            }),
            "the way above passes within 4 px of the strip"
        );
    }

    #[test]
    fn sizes_convert_at_the_monitor_scale() {
        assert_eq!(
            place(&target(SCREEN, FRAME, 144), [404, 116], KUMO),
            Some(Placement {
                body: rect(404, 116, 488, 200),
                card: rect(98, 116, 398, 296),
                above: true,
            }),
            "84 px body, 6 px clearance: a card on the right comes within it of the strip"
        );
    }
}
