use crate::{
    ErrorCode, FieldPath, Finite, Handle, Ms, Step, StepError, WireId,
    record::{Anchor, Change, KeyValue, Op, Track},
};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Line,
    Path,
    Shape,
    Text,
    Callout,
    Highlight,
    Diagram,
    Node,
    Edge,
    Table,
    Row,
    Chart,
    Series,
}
impl Kind {
    fn container(self) -> bool {
        matches!(self, Self::Diagram | Self::Table | Self::Chart)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Object {
    pub work: super::work::Work,
    pub handle: Handle,
    pub kind: Kind,
    pub parent: Option<WireId>,
    pub references: Vec<WireId>,
}

pub(crate) struct Visual<'a> {
    pub id: &'a WireId,
    pub kind: Kind,
    pub name: &'static str,
    pub parent: Option<&'a WireId>,
    pub opacity: Option<Finite>,
    pub delay: Option<Ms>,
    pub dur: Option<Ms>,
}

pub(crate) fn visual(op: &Op) -> Option<Visual<'_>> {
    macro_rules! visual {
        ($r:expr, $kind:ident, $name:literal) => {
            Some(Visual {
                id: &$r.id,
                kind: Kind::$kind,
                name: $name,
                parent: $r.parent.as_ref(),
                opacity: $r.opacity,
                delay: $r.delay,
                dur: $r.dur,
            })
        };
    }
    match op {
        Op::Line(r) => visual!(r, Line, "line"),
        Op::Arrow(r) => visual!(r, Line, "arrow"),
        Op::Path(r) => visual!(r, Path, "path"),
        Op::Shape(r) => visual!(r, Shape, "shape"),
        Op::Text(r) => visual!(r, Text, "text"),
        Op::Callout(r) => visual!(r, Callout, "callout"),
        Op::Highlight(r) => visual!(r, Highlight, "highlight"),
        Op::Diagram(r) => visual!(r, Diagram, "diagram"),
        Op::Node(r) => visual!(r, Node, "node"),
        Op::Edge(r) => visual!(r, Edge, "edge"),
        Op::Table(r) => visual!(r, Table, "table"),
        Op::Row(r) => visual!(r, Row, "row"),
        Op::Chart(r) => visual!(r, Chart, "chart"),
        Op::Series(r) => visual!(r, Series, "series"),
        Op::Anim(_) | Op::Mermaid(_) => None,
    }
}

struct Reference<'a> {
    id: &'a WireId,
    field: &'static str,
    anchor: bool,
}
fn references<'a>(op: &'a Op) -> Vec<Reference<'a>> {
    let mut refs = Vec::new();
    if let Some(parent) = visual(op).and_then(|v| v.parent) {
        refs.push(Reference {
            id: parent,
            field: "in",
            anchor: false,
        });
    }
    let mut anchor = |value: &'a Anchor, field| {
        if let Anchor::Ref(id) = value {
            refs.push(Reference {
                id,
                field,
                anchor: true,
            });
        }
    };
    match op {
        Op::Line(r) | Op::Arrow(r) => {
            anchor(&r.from, "from");
            anchor(&r.to, "to");
        }
        Op::Path(r) => {
            if let Some(at) = &r.at {
                anchor(at, "at");
            }
        }
        Op::Shape(r) => {
            if let Some(at) = &r.at {
                anchor(at, "at");
            }
        }
        Op::Text(r) => anchor(&r.at, "at"),
        Op::Callout(r) => {
            anchor(&r.target, "target");
            if let Some(at) = &r.at {
                anchor(at, "at");
            }
        }
        Op::Highlight(r) => anchor(&r.target, "target"),
        Op::Edge(r) => {
            refs.push(Reference {
                id: &r.from,
                field: "from",
                anchor: false,
            });
            refs.push(Reference {
                id: &r.to,
                field: "to",
                anchor: false,
            });
        }
        _ => {}
    }
    refs
}

pub(crate) fn reference_ids(op: &Op) -> Vec<WireId> {
    references(op)
        .into_iter()
        .map(|reference| reference.id.clone())
        .collect()
}

pub(super) fn object(
    v: &Visual<'_>,
    op: &Op,
    handle: Handle,
    index: usize,
    step: &Step,
) -> Result<Object, StepError> {
    Ok(Object {
        work: super::work::make(op, index, step)?,
        handle,
        kind: v.kind,
        parent: v.parent.cloned(),
        references: reference_ids(op),
    })
}

pub(super) fn path(index: usize, name: &'static str) -> FieldPath {
    FieldPath::default().field("put").index(index).field(name)
}

pub(super) fn validate_op(
    op: &Op,
    index: usize,
    step: &Step,
    objects: &BTreeMap<WireId, Object>,
    removed: &HashSet<&WireId>,
) -> Result<(), StepError> {
    if let Op::Anim(anim) = op {
        return validate_anim(anim, index, step, objects, removed);
    }
    let Some(v) = visual(op) else {
        return Err(StepError::in_step(
            &step.id,
            path(index, "mermaid"),
            ErrorCode::Unsupported,
        ));
    };
    let prefix = path(index, v.name);
    let fail = |field, code| StepError::in_step(&step.id, prefix.clone().field(field), code);
    if v.opacity
        .is_some_and(|value| !(0.0..=1.0).contains(&value.get()))
    {
        return Err(fail("opacity", ErrorCode::OutOfRange));
    }
    if let Some(dur) = v.dur
        && v.delay.unwrap_or_default().checked_add(dur).is_none()
    {
        return Err(fail("dur", ErrorCode::OutOfRange));
    }
    let required_parent = match v.kind {
        Kind::Node | Kind::Edge => Some(Kind::Diagram),
        Kind::Row => Some(Kind::Table),
        Kind::Series => Some(Kind::Chart),
        _ => None,
    };
    if required_parent.is_some() && v.parent.is_none() {
        return Err(fail("in", ErrorCode::MissingField));
    }
    for reference in references(op) {
        let mut field = prefix.clone().field(reference.field);
        if reference.anchor {
            field = field.field("ref");
        }
        let Some(target) = objects.get(reference.id) else {
            return Err(StepError::in_step(&step.id, field, ErrorCode::UnknownRef));
        };
        let valid = if reference.field == "in" {
            required_parent.map_or_else(|| target.kind.container(), |kind| target.kind == kind)
        } else if v.kind == Kind::Edge {
            target.kind == Kind::Node && target.parent.as_ref() == v.parent
        } else {
            true
        };
        if !valid {
            return Err(StepError::in_step(&step.id, field, ErrorCode::OutOfRange));
        }
    }
    validate_dimensions(op, &prefix, step)
}

fn validate_dimensions(op: &Op, prefix: &FieldPath, step: &Step) -> Result<(), StepError> {
    let positive = |value: Option<Finite>, field| {
        if value.is_some_and(|v| v.get() <= 0.0) {
            Err(StepError::in_step(
                &step.id,
                prefix.clone().field(field),
                ErrorCode::OutOfRange,
            ))
        } else {
            Ok(())
        }
    };
    let bounds = |value: Option<[Finite; 4]>| {
        if let Some(value) = value {
            for index in [2, 3] {
                if value[index].get() <= 0.0 {
                    return Err(StepError::in_step(
                        &step.id,
                        prefix.clone().field("box").index(index),
                        ErrorCode::OutOfRange,
                    ));
                }
            }
        }
        Ok(())
    };
    match op {
        Op::Line(r) | Op::Arrow(r) => positive(r.width, "width")?,
        Op::Path(r) => {
            positive(r.width, "width")?;
            positive(r.scale, "scale")?;
        }
        Op::Shape(r) => {
            bounds(r.bounds)?;
            for (field, value) in [("r", r.r), ("radius", r.radius), ("width", r.width)] {
                positive(value, field)?;
            }
            if let Some(size) = r.size {
                for (index, value) in size.into_iter().enumerate() {
                    if value.get() <= 0.0 {
                        return Err(StepError::in_step(
                            &step.id,
                            prefix.clone().field("size").index(index),
                            ErrorCode::OutOfRange,
                        ));
                    }
                }
            }
        }
        Op::Text(r) => {
            positive(r.size, "size")?;
            positive(r.max_width, "max_width")?;
        }
        Op::Callout(r) => positive(r.max_width, "max_width")?,
        Op::Highlight(r) => positive(r.pad, "pad")?,
        Op::Diagram(r) => bounds(r.bounds)?,
        Op::Table(r) => bounds(r.bounds)?,
        Op::Chart(r) => {
            bounds(r.bounds)?;
            if r.title.is_some()
                && let Some(bounds) = r.bounds
            {
                for (axis, origin) in bounds[..2].iter().enumerate() {
                    if origin.get() + crate::limits::CHART_LABEL_INSET > crate::limits::MAX_COORD {
                        return Err(StepError::in_step(
                            &step.id,
                            prefix.clone().field("box").index(axis),
                            ErrorCode::OutOfRange,
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn validate_changes(
    step: &Step,
    objects: &BTreeMap<WireId, Object>,
    removed: &HashSet<&WireId>,
) -> Result<(), StepError> {
    for (index, change) in step.set.iter().enumerate() {
        let prefix = FieldPath::default().field("set").index(index);
        let Some(target) = objects.get(&change.target) else {
            return Err(StepError::in_step(
                &step.id,
                prefix.field("target"),
                ErrorCode::UnknownRef,
            ));
        };
        validate_change(change, target.kind, &prefix, step)?;
        if removed.contains(&change.target) {
            return Err(StepError::in_step(
                &step.id,
                prefix.field("target"),
                ErrorCode::UnknownRef,
            ));
        }
    }
    Ok(())
}
fn validate_change(
    change: &Change,
    kind: Kind,
    prefix: &FieldPath,
    step: &Step,
) -> Result<(), StepError> {
    for (present, owned, field) in [
        (
            change.text.is_some(),
            matches!(kind, Kind::Text | Kind::Callout),
            "text",
        ),
        (
            change.label.is_some(),
            matches!(kind, Kind::Shape | Kind::Node | Kind::Edge),
            "label",
        ),
        (change.cells.is_some(), kind == Kind::Row, "cells"),
        (change.data.is_some(), kind == Kind::Series, "data"),
        (change.emphasis.is_some(), kind == Kind::Row, "emphasis"),
    ] {
        if present && !owned {
            return Err(StepError::in_step(
                &step.id,
                prefix.clone().field(field),
                ErrorCode::KindChanged,
            ));
        }
    }
    Ok(())
}

fn validate_anim(
    anim: &crate::record::Anim,
    index: usize,
    step: &Step,
    objects: &BTreeMap<WireId, Object>,
    removed: &HashSet<&WireId>,
) -> Result<(), StepError> {
    let prefix = path(index, "anim");
    let fail = |field, code| StepError::in_step(&step.id, prefix.clone().field(field), code);
    if removed.contains(&anim.target) {
        return Err(fail("target", ErrorCode::UnknownRef));
    }
    let Some(target) = objects.get(&anim.target) else {
        return Err(fail("target", ErrorCode::UnknownRef));
    };
    if anim.track == Track::Draw && !matches!(target.kind, Kind::Line | Kind::Path | Kind::Edge) {
        return Err(fail("track", ErrorCode::PropertyNotOwned));
    }
    if anim.keys.is_empty() || anim.repeat < -1 {
        return Err(fail(
            if anim.keys.is_empty() {
                "keys"
            } else {
                "repeat"
            },
            ErrorCode::OutOfRange,
        ));
    }
    let mut previous = None;
    for (key_index, (time, value)) in anim.keys.iter().enumerate() {
        let path = prefix.clone().field("keys").index(key_index);
        if previous.is_some_and(|prior| *time <= prior)
            || anim.delay.unwrap_or_default().checked_add(*time).is_none()
        {
            return Err(StepError::in_step(
                &step.id,
                path.index(0),
                ErrorCode::OutOfRange,
            ));
        }
        previous = Some(*time);
        let valid = match (anim.track, value) {
            (Track::Pos, KeyValue::V2(_))
            | (Track::Color, KeyValue::Color(_))
            | (Track::X | Track::Y | Track::Rotate, KeyValue::Num(_)) => true,
            (Track::Opacity | Track::Draw, KeyValue::Num(v)) => (0.0..=1.0).contains(&v.get()),
            (Track::Scale, KeyValue::Num(v)) => v.get() > 0.0,
            _ => false,
        };
        if !valid {
            return Err(StepError::in_step(
                &step.id,
                path.index(1),
                ErrorCode::OutOfRange,
            ));
        }
    }
    Ok(())
}

pub(super) fn acyclic(step: &Step, objects: &BTreeMap<WireId, Object>) -> Result<(), StepError> {
    let mut remaining: BTreeMap<&WireId, usize> = objects
        .iter()
        .map(|(id, o)| (id, o.references.len()))
        .collect();
    let mut dependants: BTreeMap<&WireId, Vec<&WireId>> = BTreeMap::new();
    let mut ready = Vec::new();
    for (id, object) in objects {
        if object.references.is_empty() {
            ready.push(id);
        }
        for target in &object.references {
            dependants.entry(target).or_default().push(id);
        }
    }
    let mut count = 0;
    while let Some(id) = ready.pop() {
        count += 1;
        for dependant in dependants.get(id).into_iter().flatten() {
            let pending = remaining
                .get_mut(dependant)
                .expect("every dependant is a graph object");
            *pending -= 1;
            if *pending == 0 {
                ready.push(*dependant);
            }
        }
    }
    if count != objects.len() {
        let index = step
            .put
            .iter()
            .enumerate()
            .find_map(|(i, op)| {
                visual(op)
                    .filter(|v| remaining.get(v.id).is_some_and(|count| *count > 0))
                    .map(|_| i)
            })
            .unwrap_or_default();
        return Err(StepError::in_step(
            &step.id,
            FieldPath::default().field("put").index(index),
            ErrorCode::ReferenceCycle,
        ));
    }
    Ok(())
}

pub(super) fn removals(
    step: &Step,
    objects: &mut BTreeMap<WireId, Object>,
    removed: &HashSet<&WireId>,
) -> Result<Vec<(WireId, Object)>, StepError> {
    if removed.is_empty() {
        return Ok(Vec::new());
    }
    for (index, id) in step.remove.iter().enumerate() {
        let field = FieldPath::default().field("remove").index(index);
        if !objects.contains_key(id) {
            return Err(StepError::in_step(&step.id, field, ErrorCode::UnknownRef));
        }
    }
    let mut needed = HashSet::new();
    for (owner, object) in &*objects {
        if !removed.contains(owner) {
            needed.extend(object.references.iter());
        }
    }
    for (index, id) in step.remove.iter().enumerate() {
        if needed.contains(id) {
            return Err(StepError::in_step(
                &step.id,
                FieldPath::default().field("remove").index(index),
                ErrorCode::Orphans,
            ));
        }
    }
    Ok(step
        .remove
        .iter()
        .map(|id| {
            (
                id.clone(),
                objects
                    .remove(id)
                    .expect("all removals were checked before mutation"),
            )
        })
        .collect())
}
