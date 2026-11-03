use super::{
    object::{Kind, Object},
    work::{Domain, Motion},
};
use crate::{
    Budget, ErrorCode, FieldPath, Step, StepError, WireId,
    budget::Usage,
    limits::{
        MAX_CHART_SERIES, MAX_COMPOSED_SCALE, MAX_COORD, MAX_DIAGRAM_EDGES, MAX_DIAGRAM_NODES,
        MAX_KEYS, MAX_LIVE_OBJECTS, MAX_OBJECT_DEPTH, MAX_TABLE_ROWS, MAX_TRACKS,
    },
    record::{ChartKind, Point},
};
use std::collections::BTreeMap;

pub(super) fn validate(
    step: &Step,
    objects: &mut BTreeMap<WireId, Object>,
) -> Result<(), StepError> {
    let fail = |code| StepError::in_step(&step.id, FieldPath::default().field("put"), code);
    let memo = graph_metrics(objects).map_err(&fail)?;
    let columns: BTreeMap<_, _> = objects
        .iter()
        .filter(|(_, o)| o.kind == Kind::Table)
        .map(|(id, o)| (id.clone(), o.work.columns))
        .collect();
    let charts: BTreeMap<_, _> = objects
        .iter()
        .filter_map(|(id, object)| {
            (object.kind == Kind::Chart)
                .then_some(object.work.chart)
                .flatten()
                .map(|chart| (id.clone(), chart))
        })
        .collect();
    for object in objects.values_mut() {
        if object.kind == Kind::Series
            && let Some(parent) = &object.parent
        {
            object.work.chart = charts.get(parent).copied();
        }
        if object.kind == Kind::Row {
            let cols = object
                .parent
                .as_ref()
                .and_then(|id| columns.get(id))
                .copied()
                .unwrap_or(object.work.usage.cells);
            if object.work.cells > cols {
                return Err(fail(ErrorCode::OutOfRange));
            }
            object.work.usage.cells = cols;
            object.work.usage.segments = cols + 2;
        }
        if let Some(path) = &mut object.work.path {
            let scale = memo
                .get(&object.handle)
                .expect("every object has graph metrics")
                .scale;
            object.work.usage.segments = path.segments(scale).map_err(&fail)?;
            path.composed_scale = scale;
        }
        if object.kind == Kind::Series
            && let Some(chart) = object.work.chart
        {
            let count = object.work.usage.marks;
            object.work.usage.segments = match chart.kind {
                ChartKind::Line => count.saturating_sub(1),
                ChartKind::Area => {
                    if count < 2 {
                        0
                    } else {
                        count * 2 + 1
                    }
                }
                ChartKind::Pie => count * (crate::path::PIE_ARC_SEGMENTS + 2),
                ChartKind::Bar | ChartKind::Scatter => 0,
            };
        }
    }
    domains(step, objects)
}

#[derive(Clone, Copy)]
struct Graph {
    depth: usize,
    scale: f32,
    min_scale: f32,
    translation: f32,
    rotated: bool,
}
fn graph_metrics(
    objects: &BTreeMap<WireId, Object>,
) -> Result<BTreeMap<crate::Handle, Graph>, ErrorCode> {
    let mut memo = BTreeMap::new();
    for id in objects.keys() {
        graph(id, objects, &mut memo, 0)?;
    }
    Ok(memo)
}
pub(crate) fn validate_transforms(objects: &BTreeMap<WireId, Object>) -> Result<(), ErrorCode> {
    graph_metrics(objects).map(|_| ())
}
fn graph(
    id: &WireId,
    objects: &BTreeMap<WireId, Object>,
    memo: &mut BTreeMap<crate::Handle, Graph>,
    level: usize,
) -> Result<Graph, ErrorCode> {
    if level >= MAX_OBJECT_DEPTH {
        return Err(ErrorCode::TooDeep);
    }
    let object = &objects[id];
    if let Some(value) = memo.get(&object.handle) {
        return Ok(*value);
    }
    let mut depth = 1;
    for reference in &object.references {
        depth = depth.max(graph(reference, objects, memo, level + 1)?.depth + 1);
        if depth > MAX_OBJECT_DEPTH {
            return Err(ErrorCode::TooDeep);
        }
    }
    let parent = object
        .parent
        .as_ref()
        .map(|id| graph(id, objects, memo, level + 1))
        .transpose()?
        .unwrap_or(Graph {
            depth: 0,
            scale: 1.0,
            min_scale: 1.0,
            translation: 0.0,
            rotated: false,
        });
    let scale_motion = object.work.motion.get("scale");
    let scalar = scale_motion.map_or(1.0, |m| m.max[0].max(1.0));
    let basis = parent.scale * object.work.inherited_scale;
    let (enter_lo, enter_hi, enter_offset) = object
        .work
        .enter
        .map_or((1.0, 1.0, 0.0), crate::motion::entrance_envelope);
    let before_enter = basis * object.work.scale * scalar;
    let scale = before_enter * enter_hi;
    let min_scalar = scale_motion.map_or(1.0, |motion| motion.min[0].min(1.0));
    let min_scale = parent.min_scale
        * object.work.inherited_min_scale
        * object.work.scale
        * min_scalar
        * enter_lo;
    let rotated = parent.rotated
        || object.work.inherited_rotation
        || object
            .work
            .motion
            .get("rotate")
            .is_some_and(|m| m.min[0] != 0.0 || m.max[0] != 0.0);
    let rotation_bound = if rotated {
        std::f32::consts::SQRT_2
    } else {
        1.0
    };
    let translation = parent.translation
        + before_enter
            * ((enter_hi - 1.0).max(0.0) * object.work.extent + enter_offset)
            * rotation_bound
        + object.work.inherited_translation
        + basis * scalar * object.work.offset * rotation_bound
        + basis
            * ["x", "y", "pos"]
                .iter()
                .filter_map(|key| object.work.motion.get(key))
                .map(|m| {
                    m.min[0]
                        .abs()
                        .max(m.max[0].abs())
                        .max(m.min[1].abs())
                        .max(m.max[1].abs())
                })
                .sum::<f32>()
            * rotation_bound;
    let extent = object.work.extent * scale * rotation_bound + translation;
    if !scale.is_finite()
        || scale > MAX_COMPOSED_SCALE
        || min_scale <= 0.0
        || !min_scale.is_normal()
        || !translation.is_finite()
        || !extent.is_finite()
    {
        return Err(ErrorCode::TransformOverflow);
    }
    if extent > MAX_COORD {
        return Err(ErrorCode::OutOfRange);
    }
    let value = Graph {
        depth,
        scale,
        min_scale,
        translation,
        rotated,
    };
    memo.insert(object.handle, value);
    Ok(value)
}

pub(super) fn freeze_detached(
    objects: &mut BTreeMap<WireId, Object>,
    revealed: &std::collections::BTreeSet<crate::Handle>,
) -> Result<(), ErrorCode> {
    let mut memo = BTreeMap::new();
    let mut detached = Vec::new();
    for (id, object) in &*objects {
        if !revealed.contains(&object.handle) {
            continue;
        }
        if let Some(parent) = &object.parent
            && !revealed.contains(&objects[parent].handle)
        {
            detached.push((id.clone(), graph(parent, objects, &mut memo, 0)?));
        }
    }
    for (id, transform) in detached {
        let object = objects
            .get_mut(&id)
            .expect("detached ids came from this graph");
        object.work.inherited_scale = transform.scale;
        object.work.inherited_min_scale = transform.min_scale;
        object.work.inherited_translation = transform.translation;
        object.work.inherited_rotation = transform.rotated;
    }
    Ok(())
}

pub(super) fn charge<'a>(
    versions: impl IntoIterator<Item = (&'a WireId, &'a Object)>,
) -> Result<Usage, ErrorCode> {
    let mut objects = BTreeMap::<&WireId, (&Object, Usage, BTreeMap<&'static str, Motion>)>::new();
    for (id, object) in versions {
        let entry = objects
            .entry(id)
            .or_insert_with(|| (object, object.work.usage, BTreeMap::new()));
        entry.1 = entry.1.peak(object.work.usage);
        for (key, value) in &object.work.motion {
            entry
                .2
                .entry(key)
                .and_modify(|previous| *previous = previous.peak(*value))
                .or_insert(*value);
        }
    }
    if objects.len() > MAX_LIVE_OBJECTS {
        return Err(ErrorCode::OverBudget(Budget::LiveObjects));
    }
    let mut groups = BTreeMap::<Option<&WireId>, [usize; 4]>::new();
    let mut usage = Usage {
        scenes: 1,
        ..Usage::default()
    };
    for (object, work, motions) in objects.values() {
        usage = usage.add(*work);
        usage.tracks += motions.len();
        usage.keys += motions.values().map(|m| m.keys).sum::<usize>();
        let group = groups.entry(object.parent.as_ref()).or_default();
        match object.kind {
            Kind::Node => group[0] += 1,
            Kind::Edge => group[1] += 1,
            Kind::Row => group[2] += 1,
            Kind::Series => group[3] += 1,
            _ => {}
        }
    }
    for (parent, counts) in &groups {
        if parent.is_none() {
            continue;
        }
        for (count, cap, budget) in [
            (counts[0], MAX_DIAGRAM_NODES, Budget::DiagramNodes),
            (counts[1], MAX_DIAGRAM_EDGES, Budget::DiagramEdges),
            (counts[2], MAX_TABLE_ROWS, Budget::TableCells),
            (counts[3], MAX_CHART_SERIES, Budget::ChartMarks),
        ] {
            if count > cap {
                return Err(ErrorCode::OverBudget(budget));
            }
        }
    }
    if usage.tracks > MAX_TRACKS {
        return Err(ErrorCode::OverBudget(Budget::Tracks));
    }
    if usage.keys > MAX_TRACKS * MAX_KEYS {
        return Err(ErrorCode::OverBudget(Budget::Keys));
    }
    Ok(usage)
}

fn point(point: &Point, index: usize) -> [f64; 2] {
    match point {
        Point::Value(y) => [
            f64::from(u32::try_from(index).expect("series indices are capped")),
            y.get(),
        ],
        Point::Pair(p) => [p[0].get(), p[1].get()],
    }
}
fn valid_domain(domain: Domain, min: f64, max: f64) -> bool {
    if domain
        .min
        .zip(domain.max)
        .is_some_and(|(a, b)| a >= b || !(b - a).is_finite())
    {
        return false;
    }
    if domain.log && domain.min.into_iter().chain(domain.max).any(|v| v <= 0.0) {
        return false;
    }
    if min.is_infinite() {
        return true;
    }
    if domain.log && min <= 0.0 {
        return false;
    }
    let low = domain.min.unwrap_or(min);
    let high = domain.max.unwrap_or(max);
    high >= low && (high - low).is_finite()
}
fn domains(step: &Step, objects: &BTreeMap<WireId, Object>) -> Result<(), StepError> {
    let mut groups = BTreeMap::new();
    for (id, object) in objects {
        let Some(chart) = object.work.chart else {
            continue;
        };
        if object.kind == Kind::Chart {
            groups.entry(id).or_insert((chart, Vec::new()));
        } else if let Some(owner) = &object.work.chart_owner {
            groups
                .entry(owner)
                .or_insert((chart, Vec::new()))
                .1
                .push(object);
        }
    }
    for (chart, series) in groups.values() {
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        let mut positive = [0.0_f64; crate::limits::MAX_SERIES_POINTS];
        let mut negative = positive;
        let mut pie = 0.0_f64;
        for series in series {
            let Some(data) = &series.work.data else {
                continue;
            };
            let data = data.values(step);
            if matches!(chart.kind, ChartKind::Bar | ChartKind::Pie)
                && data.len() > crate::limits::MAX_CHART_CATEGORIES
            {
                return Err(StepError::in_step(
                    &step.id,
                    FieldPath::default().field("put"),
                    ErrorCode::OverBudget(Budget::ChartMarks),
                ));
            }
            for (index, entry) in data.iter().enumerate() {
                let p = point(entry, index);
                for axis in 0..2 {
                    min[axis] = min[axis].min(p[axis]);
                    max[axis] = max[axis].max(p[axis]);
                }
                if chart.stacked {
                    let sum = if p[1] >= 0.0 {
                        &mut positive[index]
                    } else {
                        &mut negative[index]
                    };
                    *sum += p[1];
                    if !sum.is_finite() {
                        return Err(StepError::in_step(
                            &step.id,
                            FieldPath::default().field("put"),
                            ErrorCode::OutOfRange,
                        ));
                    }
                    min[1] = min[1].min(*sum);
                    max[1] = max[1].max(*sum);
                }
                if chart.kind == ChartKind::Pie {
                    if p[1] < 0.0 {
                        return Err(StepError::in_step(
                            &step.id,
                            FieldPath::default().field("put"),
                            ErrorCode::OutOfRange,
                        ));
                    }
                    pie += p[1];
                    if !pie.is_finite() {
                        return Err(StepError::in_step(
                            &step.id,
                            FieldPath::default().field("put"),
                            ErrorCode::OutOfRange,
                        ));
                    }
                }
            }
        }
        for (axis, domain) in [chart.x, chart.y].into_iter().enumerate() {
            if !valid_domain(domain, min[axis], max[axis]) {
                return Err(StepError::in_step(
                    &step.id,
                    FieldPath::default().field("put"),
                    ErrorCode::OutOfRange,
                ));
            }
        }
    }
    Ok(())
}
