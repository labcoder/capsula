//! Edge outlines, sampled curves and the narrow visible-link bow pass.
use super::{
    PrepareError,
    primitives::{Build, arrow_head, point_bounds, reference, text_origin},
    state::{LayoutState, Placed},
};
use crate::{
    Handle, Op,
    display::{Item, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    mermaid::Extra,
    record::{Align, Direction, Head, NodeShape},
};
use std::{collections::BTreeMap, sync::Arc};

/// Where a ray from `rect`'s center toward `toward` leaves the outline of the node `id`: its
/// diamond or ellipse inscribed in `rect`, else `rect` itself.
fn port(state: &LayoutState, id: &crate::WireId, rect: Rect, toward: Vec2) -> Vec2 {
    let shape = state
        .objects
        .get(id)
        .and_then(|object| match &*object.source {
            Op::Node(node) => Some(node.shape),
            _ => None,
        });
    let center = rect.center();
    let delta = toward - center;
    let half = rect.size * 0.5;
    let reach = match shape {
        Some(NodeShape::Diamond) => (delta.x / half.x).abs() + (delta.y / half.y).abs(),
        Some(NodeShape::Circle) => (delta.x / half.x).hypot(delta.y / half.y),
        _ => return rect.edge_toward(toward),
    };
    if reach.is_normal() {
        center + delta * reach.recip()
    } else {
        rect.edge_toward(toward)
    }
}

pub(super) fn build(
    source: &Arc<Op>,
    handle: Handle,
    parent: Option<Handle>,
    direction: Direction,
    state: &LayoutState,
    build: &mut Build<'_>,
    cached: Option<&Placed>,
) -> Result<Placed, PrepareError> {
    let Op::Edge(edge) = &**source else {
        unreachable!("edge builder receives an edge");
    };
    let [a, b] = if let Some(old) = cached {
        old.anchors
    } else {
        let from = super::primitives::reference(state, &edge.from, parent)?;
        let to = super::primitives::reference(state, &edge.to, parent)?;
        [
            port(state, &edge.from, from, to.center()),
            port(state, &edge.to, to, from.center()),
        ]
    };
    let points = if let Some(old) = cached {
        let Prim::Line { points, .. } = &old.item.primitives[0] else {
            unreachable!("edge starts with its curve");
        };
        Arc::clone(points)
    } else {
        curve([a, b], direction, 0.0)
    };
    let (head, thick) = match state.extra(handle) {
        Some(Extra::Link { head, thick }) => (*head, *thick),
        _ => (Head::End, false),
    };
    let heads = heads(&points, head);
    let label_middle = points[points.len() / 2];
    let mut bounds = point_bounds(&points)?;
    for head in &heads {
        bounds = bounds.union(point_bounds(head)?);
    }
    let color = build.color(edge.color.as_ref());
    let mut primitives = vec![Prim::Line {
        points,
        closed: false,
        color,
        width: if thick { 3.5 } else { 2.0 },
        dashed: edge.dash,
    }];
    primitives.extend(heads.into_iter().map(|head| Prim::Fill {
        points: head.into(),
        color,
    }));
    if let Some(label) = &edge.label {
        let style = TextStyle {
            size: build.theme.font_size - 1.0,
            bold: false,
        };
        let size = build.measure(label, style, Some(180.0))?;
        let origin = label_middle - size * 0.5;
        let chip = Rect {
            origin: origin - Vec2::new(4.0, 2.0),
            size: size + Vec2::new(8.0, 4.0),
        };
        bounds = bounds.union(chip);
        primitives.push(Prim::Rect {
            rect: chip,
            radius: 4.0,
            fill: Some(build.theme.surface.into()),
            border: None,
        });
        primitives.push(Prim::Text {
            origin: text_origin(Rect { origin, size }, Some(180.0), Align::Center),
            text: label.clone(),
            style,
            color: build.theme.fg.into(),
            max_width: Some(180.0),
            align: Align::Center,
        });
    }
    let mut placed = Placed::new(
        Item {
            handle,
            parent,
            primitives: primitives.into(),
            bounds,
            transform: Affine::default(),
            opacity: edge.opacity.map_or(1.0, crate::Finite::get),
        },
        Arc::clone(source),
        cached,
    );
    placed.anchors = [a, b];
    placed.edge_direction = cached.map_or(direction, |old| old.edge_direction);
    Ok(placed)
}

fn curve([a, b]: [Vec2; 2], direction: Direction, bow: f32) -> Arc<[Vec2]> {
    let middle = (a + b) * 0.5;
    let control = if direction == Direction::Right {
        Vec2::new(middle.x, a.y)
    } else {
        Vec2::new(a.x, middle.y)
    } + if direction == Direction::Right {
        Vec2::new(0.0, 2.0 * bow)
    } else {
        Vec2::new(2.0 * bow, 0.0)
    };
    quadratic([a, b], control)
}

fn quadratic([a, b]: [Vec2; 2], control: Vec2) -> Arc<[Vec2]> {
    let mut points = Vec::new();
    let pieces = u16::try_from(crate::path::PROCEDURAL_CURVE_PIECES)
        .expect("procedural curve count fits u16");
    for n in 0..=pieces {
        let t = f32::from(n) / f32::from(pieces);
        points.push(a * (1.0 - t).powi(2) + control * (2.0 * (1.0 - t) * t) + b * t.powi(2));
    }
    points.into()
}

fn heads(points: &[Vec2], head: Head) -> Vec<[Vec2; 3]> {
    let last = points.len() - 1;
    let mut heads = Vec::new();
    if matches!(head, Head::End | Head::Both) {
        heads.push(arrow_head(points[last - 1], points[last]));
    }
    if matches!(head, Head::Start | Head::Both) {
        heads.push(arrow_head(points[1], points[0]));
    }
    heads
}

pub(super) fn route(state: &mut LayoutState, build: &Build<'_>) -> Result<(), PrepareError> {
    let mut pairs = BTreeMap::<_, Vec<_>>::new();
    for (id, object) in &state.objects {
        if let Op::Edge(edge) = &*object.source
            && object.item.transform == Affine::default()
            && state.objects.contains_key(&edge.from)
            && state.objects.contains_key(&edge.to)
        {
            let key = if edge.from <= edge.to {
                (edge.from.clone(), edge.to.clone())
            } else {
                (edge.to.clone(), edge.from.clone())
            };
            pairs.entry(key).or_default().push(id.clone());
        }
    }
    for ids in pairs.values() {
        let lane_height = ids
            .iter()
            .flat_map(|id| state.objects[id].item.primitives.iter())
            .filter_map(|primitive| match primitive {
                Prim::Rect { rect, .. } => Some(rect.size.y),
                _ => None,
            })
            .fold(0.0_f32, f32::max);
        let parallel = ids.len() > 1
            && ids.iter().any(
                |id| matches!(&*state.objects[id].source, Op::Edge(edge) if edge.label.is_some()),
            );
        for (index, id) in ids.iter().enumerate() {
            build.check_cancel()?;
            let object = &state.objects[id];
            let Op::Edge(edge) = &*object.source else {
                unreachable!("edge pair")
            };
            let direction = object.edge_direction;
            let from = reference(state, &edge.from, object.item.parent)?;
            let to = reference(state, &edge.to, object.item.parent)?;
            let extent = if direction == Direction::Right {
                from.size.y.max(to.size.y)
            } else {
                from.size.x.max(to.size.x)
            };
            let bow = if parallel && edge.from != edge.to {
                let lane = f32::from(u16::try_from(index / 2 + 1).expect("edge cap fits u16"));
                (extent * 0.5 + 12.0) * lane * if index % 2 == 0 { -1.0 } else { 1.0 }
            } else {
                skipped_bow(state, object, direction)?
            };
            let chip_size = object
                .item
                .primitives
                .iter()
                .find_map(|primitive| match primitive {
                    Prim::Rect { rect, .. } => Some(rect.size),
                    _ => None,
                });
            let points = if edge.from == edge.to {
                // shortcut: the node-only fit can leave long loop labels past the host's right edge; include edge bounds or flip lanes when the native painter proves clipping.
                let lane = f32::from(u16::try_from(index).expect("edge cap fits u16"));
                quadratic(
                    [
                        Vec2::new(from.max().x, from.center().y),
                        Vec2::new(from.center().x, from.max().y),
                    ],
                    from.max()
                        + Vec2::new(
                            from.size.x.max(
                                from.size.x * 0.25 + chip_size.map_or(0.0, |size| size.x) + 24.0,
                            ),
                            from.size.y + 24.0 + 2.0 * lane * (lane_height + 12.0),
                        ),
                )
            } else {
                curve(object.anchors, direction, bow)
            };
            let head = match state.extra(object.item.handle) {
                Some(Extra::Link { head, .. }) => *head,
                _ => Head::End,
            };
            let heads = heads(&points, head);
            let mut bounds = point_bounds(&points)?;
            for head in &heads {
                bounds = bounds.union(point_bounds(head)?);
            }
            if let Some(size) = chip_size {
                bounds = bounds.union(Rect::centered(points[points.len() / 2], size));
            }
            let primitives = repaint_curve(&object.item.primitives, &points, heads);
            let object = state.objects.get_mut(id).expect("edge exists");
            if let Some(primitives) = primitives {
                object.item.primitives = primitives;
            }
            object.anchors = [points[0], points[points.len() - 1]];
            object.item.bounds = bounds;
        }
    }
    Ok(())
}

fn repaint_curve(
    primitives: &[Prim],
    points: &Arc<[Vec2]>,
    heads: Vec<[Vec2; 3]>,
) -> Option<Arc<[Prim]>> {
    let Prim::Line {
        points: old,
        closed,
        color,
        width,
        dashed,
    } = &primitives[0]
    else {
        unreachable!("edge starts with its curve");
    };
    let changed = **old != **points
        || !primitives
            .iter()
            .filter_map(|primitive| match primitive {
                Prim::Fill { points, .. } => Some(&**points),
                _ => None,
            })
            .eq(heads.iter().map(<[Vec2; 3]>::as_slice));
    changed.then(|| {
        std::iter::once(Prim::Line {
            points: if **old == **points {
                Arc::clone(old)
            } else {
                Arc::clone(points)
            },
            closed: *closed,
            color: *color,
            width: *width,
            dashed: *dashed,
        })
        .chain(heads.into_iter().map(|head| Prim::Fill {
            points: head.into(),
            color: *color,
        }))
        .chain(
            primitives
                .iter()
                .skip(1)
                .filter(|primitive| !matches!(primitive, Prim::Fill { .. }))
                .cloned(),
        )
        .collect()
    })
}

fn skipped_bow(
    state: &LayoutState,
    object: &Placed,
    direction: Direction,
) -> Result<f32, PrepareError> {
    let Op::Edge(edge) = &*object.source else {
        unreachable!("edge bow")
    };
    let from = &state.objects[&edge.from];
    let to = &state.objects[&edge.to];
    let [a, b] = object.anchors;
    let straight = if direction == Direction::Right {
        (a.y - b.y).abs() <= f32::EPSILON
    } else {
        (a.x - b.x).abs() <= f32::EPSILON
    };
    if edge.label.is_none()
        || !straight
        || from.item.parent != to.item.parent
        || from
            .rank
            .zip(to.rank)
            .is_none_or(|(a, b)| a.abs_diff(b) <= 1)
    {
        return Ok(0.0);
    }
    let span = Rect::centered(
        (a + b) * 0.5,
        Vec2::new((b.x - a.x).abs(), (b.y - a.y).abs()),
    );
    let mut extent = 0.0_f32;
    for (id, node) in &state.objects {
        if id == &edge.from
            || id == &edge.to
            || node.item.parent != from.item.parent
            || !matches!(&*node.source, Op::Node(_))
        {
            continue;
        }
        let bounds = reference(state, id, object.item.parent)?;
        if span.intersects(bounds) {
            extent = extent.max(if direction == Direction::Right {
                bounds.size.y
            } else {
                bounds.size.x
            });
        }
    }
    // shortcut: only straight skipped-rank links and parallel pairs bow; general node/title routing waits until after D1.
    Ok(if extent > 0.0 {
        -(extent * 0.5 + 12.0)
    } else {
        0.0
    })
}

#[cfg(test)]
mod tests {
    use super::{curve, heads, repaint_curve};
    use crate::{
        display::Prim,
        geom::{Rect, Vec2},
        record::{Direction, Head},
        theme::Theme,
    };
    use std::sync::Arc;

    #[test]
    fn repaint_replaces_any_old_head_count_and_preserves_other_geometry() {
        let points = curve(
            [Vec2::new(0.0, 0.0), Vec2::new(16.0, 0.0)],
            Direction::Right,
            0.0,
        );
        let color = Theme::default().line.into();
        let line = Prim::Line {
            points: Arc::clone(&points),
            closed: false,
            color,
            width: 2.0,
            dashed: true,
        };
        let chip = Prim::Rect {
            rect: Rect::centered(Vec2::new(8.0, 0.0), Vec2::new(2.0, 2.0)),
            radius: 1.0,
            fill: Some(color),
            border: None,
        };
        for count in [0, 1, 2, 3] {
            let mut old = vec![line.clone()];
            old.extend((0..count).map(|_| Prim::Fill {
                points: Arc::from([Vec2::new(-100.0, -100.0); 3]),
                color,
            }));
            old.push(chip.clone());
            let new = repaint_curve(&old, &points, heads(&points, Head::Both))
                .expect("repaint wrong heads");
            assert_eq!(new.len(), 4);
            assert_eq!(new[0], line);
            assert_eq!(
                new[1],
                Prim::Fill {
                    points: Arc::from([
                        Vec2::new(16.0, 0.0),
                        Vec2::new(7.0, 4.0),
                        Vec2::new(7.0, -4.0)
                    ]),
                    color
                }
            );
            assert_eq!(
                new[2],
                Prim::Fill {
                    points: Arc::from([
                        Vec2::new(0.0, 0.0),
                        Vec2::new(9.0, -4.0),
                        Vec2::new(9.0, 4.0)
                    ]),
                    color
                }
            );
            assert_eq!(new[3], chip);
            assert_eq!(
                repaint_curve(&new, &points, heads(&points, Head::Both)),
                None
            );
        }
    }
}
