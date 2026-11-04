use super::{
    object::{Object, path as op_path},
    retire,
};
use crate::{
    ErrorCode, FieldPath, Finite, Step, StepError, WireId,
    budget::Usage,
    deferred::Deferred,
    layout::heap,
    limits::{MAX_MOTION_MS, MAX_PATH_POINTS},
    path::{self, PathWork},
    record::{Anchor, Anim, Axis, AxisScale, ChartKind, Ease, Enter, KeyValue, Op, Point, Track},
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Motion {
    pub keys: usize,
    pub min: [f32; 2],
    pub max: [f32; 2],
}
impl Motion {
    pub(crate) fn peak(self, other: Self) -> Self {
        Self {
            keys: self.keys.max(other.keys),
            min: [self.min[0].min(other.min[0]), self.min[1].min(other.min[1])],
            max: [self.max[0].max(other.max[0]), self.max[1].max(other.max[1])],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Domain {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub log: bool,
}
impl From<&Axis> for Domain {
    fn from(axis: &Axis) -> Self {
        Self {
            min: axis.min.map(crate::text::Number::get),
            max: axis.max.map(crate::text::Number::get),
            log: axis.scale == AxisScale::Log,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct Chart {
    pub kind: ChartKind,
    pub x: Domain,
    pub y: Domain,
    pub stacked: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Work {
    pub usage: Usage,
    pub columns: usize,
    pub cells: usize,
    pub chart: Option<Chart>,
    pub chart_owner: Option<WireId>,
    pub data: Option<Data>,
    pub path: Option<PathWork>,
    pub extent: f32,
    pub offset: f32,
    pub scale: f32,
    pub inherited_scale: f32,
    pub inherited_min_scale: f32,
    pub inherited_translation: f32,
    pub inherited_rotation: bool,
    pub enter: Option<Enter>,
    pub motion: BTreeMap<&'static str, Motion>,
}

#[derive(Debug, Clone)]
pub(crate) enum Data {
    Stored(Arc<[Point]>),
    Put(usize),
    Set(usize),
}
impl Data {
    pub(super) fn values<'a>(&'a self, step: &'a Step) -> &'a [Point] {
        match self {
            Self::Stored(data) => data,
            Self::Put(index) => match &step.put[*index] {
                Op::Series(series) => &series.data,
                _ => unreachable!("pending series data indexes a series operation"),
            },
            Self::Set(index) => step.set[*index]
                .data
                .as_ref()
                .expect("pending data indexes a data change"),
        }
    }
    pub(super) fn freeze(&mut self, step: &Step) {
        if !matches!(self, Self::Stored(_)) {
            *self = Self::Stored(Arc::from(self.values(step)));
        }
    }
}

pub(super) fn make(op: &Op, index: usize, step: &Step) -> Result<Work, StepError> {
    let mut work = Work {
        usage: Usage {
            objects: 1,
            segments: fixed_segments(op),
            ..Usage::default()
        },
        scale: 1.0,
        inherited_scale: 1.0,
        inherited_min_scale: 1.0,
        enter: entrance(op),
        ..Work::default()
    };
    let name = super::object::visual(op)
        .expect("only visual operations get work metadata")
        .name;
    let fail = |field, code| StepError::in_step(&step.id, op_path(index, name).field(field), code);
    match op {
        Op::Line(r) | Op::Arrow(r) => {
            if r.via.len() + 2 > MAX_PATH_POINTS {
                return Err(fail(
                    "via",
                    ErrorCode::OverBudget(crate::Budget::PathPoints),
                ));
            }
            line_geometry(
                &mut work,
                r,
                if matches!(op, Op::Arrow(_)) {
                    crate::record::Head::End
                } else {
                    crate::record::Head::None
                },
            );
        }
        Op::Path(r) => {
            if let Some(at) = &r.at {
                work.offset = work.offset.max(anchor_offset(at));
            }
            let metrics =
                path::walk(r.d.as_str(), |_, _| Ok(())).map_err(|code| fail("d", code))?;
            work.scale = r.scale.map_or(1.0, Finite::get);
            work.extent = metrics.extent;
            work.path = Some(metrics);
        }
        Op::Shape(r) => shape_geometry(&mut work, r).map_err(|code| fail("box", code))?,
        Op::Text(r) => work.offset = anchor_offset(&r.at),
        Op::Callout(r) => {
            work.offset = anchor_offset(&r.target);
            if let Some(at) = &r.at {
                work.offset = work.offset.max(anchor_offset(at));
            }
        }
        Op::Highlight(r) => work.offset = anchor_offset(&r.target),
        Op::Diagram(r) => {
            if let Some(b) = r.bounds {
                work.extent = rect_extent(b).map_err(|code| fail("box", code))?;
            }
        }
        Op::Table(r) => {
            work.columns = r.cols.len();
            if r.align.len() > r.cols.len() {
                return Err(fail("align", ErrorCode::OutOfRange));
            }
            if let Some(b) = r.bounds {
                work.extent = rect_extent(b).map_err(|code| fail("box", code))?;
            }
        }
        Op::Row(r) => {
            work.cells = r.cells.len();
        }
        Op::Chart(r) => {
            work.chart = Some(Chart {
                kind: r.kind,
                x: (&r.x).into(),
                y: (&r.y).into(),
                stacked: r.stacked,
            });
            if let Some(b) = r.bounds {
                work.extent = rect_extent(b).map_err(|code| fail("box", code))?;
            }
        }
        Op::Series(r) => {
            work.chart_owner.clone_from(&r.parent);
            work.data = Some(Data::Put(index));
            work.usage.marks = r.data.len();
        }
        Op::Node(_) | Op::Edge(_) => {}
        Op::Anim(_) | Op::Mermaid(_) => unreachable!("only visual operations get work metadata"),
    }
    Ok(work)
}

fn fixed_segments(op: &Op) -> usize {
    match op {
        Op::Callout(_) => 1,
        Op::Highlight(r) => match r.style {
            crate::record::HighlightStyle::Marker => 0,
            crate::record::HighlightStyle::Underline => 1,
            _ => path::ROUND_RECT_SEGMENTS,
        },
        Op::Table(r) => r.cols.len() + 6,
        Op::Chart(r) => {
            if r.kind == ChartKind::Pie {
                0
            } else {
                path::CHART_AXES_SEGMENTS
            }
        }
        Op::Node(r) => match r.shape {
            crate::record::NodeShape::Diamond => 4,
            crate::record::NodeShape::Cylinder => path::CYLINDER_CAP_SEGMENTS,
            crate::record::NodeShape::Note => 7,
            _ => 0,
        },
        Op::Edge(_) => path::PROCEDURAL_CURVE_PIECES + 3,
        _ => 0,
    }
}

fn anchor_offset(anchor: &Anchor) -> f32 {
    match anchor {
        Anchor::At(p) => p[0].get().abs().max(p[1].get().abs()),
        Anchor::Ref(_) => 0.0,
    }
}

fn line_geometry(work: &mut Work, r: &crate::record::Line, head: crate::record::Head) {
    work.offset = anchor_offset(&r.from).max(anchor_offset(&r.to));
    work.extent = work.offset;
    work.offset = 0.0;
    work.usage.segments = r.via.len() + 1;
    if r.via.is_empty() && r.bend.is_some_and(|bend| bend.get().abs() > 0.001) {
        work.usage.segments = path::PROCEDURAL_CURVE_PIECES;
        if let (Anchor::At(a), Anchor::At(b)) = (&r.from, &r.to) {
            let bend = r.bend.expect("a nonzero bend was checked").get() * 0.5;
            let x = a[0].get().midpoint(b[0].get()) - (b[1].get() - a[1].get()) * bend;
            let y = a[1].get().midpoint(b[1].get()) + (b[0].get() - a[0].get()) * bend;
            work.extent = work.extent.max(x.abs()).max(y.abs());
        }
    }
    work.usage.segments += match r.head.unwrap_or(head) {
        crate::record::Head::None => 0,
        crate::record::Head::Both => 6,
        crate::record::Head::Start | crate::record::Head::End => 3,
    };
    for p in &*r.via {
        work.extent = work.extent.max(p[0].get().abs()).max(p[1].get().abs());
    }
}

fn shape_geometry(work: &mut Work, r: &crate::record::Shape) -> Result<(), ErrorCode> {
    if let Some(b) = r.bounds {
        work.extent = work.extent.max(rect_extent(b)?);
    } else {
        if let Some(at) = &r.at {
            work.offset = anchor_offset(at);
        }
        if let Some(size) = r.size {
            work.extent = size[0].get().max(size[1].get()) * 0.5;
        } else if let Some(radius) = r.r {
            work.extent = radius.get();
        }
    }
    for p in &*r.points {
        work.extent = work.extent.max(p[0].get().abs()).max(p[1].get().abs());
    }
    work.usage.segments = match r.kind {
        crate::record::ShapeKind::Polygon => r.points.len(),
        crate::record::ShapeKind::Rect => path::ROUND_RECT_SEGMENTS,
        crate::record::ShapeKind::Ellipse | crate::record::ShapeKind::Circle => {
            path::ELLIPSE_SEGMENTS
        }
    };
    Ok(())
}

fn rect_extent(bounds: [Finite; 4]) -> Result<f32, ErrorCode> {
    let extent = bounds[0]
        .get()
        .abs()
        .max(bounds[1].get().abs())
        .max((bounds[0].get() + bounds[2].get()).abs())
        .max((bounds[1].get() + bounds[3].get()).abs());
    if extent > crate::limits::MAX_COORD {
        return Err(ErrorCode::OutOfRange);
    }
    Ok(extent)
}

pub(crate) fn entrance(op: &Op) -> Option<Enter> {
    match op {
        Op::Line(r) | Op::Arrow(r) => r.enter,
        Op::Path(r) => r.enter,
        Op::Shape(r) => r.enter,
        Op::Text(r) => r.enter,
        Op::Callout(r) => r.enter,
        Op::Highlight(r) => r.enter,
        Op::Diagram(r) => r.enter,
        Op::Node(r) => r.enter,
        Op::Edge(r) => r.enter,
        Op::Table(r) => r.enter,
        Op::Row(r) => r.enter,
        Op::Chart(r) => r.enter,
        Op::Series(r) => r.enter,
        Op::Anim(_) | Op::Mermaid(_) => None,
    }
}

pub(crate) fn property(track: Track) -> &'static str {
    match track {
        Track::X => "x",
        Track::Y => "y",
        Track::Pos => "pos",
        Track::Opacity => "opacity",
        Track::Scale => "scale",
        Track::Rotate => "rotate",
        Track::Draw => "draw",
        Track::Color => "color",
    }
}

pub(crate) fn motion(anim: &Anim, index: usize, step: &Step) -> Result<Motion, StepError> {
    let fail =
        |field, code| StepError::in_step(&step.id, op_path(index, "anim").field(field), code);
    let period = anim
        .keys
        .last()
        .expect("nonempty keys were validated")
        .0
        .get();
    if period == 0 && anim.repeat != 0 {
        return Err(fail("repeat", ErrorCode::OutOfRange));
    }
    if anim.repeat > i32::try_from(MAX_MOTION_MS - 1).expect("motion cap fits i32") {
        return Err(fail("repeat", ErrorCode::OutOfRange));
    }
    if anim.repeat >= 0 {
        let plays = u32::try_from(anim.repeat).expect("repeat is nonnegative") + 1;
        if period
            .checked_mul(plays)
            .and_then(|ms| ms.checked_add(anim.delay.unwrap_or_default().get()))
            .is_none_or(|ms| ms > MAX_MOTION_MS)
        {
            return Err(fail("repeat", ErrorCode::OutOfRange));
        }
    }
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    for (_, value) in &*anim.keys {
        let value = key_vector(value);
        for axis in 0..2 {
            min[axis] = min[axis].min(value[axis]);
            max[axis] = max[axis].max(value[axis]);
        }
    }
    if anim.ease == Ease::Spring {
        for pair in anim.keys.windows(2) {
            let first = key_vector(&pair[0].1);
            let last = key_vector(&pair[1].1);
            for axis in 0..2 {
                let progress = 1.0 + crate::motion::SPRING_OVERSHOOT;
                let end = if anim.track == Track::Scale && axis == 0 {
                    crate::motion::interpolate_scale(first[axis], last[axis], progress)
                } else {
                    last[axis] + (last[axis] - first[axis]) * crate::motion::SPRING_OVERSHOOT
                };
                min[axis] = min[axis].min(end);
                max[axis] = max[axis].max(end);
            }
        }
    }
    if anim.track == Track::Scale && min[0] <= 0.0 {
        return Err(fail("keys", ErrorCode::OutOfRange));
    }
    Ok(Motion {
        keys: anim.keys.len(),
        min,
        max,
    })
}

fn key_vector(value: &KeyValue) -> [f32; 2] {
    match value {
        KeyValue::Num(value) => [value.get(), 0.0],
        KeyValue::V2(point) => [point[0].get(), point[1].get()],
        KeyValue::Color(_) => [0.0; 2],
    }
}

/// Copies a pre-step object once, keeping the partial map's attribution current at every
/// early return or unwind; `held` caches the copies' owned bytes.
fn keep(
    original: &mut Deferred<BTreeMap<WireId, Object>>,
    held: &mut usize,
    id: &WireId,
    object: &Object,
) {
    if !original.contains_key(id) {
        let copy = object.clone();
        *held += retire::owned((id, &copy));
        original.insert(id.clone(), copy);
        retire::attribute(original, *held);
    }
}

/// Applies tracks and data changes to `objects`, whose cached `owned` bytes follow their
/// motion maps and series data, and returns the pre-step copies of earlier objects they touched. Both maps stay
/// attributed as `retire::map_bytes` would.
pub(super) fn changes(
    step: &Step,
    objects: &mut Deferred<BTreeMap<WireId, Object>>,
    owned: &mut usize,
    revision: u32,
) -> Result<Deferred<BTreeMap<WireId, Object>>, StepError> {
    if step
        .put
        .iter()
        .filter(|op| matches!(op, Op::Anim(_)))
        .count()
        > crate::limits::MAX_TRACKS
    {
        return Err(StepError::in_step(
            &step.id,
            FieldPath::default().field("put"),
            ErrorCode::OverBudget(crate::Budget::Tracks),
        ));
    }
    let mut original = Deferred::new(BTreeMap::new(), 0);
    let mut held = 0;
    let mut owners = BTreeMap::<(WireId, &'static str), Motion>::new();
    for (index, op) in step.put.iter().enumerate() {
        if let Op::Anim(anim) = op {
            let object = objects
                .get_mut(&anim.target)
                .expect("track targets were validated");
            if object.handle.revision() != revision {
                keep(&mut original, &mut held, &anim.target, object);
            }
            let value = motion(anim, index, step)?;
            if owners
                .insert((anim.target.clone(), property(anim.track)), value)
                .is_some()
            {
                return Err(StepError::in_step(
                    &step.id,
                    op_path(index, "anim").field("track"),
                    ErrorCode::OutOfRange,
                ));
            }
        }
    }
    for ((target, property), value) in owners {
        let motions = &mut objects
            .get_mut(&target)
            .expect("track targets remain present")
            .work
            .motion;
        let before = heap::btree::<&str, Motion>(motions.len());
        motions.insert(property, value);
        *owned += heap::btree::<&str, Motion>(motions.len()) - before;
    }
    for (index, change) in step.set.iter().enumerate() {
        let object = objects
            .get_mut(&change.target)
            .expect("change targets were validated");
        if object.handle.revision() != revision && (change.cells.is_some() || change.data.is_some())
        {
            keep(&mut original, &mut held, &change.target, object);
        }
        if let Some(cells) = &change.cells {
            object.work.cells = cells.len();
        }
        if let Some(data) = &change.data {
            *owned -= retire::owned((&change.target, &*object));
            object.work.data = Some(Data::Set(index));
            object.work.usage.marks = data.len();
            *owned += retire::owned((&change.target, &*object));
        }
    }
    retire::attribute(objects, *owned);
    Ok(original)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_scale_shrink_has_a_positive_geometric_envelope() {
        let step: Step = serde_json::from_str(
            r#"{"id":"shrink","put":[{"anim":{"target":"t","track":"scale","ease":"spring","keys":[[0,1],[100,0.2]]}}]}"#,
        ).expect("step");
        let Op::Anim(anim) = &step.put[0] else {
            panic!("anim fixture")
        };
        let envelope = motion(anim, 0, &step).expect("positive scale envelope");
        assert!((envelope.min[0] - 0.109_216_23).abs() < 1e-6);
        assert_eq!(envelope.max[0].to_bits(), 1.0_f32.to_bits());
    }
}
