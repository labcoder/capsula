use super::{
    Glide, PrepareError, SurfaceGeometry, diagram,
    primitives::{Build, arrow_head, ellipse, layout_error, point_bounds, rect},
    state::{LayoutState, Placed, Rest},
};
use crate::{
    ErrorCode, Handle, Op,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    record::{Align, Direction, Node, NodeShape},
};
use std::{collections::BTreeMap, sync::Arc};

#[expect(
    clippy::too_many_lines,
    reason = "initial batch and stable incremental placement share the same measured node index"
)]
pub(super) fn place(
    sources: &BTreeMap<crate::WireId, (Handle, Arc<Op>)>,
    order: &[crate::WireId],
    state: &mut LayoutState,
    build: &mut Build<'_>,
    surface: SurfaceGeometry,
) -> Result<Vec<Glide>, PrepareError> {
    let mut measured = BTreeMap::new();
    for id in order {
        let Some((_, source)) = sources.get(id) else {
            continue;
        };
        let Op::Node(node) = &**source else {
            continue;
        };
        let style = TextStyle {
            size: build.theme.font_size,
            bold: true,
        };
        let label = build.measure(&node.label, style, Some(180.0))?;
        let sub = node
            .sub
            .as_ref()
            .map(|text| {
                build.measure(
                    text,
                    TextStyle {
                        size: build.theme.font_size - 2.0,
                        bold: false,
                    },
                    Some(180.0),
                )
            })
            .transpose()?;
        let mut size = Vec2::new(
            label.x.max(sub.map_or(0.0, |size| size.x)) + 24.0,
            (label.y + sub.map_or(0.0, |size| size.y + 4.0) + 20.0).max(40.0),
        );
        size.x = size.x.max(80.0);
        if node.shape == NodeShape::Circle {
            let diameter = size.x.max(size.y);
            size = Vec2::new(diameter, diameter);
        }
        measured.insert(id.clone(), (size, label, sub));
    }
    let mut groups = BTreeMap::<Option<Handle>, Vec<crate::WireId>>::new();
    for id in order {
        let Some((_, source)) = sources.get(id) else {
            continue;
        };
        let Op::Node(node) = &**source else {
            continue;
        };
        let parent = if let Some(old) = state.objects.get(id) {
            old.item.parent
        } else {
            node.parent
                .as_ref()
                .map(|parent| {
                    state
                        .objects
                        .get(parent)
                        .map(|object| object.item.handle)
                        .ok_or_else(|| layout_error(ErrorCode::UnknownRef))
                })
                .transpose()?
        };
        groups.entry(parent).or_default().push(id.clone());
    }
    let mut glides = Vec::new();
    for (parent, mut new_ids) in groups {
        let group_id = parent.and_then(|handle| state.handles.get(&handle).cloned());
        let group = group_id.as_ref().and_then(|id| state.objects.get(id));
        let viewport = group
            .and_then(|group| match &*group.source {
                Op::Diagram(diagram) => diagram.bounds.map(rect),
                _ => None,
            })
            .unwrap_or(build.viewport);
        let direction = group
            .and_then(|group| match &*group.source {
                Op::Diagram(diagram) => diagram.dir,
                _ => None,
            })
            .unwrap_or(match surface {
                SurfaceGeometry::Inline { .. } => Direction::Down,
                SurfaceGeometry::Overlay { .. } => Direction::Right,
            });
        let initialized = group.is_some_and(|group| group.initialized);
        let mut all: Vec<_> = state
            .objects
            .iter()
            .filter(|(_, object)| {
                matches!(&*object.source, Op::Node(_)) && object.item.parent == parent
            })
            .map(|(id, object)| (object.item.handle, id.clone(), object.item.bounds.size))
            .collect();
        for id in &new_ids {
            let (handle, _) = &sources[id];
            if let Some(existing) = all.iter_mut().find(|(_, existing, _)| existing == id) {
                existing.2 = measured[id].0;
            } else {
                all.push((*handle, id.clone(), measured[id].0));
            }
        }
        all.sort_by_key(|(handle, _, _)| *handle);
        let sizes: Vec<_> = all.iter().map(|(_, _, size)| *size).collect();
        let index: BTreeMap<_, _> = all
            .iter()
            .enumerate()
            .map(|(index, (_, id, _))| (id.clone(), index))
            .collect();
        let mut edges: Vec<_> = state
            .objects
            .values()
            .filter_map(|object| match &*object.source {
                Op::Edge(edge) => Some((object.item.handle, edge)),
                _ => None,
            })
            .collect();
        edges.extend(
            sources
                .values()
                .filter_map(|(handle, source)| match &**source {
                    Op::Edge(edge) => Some((*handle, edge)),
                    _ => None,
                }),
        );
        edges.sort_by_key(|(handle, _)| *handle);
        edges.dedup_by_key(|(handle, _)| *handle);
        let edges: Vec<_> = edges
            .into_iter()
            .filter_map(|(_, edge)| Some((*index.get(&edge.from)?, *index.get(&edge.to)?)))
            .collect();
        let placement = diagram::layout(&sizes, &edges, direction, diagram::RANK_GAP)?;
        let mut ranks = placement.ranks.clone();
        for (slot, (_, id, _)) in all.iter().enumerate() {
            if let Some(rank) = state.objects.get(id).and_then(|object| object.rank) {
                ranks[slot] = rank;
            }
        }
        for _ in 0..all.len() {
            for &(from, to) in &edges {
                if placement.ranks[from] < placement.ranks[to]
                    && !state.objects.contains_key(&all[to].1)
                {
                    ranks[to] = ranks[to].max(ranks[from] + 1);
                }
            }
        }
        let initial = !initialized && all.iter().all(|(_, id, _)| new_ids.contains(id));
        let translate = Vec2::new(
            viewport.center().x - placement.bounds.center().x,
            viewport.origin.y + 16.0 - placement.bounds.origin.y,
        );
        let mut focus = None;
        new_ids.sort_by_key(|id| ranks[index[id]]);
        for id in new_ids {
            let (handle, source) = &sources[&id];
            let Op::Node(node) = &**source else {
                unreachable!("node group contains nodes");
            };
            let (size, label, sub) = measured[&id];
            let slot = index[&id];
            let rank = ranks[slot];
            let previous = state.objects.get(&id).cloned();
            let bounds = if let Some(previous) = &previous {
                Rect::centered(previous.item.bounds.center(), size)
            } else if initial {
                Rect {
                    origin: placement.rects[slot].origin + translate,
                    size,
                }
            } else {
                free_slot(
                    state,
                    parent,
                    rank,
                    size,
                    direction,
                    viewport,
                    placement.rects[slot].center() + translate,
                )?
            };
            let primitives = node_primitives(node, bounds, label, sub, build);
            let object = Placed {
                item: Item {
                    handle: *handle,
                    parent,
                    primitives: primitives.into(),
                    bounds,
                    transform: previous
                        .as_ref()
                        .map_or_else(Affine::default, |object| object.item.transform),
                    opacity: node.opacity.map_or(1.0, crate::Finite::get),
                },
                source: Arc::clone(source),
                anchors: [Vec2::default(); 2],
                rest: previous
                    .as_ref()
                    .map_or_else(Rest::default, |object| object.rest),
                rank: Some(
                    previous
                        .as_ref()
                        .and_then(|object| object.rank)
                        .unwrap_or(rank),
                ),
                initialized: false,
                motion: previous
                    .as_ref()
                    .map_or_else(BTreeMap::new, |object| object.motion.clone()),
                references: previous.as_ref().map_or_else(
                    || crate::state::object::reference_ids(source),
                    |old| old.references.clone(),
                ),
            };
            state.handles.insert(*handle, id.clone());
            state.objects.insert(id, object);
            focus = Some(focus.map_or(bounds, |previous: Rect| previous.union(bounds)));
        }
        if let Some(group_id) = group_id {
            let group = state
                .objects
                .get_mut(&group_id)
                .expect("group was resolved above");
            group.initialized = true;
            if let Some(focus) = focus {
                let current = group.rest.pos;
                let shifted = Rect {
                    origin: focus.origin + current,
                    size: focus.size,
                };
                let mut delta = Vec2::default();
                if shifted.max().x > viewport.max().x {
                    delta.x = viewport.max().x - shifted.max().x;
                } else if shifted.origin.x < viewport.origin.x {
                    delta.x = viewport.origin.x - shifted.origin.x;
                }
                if shifted.max().y > viewport.max().y {
                    delta.y = viewport.max().y - shifted.max().y;
                } else if shifted.origin.y < viewport.origin.y {
                    delta.y = viewport.origin.y - shifted.origin.y;
                }
                if delta.x.abs() > f32::EPSILON || delta.y.abs() > f32::EPSILON {
                    glides.push(Glide {
                        target: group.item.handle,
                        to: current + delta,
                    });
                }
            }
        }
    }
    Ok(glides)
}

fn free_slot(
    state: &LayoutState,
    parent: Option<Handle>,
    rank: usize,
    size: Vec2,
    dir: Direction,
    viewport: Rect,
    fallback: Vec2,
) -> Result<Rect, PrepareError> {
    let mut same_rank: Vec<_> = state
        .objects
        .values()
        .filter(|object| object.item.parent == parent && object.rank == Some(rank))
        .map(|object| object.item.bounds)
        .collect();
    let across = |point: Vec2| {
        if dir == Direction::Right {
            point.y
        } else {
            point.x
        }
    };
    same_rank.sort_by(|a, b| across(a.origin).total_cmp(&across(b.origin)));
    let prior = state
        .objects
        .values()
        .filter(|object| {
            object.item.parent == parent && object.rank.is_some_and(|previous| previous < rank)
        })
        .map(|object| {
            if dir == Direction::Right {
                object.item.bounds.max().x
            } else {
                object.item.bounds.max().y
            }
        })
        .max_by(f32::total_cmp);
    let fallback = prior.map_or(
        if dir == Direction::Right {
            fallback.x
        } else {
            fallback.y
        },
        |edge| {
            edge + diagram::RANK_GAP
                + if dir == Direction::Right {
                    size.x * 0.5
                } else {
                    size.y * 0.5
                }
        },
    );
    let along = same_rank.first().map_or(fallback, |rect| {
        if dir == Direction::Right {
            rect.center().x
        } else {
            rect.center().y
        }
    });
    let across_size = if dir == Direction::Right {
        size.y
    } else {
        size.x
    };
    let mut cursor = across(viewport.origin) + 16.0;
    for occupied in &same_rank {
        if cursor + across_size + 15.0 <= across(occupied.origin) {
            break;
        }
        cursor = cursor.max(across(occupied.max()) + 30.0);
    }
    let center = if dir == Direction::Right {
        Vec2::new(along, cursor + across_size * 0.5)
    } else {
        Vec2::new(cursor + across_size * 0.5, along)
    };
    let rect = Rect::centered(center, size);
    if !rect.within_canvas() {
        return Err(layout_error(ErrorCode::OutOfRange));
    }
    Ok(rect)
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the bounded contour variants and their shared labels together"
)]
fn node_primitives(
    node: &Node,
    bounds: Rect,
    label: Vec2,
    sub: Option<Vec2>,
    build: &mut Build<'_>,
) -> Vec<Prim> {
    let color = build.color(node.color.as_ref());
    let fill = Paint::from(build.theme.surface_raised).faded(0.95);
    let mut output = Vec::new();
    match node.shape {
        NodeShape::Diamond => {
            let c = bounds.center();
            let max = bounds.max();
            let points = Arc::from([
                Vec2::new(c.x, bounds.origin.y),
                Vec2::new(max.x, c.y),
                Vec2::new(c.x, max.y),
                Vec2::new(bounds.origin.x, c.y),
            ]);
            output.push(Prim::Fill {
                points: Arc::clone(&points),
                color: fill,
            });
            output.push(Prim::Line {
                points,
                closed: true,
                color,
                width: 1.5,
                dashed: false,
            });
        }
        NodeShape::Cylinder => {
            output.push(Prim::Rect {
                rect: bounds,
                radius: 0.0,
                fill: Some(fill),
                border: None,
            });
            let cap = Rect {
                origin: bounds.origin,
                size: Vec2::new(bounds.size.x, 12.0_f32.min(bounds.size.y * 0.5)),
            };
            let points = ellipse(cap, 16);
            output.push(Prim::Fill {
                points: Arc::clone(&points),
                color: fill,
            });
            output.push(Prim::Line {
                points,
                closed: true,
                color,
                width: 1.5,
                dashed: false,
            });
            let bottom = cap.center() + Vec2::new(0.0, bounds.size.y - cap.size.y);
            let points: Arc<[_]> = (0..=14_u16)
                .map(|n| {
                    let angle = f32::from(n) / 14.0 * std::f32::consts::PI;
                    let (sin, cos) = angle.sin_cos();
                    bottom + Vec2::new(cos * cap.size.x * 0.5, sin * cap.size.y * 0.5)
                })
                .collect();
            output.push(Prim::Line {
                points,
                closed: false,
                color,
                width: 1.5,
                dashed: false,
            });
            for x in [bounds.origin.x, bounds.max().x] {
                output.push(Prim::Line {
                    points: Arc::from([Vec2::new(x, cap.center().y), Vec2::new(x, bottom.y)]),
                    closed: false,
                    color,
                    width: 1.5,
                    dashed: false,
                });
            }
        }
        NodeShape::Note => {
            let o = bounds.origin;
            let m = bounds.max();
            let fold = 12.0_f32.min(bounds.size.x * 0.25).min(bounds.size.y * 0.25);
            let points = Arc::from([
                o,
                Vec2::new(m.x - fold, o.y),
                Vec2::new(m.x, o.y + fold),
                m,
                Vec2::new(o.x, m.y),
            ]);
            output.push(Prim::Fill {
                points: Arc::clone(&points),
                color: fill,
            });
            output.push(Prim::Line {
                points,
                closed: true,
                color,
                width: 1.5,
                dashed: false,
            });
            output.push(Prim::Line {
                points: Arc::from([
                    Vec2::new(m.x - fold, o.y),
                    Vec2::new(m.x - fold, o.y + fold),
                    Vec2::new(m.x, o.y + fold),
                ]),
                closed: false,
                color,
                width: 1.5,
                dashed: false,
            });
        }
        _ => {
            let radius = match node.shape {
                NodeShape::Circle | NodeShape::Pill => bounds.size.y * 0.5,
                NodeShape::Round => 12.0,
                _ => 4.0,
            };
            output.push(Prim::Rect {
                rect: bounds,
                radius,
                fill: Some(fill),
                border: Some((color, 1.5)),
            });
        }
    }
    let total_height = label.y + sub.map_or(0.0, |size| size.y + 4.0);
    let origin = Vec2::new(
        bounds.center().x - label.x * 0.5,
        bounds.center().y - total_height * 0.5,
    );
    output.push(Prim::Text {
        origin,
        text: node.label.clone(),
        style: TextStyle {
            size: build.theme.font_size,
            bold: true,
        },
        color: build.theme.fg.into(),
        max_width: Some(180.0),
        align: Align::Center,
    });
    if let Some(text) = &node.sub {
        let size = sub.expect("subtitle was measured");
        output.push(Prim::Text {
            origin: Vec2::new(bounds.center().x - size.x * 0.5, origin.y + label.y + 4.0),
            text: text.clone(),
            style: TextStyle {
                size: build.theme.font_size - 2.0,
                bold: false,
            },
            color: build.theme.muted.into(),
            max_width: Some(180.0),
            align: Align::Center,
        });
    }
    output
}

pub(super) fn edge(
    source: &Arc<Op>,
    handle: Handle,
    parent: Option<Handle>,
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
        [from.edge_toward(to.center()), to.edge_toward(from.center())]
    };
    let middle = (a + b) * 0.5;
    let control = Vec2::new(middle.x, a.y);
    let mut points = Vec::new();
    let pieces = u16::try_from(crate::path::PROCEDURAL_CURVE_PIECES)
        .expect("procedural curve count fits u16");
    for n in 0..=pieces {
        let t = f32::from(n) / f32::from(pieces);
        points.push(a * (1.0 - t).powi(2) + control * (2.0 * (1.0 - t) * t) + b * t.powi(2));
    }
    let head = arrow_head(points[usize::from(pieces) - 1], points[usize::from(pieces)]);
    let mut bounds = point_bounds(&points)?.union(point_bounds(&head)?);
    let color = build.color(edge.color.as_ref());
    let mut primitives = vec![
        Prim::Line {
            points: points.into(),
            closed: false,
            color,
            width: 2.0,
            dashed: edge.dash,
        },
        Prim::Fill {
            points: head.into(),
            color,
        },
    ];
    if let Some(label) = &edge.label {
        let style = TextStyle {
            size: build.theme.font_size - 1.0,
            bold: false,
        };
        let size = build.measure(label, style, Some(180.0))?;
        let origin = middle - size * 0.5;
        bounds = bounds.union(Rect { origin, size });
        primitives.push(Prim::Text {
            origin,
            text: label.clone(),
            style,
            color: build.theme.fg.into(),
            max_width: Some(180.0),
            align: Align::Center,
        });
    }
    Ok(Placed {
        item: Item {
            handle,
            parent,
            primitives: primitives.into(),
            bounds,
            transform: cached.map_or_else(Affine::default, |old| old.item.transform),
            opacity: edge.opacity.map_or(1.0, crate::Finite::get),
        },
        source: Arc::clone(source),
        anchors: [a, b],
        rest: cached.map_or_else(Rest::default, |old| old.rest),
        rank: None,
        initialized: false,
        motion: cached.map_or_else(BTreeMap::new, |old| old.motion.clone()),
        references: cached.map_or_else(
            || crate::state::object::reference_ids(source),
            |old| old.references.clone(),
        ),
    })
}
