use super::{Prim, Vec2};

pub(super) fn connected_stroke(primitives: &[Prim]) -> Option<Vec<Vec2>> {
    let mut pieces = primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Prim::Line {
                points,
                closed: false,
                color,
                width,
                ..
            } if *width > 0.0 && color.opacity > 0.0 && points.len() >= 2 => Some(points.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut path = pieces.pop()?.to_vec();
    while !pieces.is_empty() {
        let (index, before, reverse) = pieces.iter().enumerate().find_map(|(index, piece)| {
            let (start, end) = (piece.first(), piece.last());
            if start == path.last() {
                Some((index, false, false))
            } else if end == path.first() {
                Some((index, true, false))
            } else if end == path.last() {
                Some((index, false, true))
            } else if start == path.first() {
                Some((index, true, true))
            } else {
                None
            }
        })?;
        let piece = pieces.swap_remove(index);
        match (before, reverse) {
            (true, false) => {
                path.splice(..0, piece[..piece.len() - 1].iter().copied());
            }
            (true, true) => {
                path.splice(..0, piece[1..].iter().rev().copied());
            }
            (false, false) => path.extend_from_slice(&piece[1..]),
            (false, true) => path.extend(piece[..piece.len() - 1].iter().rev().copied()),
        }
    }
    Some(path)
}
