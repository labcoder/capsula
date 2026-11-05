//! World-extent bounds for solids down their attachment chains.
//!
//! The bound sums translation lengths, so it ignores cancellation between links and is
//! stricter than the coordinate cube; rotations preserve lengths and need no envelope.

use super::object::{Kind, Object};
use crate::{ErrorCode, Handle, WireId, limits::MAX_COORD};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
struct Placement {
    origin: f64,
    scale: f64,
}

/// Checks every solid in `after`, using the largest `pos3` envelope any charged version holds.
pub(super) fn bounds<'a>(
    after: &BTreeMap<WireId, Object>,
    versions: impl IntoIterator<Item = (&'a WireId, &'a Object)>,
) -> Result<(), (Handle, ErrorCode)> {
    let mut offsets = BTreeMap::<&WireId, f64>::new();
    for (id, object) in versions {
        if let Some(motion) = object.work.motion.get("pos3") {
            let entry = offsets.entry(id).or_default();
            *entry = entry.max(f64::from(motion.max[0]));
        }
    }
    let mut memo = BTreeMap::new();
    for (id, object) in after {
        if object.kind == Kind::Solid {
            placement(id, after, &offsets, &mut memo).map_err(|code| (object.handle, code))?;
        }
    }
    Ok(())
}

fn placement(
    id: &WireId,
    after: &BTreeMap<WireId, Object>,
    offsets: &BTreeMap<&WireId, f64>,
    memo: &mut BTreeMap<WireId, Placement>,
) -> Result<Placement, ErrorCode> {
    if let Some(known) = memo.get(id) {
        return Ok(*known);
    }
    let object = &after[id];
    let solid = object.work.solid.expect("solids carry placement work");
    let parent = match object
        .references
        .iter()
        .find(|reference| Some(*reference) != object.parent.as_ref())
    {
        Some(attach) => placement(attach, after, offsets, memo)?,
        None => Placement {
            origin: 0.0,
            scale: 1.0,
        },
    };
    let offset = solid.pos + offsets.get(id).copied().unwrap_or_default();
    let value = check(parent, offset, solid.scale, solid.reach)?;
    memo.insert(id.clone(), value);
    Ok(value)
}

fn check(parent: Placement, offset: f64, scale: f64, reach: f64) -> Result<Placement, ErrorCode> {
    let origin = parent.origin + parent.scale * offset;
    let scale = parent.scale * scale;
    let extent = origin + scale * reach;
    if !(origin.is_finite() && scale.is_finite() && extent.is_finite()) {
        return Err(ErrorCode::TransformOverflow);
    }
    if extent > f64::from(MAX_COORD) {
        return Err(ErrorCode::OutOfRange);
    }
    Ok(Placement { origin, scale })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Placement = Placement {
        origin: 0.0,
        scale: 1.0,
    };

    #[test]
    fn a_chain_link_refuses_overflow_and_the_coordinate_limit_exactly() {
        let limit = f64::from(MAX_COORD);
        let at = check(ROOT, limit - 2.0, 1.0, 2.0).expect("exactly at the limit");
        assert_eq!((at.origin, at.scale), (limit - 2.0, 1.0));
        assert_eq!(
            check(ROOT, limit.next_up(), 1.0, 0.0).map(|_| ()),
            Err(ErrorCode::OutOfRange)
        );
        let huge = Placement {
            origin: 0.0,
            scale: f64::MAX,
        };
        assert_eq!(
            check(huge, 0.0, 2.0, 0.0).map(|_| ()),
            Err(ErrorCode::TransformOverflow)
        );
        assert_eq!(
            check(ROOT, f64::NAN, 1.0, 0.0).map(|_| ()),
            Err(ErrorCode::TransformOverflow)
        );
        let child = check(
            Placement {
                origin: 100.0,
                scale: 2.0,
            },
            10.0,
            3.0,
            1.0,
        )
        .expect("small chain");
        assert_eq!((child.origin, child.scale), (120.0, 6.0));
    }
}
