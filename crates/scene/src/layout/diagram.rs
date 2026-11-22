//! Layered layout for node-and-edge diagrams.
//!
//! Ranks nodes by longest path along the flow (cycles broken by DFS), orders each rank by
//! barycenter sweeps to reduce crossings, then stacks ranks along the flow direction.

use crate::{
    Budget, ErrorCode,
    geom::{Rect, Vec2},
    limits::{MAX_DIAGRAM_EDGES, MAX_DIAGRAM_NODES},
    record::Direction,
};

/// Smallest gap between ranks.
// shortcut: callers widen one gap per group for its widest edge label, not per rank pair;
// upgrade when one long label visibly spreads a whole diagram.
pub(super) const RANK_GAP: f32 = 72.0;
const NODE_GAP: f32 = 30.0;
const SWEEPS: usize = 4;

/// Node centers in layout space and the bounds of the whole drawing.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Placement {
    /// Node rectangles in input order.
    pub rects: Vec<Rect>,
    /// Union of the placed node rectangles.
    pub bounds: Rect,
    /// Longest-path rank of each node.
    pub ranks: Vec<usize>,
}

/// Lays out members of the given sizes: `nodes` of them are nodes and the rest are nested
/// clusters, which don't count toward the node cap. Edges index into `sizes`; edges out of
/// range or self-loops are ignored. A `reverse` flow mirrors the ranks, so the first is last.
/// # Errors
/// Rejects excess nodes/edges, invalid dimensions or out-of-cap generated coordinates.
pub(super) fn layout(
    sizes: &[Vec2],
    nodes: usize,
    edges: &[(usize, usize)],
    (dir, reverse): (Direction, bool),
    rank_gap: f32,
) -> Result<Placement, ErrorCode> {
    if nodes > MAX_DIAGRAM_NODES {
        return Err(ErrorCode::OverBudget(Budget::DiagramNodes));
    }
    if edges.len() > MAX_DIAGRAM_EDGES {
        return Err(ErrorCode::OverBudget(Budget::DiagramEdges));
    }
    if !rank_gap.is_finite()
        || sizes
            .iter()
            .any(|s| !s.within_canvas() || s.x <= 0.0 || s.y <= 0.0)
    {
        return Err(ErrorCode::OutOfRange);
    }
    let n = sizes.len();
    let edges: Vec<(usize, usize)> = edges
        .iter()
        .copied()
        .filter(|&(a, b)| a < n && b < n && a != b)
        .collect();
    let forward = acyclic(n, &edges);
    let ranks = longest_path_ranks(n, &forward);
    let rank_count = ranks.iter().max().map_or(0, |r| r + 1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); rank_count];
    for (v, &r) in ranks.iter().enumerate() {
        layers[r].push(v);
    }
    order_layers(&mut layers, &forward, n);

    let along = |s: Vec2| if dir == Direction::Right { s.x } else { s.y };
    let across = |s: Vec2| if dir == Direction::Right { s.y } else { s.x };
    let sign = if reverse { -1.0 } else { 1.0 };
    let mut rects = vec![Rect::default(); n];
    let mut offset = 0.0;
    for layer in &layers {
        let depth = layer.iter().map(|&v| along(sizes[v])).fold(0.0, f32::max);
        let span: f32 = layer.iter().map(|&v| across(sizes[v])).sum::<f32>()
            + NODE_GAP * count_f32(layer.len().saturating_sub(1));
        let mut cursor = -span / 2.0;
        for &v in layer {
            let size = sizes[v];
            let center_along = sign * (offset + depth / 2.0);
            let center_across = cursor + across(size) / 2.0;
            let center = if dir == Direction::Right {
                Vec2::new(center_along, center_across)
            } else {
                Vec2::new(center_across, center_along)
            };
            rects[v] = Rect::centered(center, size);
            cursor += across(size) + NODE_GAP;
        }
        offset += depth + rank_gap.max(RANK_GAP);
    }
    let bounds = rects
        .iter()
        .copied()
        .reduce(Rect::union)
        .unwrap_or_default();
    if !bounds.within_canvas() {
        return Err(ErrorCode::OutOfRange);
    }
    Ok(Placement {
        rects,
        bounds,
        ranks,
    })
}

fn count_f32(count: usize) -> f32 {
    f32::from(u16::try_from(count).expect("diagram member and edge caps fit u16"))
}

/// Drops edges that close a cycle, found by DFS in insertion order.
fn acyclic(n: usize, edges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let mut out_edges = vec![Vec::new(); n];
    for (i, &(a, b)) in edges.iter().enumerate() {
        out_edges[a].push((b, i));
    }
    let mut mark = vec![Mark::New; n];
    let mut keep = vec![true; edges.len()];
    for root in 0..n {
        if mark[root] != Mark::New {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        mark[root] = Mark::Open;
        while let Some((v, next)) = stack.pop() {
            if let Some(&(w, edge)) = out_edges[v].get(next) {
                stack.push((v, next + 1));
                match mark[w] {
                    Mark::Open => keep[edge] = false,
                    Mark::New => {
                        mark[w] = Mark::Open;
                        stack.push((w, 0));
                    }
                    Mark::Done => {}
                }
            } else {
                mark[v] = Mark::Done;
            }
        }
    }
    edges
        .iter()
        .zip(keep)
        .filter_map(|(&e, k)| k.then_some(e))
        .collect()
}

fn longest_path_ranks(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    let mut indegree = vec![0usize; n];
    let mut out_edges = vec![Vec::new(); n];
    for &(a, b) in edges {
        indegree[b] += 1;
        out_edges[a].push(b);
    }
    let mut rank = vec![0usize; n];
    let mut queue: std::collections::VecDeque<usize> =
        (0..n).filter(|&v| indegree[v] == 0).collect();
    while let Some(v) = queue.pop_front() {
        for &w in &out_edges[v] {
            rank[w] = rank[w].max(rank[v] + 1);
            indegree[w] -= 1;
            if indegree[w] == 0 {
                queue.push_back(w);
            }
        }
    }
    rank
}

fn order_layers(layers: &mut [Vec<usize>], edges: &[(usize, usize)], n: usize) {
    let mut position = vec![0.0f32; n];
    let index = |layers: &[Vec<usize>], position: &mut [f32]| {
        for layer in layers {
            for (i, &v) in layer.iter().enumerate() {
                position[v] = count_f32(i);
            }
        }
    };
    index(layers, &mut position);
    for sweep in 0..SWEEPS {
        let down = sweep % 2 == 0;
        let order: Vec<usize> = if down {
            (1..layers.len()).collect()
        } else {
            (0..layers.len().saturating_sub(1)).rev().collect()
        };
        for r in order {
            let keyed: Vec<(f32, usize)> = layers[r]
                .iter()
                .map(|&v| {
                    let neighbors: Vec<f32> = edges
                        .iter()
                        .filter_map(|&(a, b)| match down {
                            true if b == v => Some(position[a]),
                            false if a == v => Some(position[b]),
                            _ => None,
                        })
                        .collect();
                    let key = if neighbors.is_empty() {
                        position[v]
                    } else {
                        neighbors.iter().sum::<f32>() / count_f32(neighbors.len())
                    };
                    (key, v)
                })
                .collect();
            let mut keyed = keyed;
            keyed.sort_by(|a, b| {
                a.0.total_cmp(&b.0)
                    .then(position[a.1].total_cmp(&position[b.1]))
            });
            layers[r] = keyed.into_iter().map(|(_, v)| v).collect();
            index(layers, &mut position);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_ranks_left_to_right_and_survives_cycles() {
        let sizes = vec![Vec2::new(100.0, 40.0); 4];
        let p = layout(
            &sizes,
            4,
            &[(0, 1), (1, 2), (2, 3), (3, 1), (2, 2), (9, 1)],
            (Direction::Right, false),
            RANK_GAP,
        )
        .expect("bounded chain");
        assert_eq!(p.ranks, vec![0, 1, 2, 3]);
        assert_eq!(p.rects.len(), 4);
        assert!(
            p.rects
                .windows(2)
                .all(|pair| pair[0].max().x < pair[1].origin.x),
            "each rank is separated to the right of the previous one"
        );
    }

    #[test]
    fn siblings_stack_across_the_flow() {
        let sizes = vec![Vec2::new(80.0, 40.0); 3];
        let p = layout(
            &sizes,
            3,
            &[(0, 1), (0, 2)],
            (Direction::Down, false),
            RANK_GAP,
        )
        .expect("bounded siblings");
        assert_eq!(p.ranks, vec![0, 1, 1]);
        let [root, a, b] = p.rects.as_slice() else {
            panic!("one root and two siblings");
        };
        assert!(
            root.max().y < a.origin.y
                && root.max().y < b.origin.y
                && (a.center().y - b.center().y).abs() < 1e-3,
            "both siblings share one rank below the root"
        );
        assert!(
            a.max().x < b.origin.x || b.max().x < a.origin.x,
            "siblings have a horizontal gap"
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn empty_singleton_and_exact_caps_are_deterministic() {
        assert_eq!(
            layout(&[], 0, &[], (Direction::Down, false), RANK_GAP).expect("empty"),
            Placement {
                rects: vec![],
                bounds: Rect::default(),
                ranks: vec![]
            }
        );
        let singleton = layout(
            &[Vec2::new(80.0, 40.0)],
            1,
            &[],
            (Direction::Right, false),
            0.0,
        )
        .expect("singleton");
        assert_eq!(singleton.ranks, [0]);
        assert_eq!(singleton.rects, [singleton.bounds]);
        assert_eq!(singleton.bounds.size, Vec2::new(80.0, 40.0));
        let sizes = vec![Vec2::new(80.0, 40.0); MAX_DIAGRAM_NODES];
        let edges = vec![(0, 1); MAX_DIAGRAM_EDGES];
        let exact =
            layout(&sizes, 48, &edges, (Direction::Down, false), RANK_GAP).expect("exact caps");
        assert!(exact.bounds.within_canvas(), "the maximum diagram fits");
        assert!(
            exact.rects.iter().all(|rect| {
                rect.size == Vec2::new(80.0, 40.0)
                    && rect.origin.x >= exact.bounds.origin.x
                    && rect.origin.y >= exact.bounds.origin.y
                    && rect.max().x <= exact.bounds.max().x
                    && rect.max().y <= exact.bounds.max().y
            }),
            "all 48 nodes keep their size and are covered by the diagram bounds"
        );
        let mut ranks = [0; 48];
        ranks[1] = 1;
        assert_eq!(exact.ranks, ranks);
        assert_eq!(exact.rects.len(), 48);
        assert_eq!(
            layout(
                &vec![Vec2::new(1.0, 1.0); 49],
                49,
                &[],
                (Direction::Down, false),
                RANK_GAP
            )
            .expect_err("node cap"),
            ErrorCode::OverBudget(Budget::DiagramNodes)
        );
        assert_eq!(
            layout(
                &sizes,
                48,
                &vec![(0, 1); 97],
                (Direction::Down, false),
                RANK_GAP
            )
            .expect_err("edge cap"),
            ErrorCode::OverBudget(Budget::DiagramEdges)
        );
    }
    #[test]
    fn unsafe_measurements_and_generated_coordinates_fail() {
        for size in [
            Vec2::new(f32::NAN, 1.0),
            Vec2::new(0.0, 1.0),
            Vec2::new(1.0, -1.0),
            Vec2::new(20_001.0, 1.0),
        ] {
            assert_eq!(
                layout(&[size], 1, &[], (Direction::Down, false), RANK_GAP)
                    .expect_err("unsafe size"),
                ErrorCode::OutOfRange
            );
        }
        assert_eq!(
            layout(
                &[Vec2::new(1.0, 1.0)],
                1,
                &[],
                (Direction::Down, false),
                f32::INFINITY
            )
            .expect_err("unsafe gap"),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            layout(
                &[Vec2::new(20_000.0, 20_000.0); 2],
                2,
                &[(0, 1)],
                (Direction::Right, false),
                RANK_GAP
            )
            .expect_err("generated overflow"),
            ErrorCode::OutOfRange
        );
    }
}
