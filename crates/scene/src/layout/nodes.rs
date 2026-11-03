use super::{
    Glide, PrepareError, SurfaceGeometry, compound, diagram,
    primitives::{Build, arrow_head, ellipse, layout_error, point_bounds, rect},
    state::{LayoutState, Placed, Rest},
};
use crate::{
    ErrorCode, Handle, Op,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    mermaid::Extra,
    record::{Align, Direction, Head, Node, NodeShape},
    state::object,
};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Places nodes and clusters, and returns the generated glides plus the new contents a cluster
/// owns that are built here, anchored inside it, so they are in its size before it is ranked.
/// New nodes take their prepared `rests` first, so those contents and sizes see resting bounds.
#[expect(
    clippy::too_many_lines,
    reason = "initial batch and stable incremental placement share the same measured node index"
)]
pub(super) fn place(
    sources: &BTreeMap<crate::WireId, (Handle, Arc<Op>)>,
    order: &[crate::WireId],
    rests: &BTreeMap<crate::WireId, Rest>,
    state: &mut LayoutState,
    build: &mut Build<'_>,
    surface: SurfaceGeometry,
) -> Result<(Vec<Glide>, BTreeSet<crate::WireId>), PrepareError> {
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
    let mut parents = BTreeMap::<crate::WireId, (Handle, Option<Handle>)>::new();
    let mut groups = BTreeMap::<Option<Handle>, Vec<crate::WireId>>::new();
    for id in order {
        let Some((handle, source)) = sources.get(id) else {
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
        parents.insert(id.clone(), (*handle, parent));
        groups.entry(parent).or_default().push(id.clone());
    }
    for (id, object) in &state.objects {
        if object.rank.is_none() && compound::is_cluster(state, object) {
            groups
                .entry(object.item.parent)
                .or_default()
                .push(id.clone());
            // Its own pass measures contents that aren't new nodes, such as a table.
            groups.entry(Some(object.item.handle)).or_default();
        }
    }
    if groups.is_empty() {
        return Ok((Vec::new(), BTreeSet::new()));
    }
    // An edge ranks once, in its endpoints' least common group, between the two members there
    // that hold them; edges inside one member never count toward that group's edge cap.
    let mut ranked = BTreeMap::<Option<Handle>, Vec<(Handle, Handle)>>::new();
    let mut edges = BTreeMap::new();
    for object in state.objects.values() {
        if let Op::Edge(edge) = &*object.source {
            edges.entry(object.item.handle).or_insert(edge);
        }
    }
    for (handle, source) in sources.values() {
        if let Op::Edge(edge) = &**source {
            edges.entry(*handle).or_insert(edge);
        }
    }
    let mut chains = BTreeMap::new();
    for edge in edges.into_values() {
        for id in [&edge.from, &edge.to] {
            chains
                .entry(id)
                .or_insert_with(|| compound::ancestry(state, &parents, id));
        }
        let to = &chains[&edge.to];
        let common = chains[&edge.from].iter().find_map(|&(from, group)| {
            to.iter()
                .find(|(_, other)| *other == group)
                .map(|&(to, _)| (group, from, to))
        });
        if let Some((group, from, to)) = common
            && from != to
        {
            ranked.entry(group).or_default().push((from, to));
        }
    }
    let mut owned_by = BTreeMap::<Handle, Vec<&crate::WireId>>::new();
    for id in order {
        let Some((_, source)) = sources.get(id) else {
            continue;
        };
        if state.objects.contains_key(id)
            || !matches!(
                &**source,
                Op::Line(_)
                    | Op::Arrow(_)
                    | Op::Path(_)
                    | Op::Shape(_)
                    | Op::Text(_)
                    | Op::Callout(_)
                    | Op::Highlight(_)
                    | Op::Edge(_)
            )
        {
            continue;
        }
        if let Some(owner) = object::visual(source)
            .and_then(|visual| visual.parent)
            .and_then(|owner| state.objects.get(owner))
        {
            owned_by.entry(owner.item.handle).or_default().push(id);
        }
    }
    // Each group's children, placed or built below, in the id order the final bounds pass unions.
    let mut children = BTreeMap::<Option<Handle>, BTreeSet<crate::WireId>>::new();
    for (id, parent) in state
        .objects
        .iter()
        .map(|(id, object)| (id, object.item.parent))
        .chain(parents.iter().map(|(id, &(_, parent))| (id, parent)))
        .chain(
            owned_by
                .iter()
                .flat_map(|(&owner, ids)| ids.iter().map(move |&id| (id, Some(owner)))),
        )
    {
        if groups.contains_key(&parent) {
            children.entry(parent).or_default().insert(id.clone());
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_cached_key(|(parent, _)| Reverse(compound::depth(state, *parent)));
    let mut glides = Vec::new();
    let mut prebuilt = BTreeSet::new();
    for (parent, mut new_ids) in groups {
        build.check_cancel()?;
        let group_id = parent.and_then(|handle| state.handles.get(&handle).cloned());
        let group = group_id.as_ref().and_then(|id| state.objects.get(id));
        let cluster = group.is_some_and(|group| compound::is_cluster(state, group));
        let viewport = group
            .and_then(|group| match &*group.source {
                Op::Diagram(diagram) => diagram.bounds.map(rect),
                _ => None,
            })
            .unwrap_or(build.viewport);
        let direction = compound::direction(state, parent, surface);
        let initialized = group.is_some_and(|group| group.initialized);
        let members = children.remove(&parent).unwrap_or_default();
        let mut all = Vec::new();
        let mut nodes = 0;
        for id in &members {
            let Some(object) = state.objects.get(id) else {
                continue;
            };
            if matches!(&*object.source, Op::Node(_)) {
                nodes += 1;
                all.push((object.item.handle, id.clone(), object.item.bounds.size));
            } else if compound::is_cluster(state, object) {
                all.push((
                    object.item.handle,
                    id.clone(),
                    compound::member_rect(object).size,
                ));
            }
        }
        for id in &new_ids {
            let Some(&(size, _, _)) = measured.get(id) else {
                continue;
            };
            if let Some(existing) = all.iter_mut().find(|(_, existing, _)| existing == id) {
                existing.2 = size;
            } else {
                nodes += 1;
                all.push((sources[id].0, id.clone(), size));
            }
        }
        all.sort_by_key(|(handle, _, _)| *handle);
        let sizes: Vec<_> = all.iter().map(|(_, _, size)| *size).collect();
        let index: BTreeMap<_, _> = all
            .iter()
            .enumerate()
            .map(|(index, (_, id, _))| (id.clone(), index))
            .collect();
        let fresh: Vec<_> = all
            .iter()
            .map(|(_, id, _)| {
                state
                    .objects
                    .get(id)
                    .is_none_or(|object| object.rank.is_none())
            })
            .collect();
        let slot = |member: Handle| {
            all.binary_search_by_key(&member, |(handle, _, _)| *handle)
                .ok()
        };
        let edges: Vec<_> = ranked
            .get(&parent)
            .into_iter()
            .flatten()
            .filter_map(|&(from, to)| Some((slot(from)?, slot(to)?)))
            .collect();
        let placement = diagram::layout(&sizes, nodes, &edges, direction, diagram::RANK_GAP)?;
        let mut ranks = placement.ranks.clone();
        for (slot, (_, id, _)) in all.iter().enumerate() {
            if let Some(rank) = state.objects.get(id).and_then(|object| object.rank) {
                ranks[slot] = rank;
            }
        }
        for _ in 0..all.len() {
            for &(from, to) in &edges {
                if placement.ranks[from] < placement.ranks[to] && fresh[to] {
                    ranks[to] = ranks[to].max(ranks[from] + 1);
                }
            }
        }
        // shortcut: wide initial batches stay unscaled until A9's visual gate.
        let initial = !initialized && all.iter().all(|(_, id, _)| new_ids.contains(id));
        let translate = Vec2::new(
            viewport.center().x - placement.bounds.center().x,
            viewport.origin.y + 16.0 - placement.bounds.origin.y,
        );
        let mut focus = None;
        new_ids.sort_by_key(|id| ranks[index[id]]);
        for id in new_ids {
            let slot = index[&id];
            let rank = ranks[slot];
            let Some(&(size, label, sub)) = measured.get(&id) else {
                let size = sizes[slot];
                let target = if initial {
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
                state.objects.entry(id).and_modify(|member| {
                    let pad = Vec2::new(compound::PAD, compound::PAD);
                    let offset = target.origin + pad - member.item.bounds.origin;
                    member.item.transform = Affine::pose(1.0, 0.0, offset);
                    member.rank = Some(rank);
                });
                focus = Some(focus.map_or(target, |previous: Rect| previous.union(target)));
                continue;
            };
            let (handle, source) = &sources[&id];
            let Op::Node(node) = &**source else {
                unreachable!("measured members are nodes");
            };
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
            let mut object = Placed::new(
                Item {
                    handle: *handle,
                    parent,
                    primitives: primitives.into(),
                    bounds,
                    transform: Affine::default(),
                    opacity: node.opacity.map_or(1.0, crate::Finite::get),
                },
                Arc::clone(source),
                previous.as_ref(),
            );
            object.anchors = [Vec2::default(); 2];
            object.rank = Some(previous.as_ref().and_then(|old| old.rank).unwrap_or(rank));
            object.initialized = false;
            object.rest = rests.get(&id).copied().unwrap_or_default();

            state.handles.insert(*handle, id.clone());
            state.objects.insert(id, object);
            focus = Some(focus.map_or(bounds, |previous: Rect| previous.union(bounds)));
        }
        // Everything below this cluster is placed, so its new contents anchored there are built
        // once, in source order, before its parent ranks it; its own transform cancels in them.
        // shortcut: contents anchored outside this cluster's subtree, or to the cluster itself,
        // are still drawn, but late, after its parent ranks it, so they don't count toward its
        // ranked size; A9's renders judge whether that needs placing first.
        if cluster
            && let Some(owner) = parent
            && let Some(waiting) = owned_by.remove(&owner)
        {
            let mut waiting: Vec<_> = waiting
                .into_iter()
                .map(|id| {
                    let mut references = object::reference_ids(&sources[id].1);
                    if let Some(within) = references
                        .iter()
                        .position(|reference| Some(reference) == group_id.as_ref())
                    {
                        references.swap_remove(within);
                    }
                    (id, references)
                })
                .collect();
            while let Some(index) = waiting.iter().position(|(_, references)| {
                references
                    .iter()
                    .all(|reference| compound::inside(state, reference, owner))
            }) {
                build.check_cancel()?;
                let (id, _) = waiting.remove(index);
                let (handle, source) = &sources[id];
                let mut object = if matches!(&**source, Op::Edge(_)) {
                    edge(source, *handle, parent, state, build, None)?
                } else {
                    build.primitive(source, *handle, parent, state, None)?
                };
                object.rest = rests.get(id).copied().unwrap_or_default();
                state.handles.insert(*handle, id.clone());
                state.objects.insert(id.clone(), object);
                prebuilt.insert(id.clone());
            }
        }
        if let Some(group_id) = group_id {
            // A cluster's parent ranks its contents, so they must be measured before it.
            let contents = cluster
                .then(|| compound::contents(members.iter().filter_map(|id| state.objects.get(id))))
                .flatten();
            let style = TextStyle {
                size: build.theme.font_size,
                bold: true,
            };
            let title = match parent.and_then(|group| state.extra(group)) {
                Some(Extra::Group {
                    title: Some(title), ..
                }) if cluster => Some((title.clone(), build.measure(title, style, Some(180.0))?)),
                _ => None,
            };
            let group = state
                .objects
                .get_mut(&group_id)
                .expect("group was resolved above");
            group.initialized |= focus.is_some();
            if let Some((text, size)) = title {
                let (bounds, origin) = compound::banded(contents.unwrap_or_default(), size);
                group.item.bounds = bounds;
                group.item.primitives = Arc::from([Prim::Text {
                    origin,
                    text,
                    style,
                    color: build.theme.fg.into(),
                    max_width: Some(180.0),
                    align: Align::Center,
                }]);
                group.title = Some(size);
            } else if let Some(contents) = contents {
                group.item.bounds = contents;
            }
            // shortcut: a cluster growing on a later step never glides or moves, so it can overlap
            // a sibling or end off-screen with nothing bringing it into view; revisit in A9.
            if !cluster && let Some(focus) = focus {
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
    Ok((glides, prebuilt))
}

fn free_slot(
    state: &LayoutState,
    parent: Option<Handle>,
    rank: usize,
    size: Vec2,
    (dir, reverse): (Direction, bool),
    viewport: Rect,
    fallback: Vec2,
) -> Result<Rect, PrepareError> {
    // shortcut: slots ignore edge alignment and the resting glide; revisit in A9.
    let mut same_rank: Vec<_> = state
        .objects
        .values()
        .filter(|object| object.item.parent == parent && object.rank == Some(rank))
        .map(compound::member_rect)
        .collect();
    let across = |point: Vec2| {
        if dir == Direction::Right {
            point.y
        } else {
            point.x
        }
    };
    same_rank.sort_by(|a, b| across(a.origin).total_cmp(&across(b.origin)));
    let lengthwise = |point: Vec2| {
        if dir == Direction::Right {
            point.x
        } else {
            point.y
        }
    };
    // A reversed flow grows toward lower coordinates, so its earlier ranks end at their origins.
    let sign = if reverse { -1.0 } else { 1.0 };
    let prior = state
        .objects
        .values()
        .filter(|object| {
            object.item.parent == parent && object.rank.is_some_and(|previous| previous < rank)
        })
        .map(|object| {
            let rect = compound::member_rect(object);
            sign * lengthwise(if reverse { rect.origin } else { rect.max() })
        })
        .max_by(f32::total_cmp);
    let fallback = prior.map_or(lengthwise(fallback), |edge| {
        sign * (edge + diagram::RANK_GAP + lengthwise(size) * 0.5)
    });
    let along = same_rank
        .first()
        .map_or(fallback, |rect| lengthwise(rect.center()));
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
    let (head, thick) = match state.extra(handle) {
        Some(Extra::Link { head, thick }) => (*head, *thick),
        _ => (Head::End, false),
    };
    let last = usize::from(pieces);
    let mut heads = Vec::new();
    if matches!(head, Head::End | Head::Both) {
        heads.push(arrow_head(points[last - 1], points[last]));
    }
    if matches!(head, Head::Start | Head::Both) {
        heads.push(arrow_head(points[1], points[0]));
    }
    let mut bounds = point_bounds(&points)?;
    for head in &heads {
        bounds = bounds.union(point_bounds(head)?);
    }
    let color = build.color(edge.color.as_ref());
    let mut primitives = vec![Prim::Line {
        points: points.into(),
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
    Ok(placed)
}
