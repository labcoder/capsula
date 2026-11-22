mod axis;

use super::{
    PrepareError,
    primitives::{Build, invariant},
    state::{LayoutState, Placed},
};
use crate::{
    Handle, Op, Text, WireId,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    record::{Align, ChartKind, Point, Series},
};
use axis::{Domain, header};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    sync::Arc,
};

fn count(value: usize) -> u16 {
    u16::try_from(value).expect("admitted chart counts fit in u16")
}
#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "only normalized fractions become canvas floats"
)]
fn fraction(value: f64) -> f32 {
    value.clamp(0.0, 1.0) as f32
}
fn point(value: &Point, index: usize) -> (f64, f64) {
    match value {
        Point::Value(y) => (f64::from(count(index)), y.get()),
        Point::Pair(p) => (p[0].get(), p[1].get()),
    }
}

struct Plot {
    frame: Rect,
    kind: ChartKind,
    x: Domain,
    y: Domain,
    categories: Vec<Text>,
}
impl Plot {
    fn payload_bytes(&self) -> usize {
        super::heap::ARC_HEADER
            + size_of::<Self>()
            + self.categories.len() * size_of::<Text>()
            + self
                .categories
                .iter()
                .map(|text| text.as_str().len())
                .sum::<usize>()
    }
    fn at(&self, x: f64, y: f64) -> Vec2 {
        Vec2::new(
            self.frame.origin.x + self.x.fraction(x) * self.frame.size.x,
            self.frame.max().y - self.y.fraction(y) * self.frame.size.y,
        )
    }
}
pub(super) struct Basis {
    plot: Arc<Plot>,
    palette: usize,
    group: [f32; 2],
    baselines: Box<[[f64; 2]]>,
    pie: Option<[f64; 2]>,
}
impl Basis {
    fn bytes(&self) -> usize {
        super::heap::ARC_HEADER + size_of::<Self>() + self.baselines.len() * size_of::<[f64; 2]>()
    }
}
impl fmt::Debug for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChartBasis { .. }")
    }
}
/// Bytes the distinct chart bases in `state` retain, each shared plot counted once.
pub(super) fn retained_bytes(state: &LayoutState) -> usize {
    let mut bases = HashSet::new();
    let mut plots = HashSet::new();
    let mut bytes = 0;
    for basis in state
        .objects
        .values()
        .filter_map(|object| object.chart_basis.as_ref())
    {
        if !bases.insert(Arc::as_ptr(basis)) {
            continue;
        }
        bytes += basis.bytes();
        if plots.insert(Arc::as_ptr(&basis.plot)) {
            bytes += basis.plot.payload_bytes();
        }
    }
    bytes
}
struct Drawing {
    bounds: Rect,
    output: Vec<Prim>,
}
impl Drawing {
    fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            output: Vec::new(),
        }
    }
    fn rect(&mut self, rect: Rect, radius: f32, color: Paint) {
        self.bounds = self.bounds.union(rect);
        self.output.push(Prim::Rect {
            rect,
            radius,
            fill: Some(color),
            border: None,
        });
    }
    fn line(&mut self, points: Arc<[Vec2]>, color: Paint, width: f32) {
        self.output.push(Prim::Line {
            points,
            closed: false,
            color,
            width,
            dashed: false,
        });
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one measured text installation owns its style and alignment"
    )]
    fn label(
        &mut self,
        text: Text,
        origin: Vec2,
        width: f32,
        align: Align,
        style: TextStyle,
        color: Paint,
        build: &mut Build<'_>,
    ) -> Result<Vec2, PrepareError> {
        let width = width.max(1.0);
        let size = build.measure(&text, style, Some(width))?;
        let bounds = Rect {
            origin,
            size: Vec2::new(width.max(size.x), size.y),
        };
        if !bounds.within_canvas() {
            return Err(crate::ErrorCode::OutOfRange.into());
        }
        self.bounds = self.bounds.union(bounds);
        self.output.push(Prim::Text {
            origin,
            text,
            style,
            color,
            max_width: Some(width),
            align,
        });
        Ok(size)
    }
}
fn color(series: &Series, palette: usize, build: &Build<'_>) -> Paint {
    // shortcut: the theme palette wraps after 8 series, add colors when charts need more.
    series
        .color
        .as_ref()
        .map_or_else(|| build.theme.series(palette), |c| build.theme.resolve(c))
        .into()
}

fn pie_label(category: Option<&Text>, share: f64) -> Result<Text, PrepareError> {
    let percent = format!("{:.0}%", share * 100.0);
    let Some(category) = category else {
        return Ok(percent.try_into()?);
    };
    let label = format!("{} {percent}", category.as_str());
    if label.chars().count() > crate::limits::MAX_TEXT_CHARS {
        return Ok(category.clone());
    }
    Ok(label.try_into()?)
}
fn pie_total(series: &Series, scale: f64) -> f64 {
    if scale <= 0.0 {
        return 0.0;
    }
    series
        .data
        .iter()
        .enumerate()
        .map(|(i, p)| point(p, i).1 / scale)
        .sum()
}
#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "trigonometric unit vectors are bounded canvas floats"
)]
fn direction(angle: f64) -> Vec2 {
    Vec2::new(angle.cos() as f32, angle.sin() as f32)
}
#[expect(
    clippy::too_many_lines,
    reason = "attached and frozen series share exactly one drawing path per kind"
)]
fn draw(series: &Series, basis: &Basis, build: &mut Build<'_>) -> Result<Drawing, PrepareError> {
    build.check_cancel()?;
    let plot = &basis.plot;
    let mut out = Drawing::new(plot.frame);
    if plot.frame.size.x < 1.0 || plot.frame.size.y < 1.0 {
        return Ok(out);
    }
    let paint = color(series, basis.palette, build);
    match plot.kind {
        ChartKind::Bar => {
            for (i, p) in series.data.iter().enumerate() {
                build.check_cancel()?;
                let (x, y) = point(p, i);
                let base = basis
                    .baselines
                    .get(i)
                    .map_or(0.0, |b| b[usize::from(y < 0.0)]);
                let a = plot.at(x, base);
                let b = plot.at(x, base + y);
                let left = (a.x + basis.group[0]).clamp(plot.frame.origin.x, plot.frame.max().x);
                let right = (a.x + basis.group[0] + basis.group[1])
                    .clamp(plot.frame.origin.x, plot.frame.max().x);
                out.rect(
                    Rect {
                        origin: Vec2::new(left, a.y.min(b.y)),
                        size: Vec2::new(right - left, (a.y - b.y).abs()),
                    },
                    2.0,
                    paint,
                );
            }
        }
        ChartKind::Line | ChartKind::Area | ChartKind::Scatter => {
            let mut points = Vec::with_capacity(series.data.len() + 2);
            for (i, p) in series.data.iter().enumerate() {
                build.check_cancel()?;
                let (x, y) = point(p, i);
                let at = plot.at(x, y);
                points.push(at);
                if plot.kind == ChartKind::Scatter || series.data.len() < 40 {
                    let radius = if plot.kind == ChartKind::Scatter {
                        4.0
                    } else {
                        3.0
                    };
                    out.rect(
                        Rect::centered(at, Vec2::new(radius * 2.0, radius * 2.0)),
                        radius,
                        paint,
                    );
                }
            }
            if plot.kind != ChartKind::Scatter && points.len() >= 2 {
                if plot.kind == ChartKind::Area {
                    let baseline = plot.at(plot.x.value(plot.x.min), 0.0).y;
                    let mut fill = Vec::with_capacity(points.len() + 2);
                    fill.push(Vec2::new(points[0].x, baseline));
                    fill.extend_from_slice(&points);
                    fill.push(Vec2::new(points[points.len() - 1].x, baseline));
                    out.output.push(Prim::Fill {
                        points: fill.into(),
                        color: paint.faded(0.25),
                    });
                }
                out.line(points.into(), paint, 2.0);
            }
        }
        ChartKind::Pie => {
            let scale = series
                .data
                .iter()
                .enumerate()
                .map(|(i, p)| point(p, i).1)
                .fold(0.0, f64::max);
            let total = pie_total(series, scale);
            if total <= 0.0 {
                return Ok(out);
            }
            let center = plot.frame.center();
            let radius = plot.frame.size.x.min(plot.frame.size.y) * 0.46;
            // shortcut: a newly positive detached pie fills the circle, until allocation changes.
            let [mut angle, span] = basis
                .pie
                .unwrap_or([-std::f64::consts::FRAC_PI_2, std::f64::consts::TAU]);
            for (i, p) in series.data.iter().enumerate() {
                build.check_cancel()?;
                let share = (point(p, i).1 / scale) / total;
                let end = angle + span * share;
                if end > angle {
                    let mut points = Vec::new();
                    points.push(center);
                    for s in 0..=crate::path::PIE_ARC_SEGMENTS {
                        let a = angle
                            + (end - angle) * f64::from(count(s))
                                / f64::from(count(crate::path::PIE_ARC_SEGMENTS));
                        points.push(center + direction(a) * radius);
                    }
                    out.output.push(Prim::Fill {
                        points: points.into(),
                        color: if i == 0 {
                            paint
                        } else {
                            build.theme.series(basis.palette + i).into()
                        },
                    });
                    let global_share = share * (span / std::f64::consts::TAU);
                    if global_share >= 0.045 {
                        let middle = angle.midpoint(end);
                        let style = TextStyle {
                            size: build.theme.font_size * 0.8,
                            bold: false,
                        };
                        let width = (radius * 0.9).max(1.0);
                        let origin = center + direction(middle) * (radius * 0.64)
                            - Vec2::new(width * 0.5, style.size * 0.65);
                        out.label(
                            pie_label(plot.categories.get(i), global_share)?,
                            origin,
                            width,
                            Align::Center,
                            style,
                            build.theme.fg.into(),
                            build,
                        )?;
                    }
                }
                angle = end;
            }
        }
    }
    Ok(out)
}

fn install(
    state: &mut LayoutState,
    id: WireId,
    source: Arc<Op>,
    handle: Handle,
    parent: Option<Handle>,
    drawing: Drawing,
    basis: Option<Arc<Basis>>,
) {
    let old = state.objects.get(&id);
    let opacity = crate::state::object::visual(&source)
        .and_then(|v| v.opacity)
        .map_or(1.0, crate::Finite::get);
    let mut placed = Placed::new(
        Item {
            handle,
            parent,
            bounds: drawing.bounds,
            primitives: drawing.output.into(),
            transform: Affine::default(),
            opacity,
        },
        source,
        old,
    );
    placed.chart_basis = basis;
    state.handles.insert(handle, id.clone());
    state.objects.insert(id, placed);
}
pub(super) fn place(
    sources: &BTreeMap<WireId, (Handle, Arc<Op>)>,
    before: &LayoutState,
    state: &mut LayoutState,
    removed: &[Handle],
    build: &mut Build<'_>,
) -> Result<usize, PrepareError> {
    let mut dirty = BTreeSet::new();
    for (id, (handle, source)) in sources {
        match &**source {
            Op::Chart(_) => {
                dirty.insert(*handle);
            }
            Op::Series(series) => {
                if let Some(basis) = state
                    .objects
                    .get(id)
                    .filter(|o| o.item.parent.is_none())
                    .and_then(|o| o.chart_basis.clone())
                {
                    // shortcut: clips to the frozen domain, until a standalone refit exists.
                    let drawing = draw(series, &basis, build)?;
                    install(
                        state,
                        id.clone(),
                        Arc::clone(source),
                        *handle,
                        None,
                        drawing,
                        Some(basis),
                    );
                } else {
                    dirty.insert(
                        series
                            .parent
                            .as_ref()
                            .and_then(|id| state.handle(id))
                            .ok_or_else(invariant)?,
                    );
                }
            }
            _ => {}
        }
    }
    for handle in removed {
        if let Some(old) = before
            .handles
            .get(handle)
            .and_then(|id| before.objects.get(id))
            && matches!(&*old.source, Op::Series(_))
            && let Some(parent) = old.item.parent
            && state.handles.contains_key(&parent)
        {
            dirty.insert(parent);
        }
    }
    let mut bytes = 0;
    for handle in dirty {
        bytes += reflow(sources, state, removed, handle, build)?;
    }
    Ok(bytes)
}
struct SeriesMembers {
    id: WireId,
    source: Arc<Op>,
    parent: Option<Handle>,
    members: BTreeMap<Handle, (WireId, Arc<Op>)>,
}
fn series_members(
    sources: &BTreeMap<WireId, (Handle, Arc<Op>)>,
    state: &LayoutState,
    removed: &[Handle],
    handle: Handle,
) -> Result<SeriesMembers, PrepareError> {
    let id = state.handles.get(&handle).ok_or_else(invariant)?.clone();
    let old = state.objects.get(&id).ok_or_else(invariant)?;
    let source = Arc::clone(sources.get(&id).map_or(&old.source, |(_, s)| s));
    let Op::Chart(chart) = &*source else {
        return Err(invariant());
    };
    let mut members: BTreeMap<_, _> = state
        .objects
        .iter()
        .filter(|(_, o)| {
            o.item.parent == Some(handle)
                && matches!(&*o.source, Op::Series(_))
                && !removed.contains(&o.item.handle)
        })
        .map(|(id, o)| (o.item.handle, (id.clone(), Arc::clone(&o.source))))
        .collect();
    for (id, (h, s)) in sources {
        if let Op::Series(value) = &**s
            && value.parent.as_ref() == Some(&chart.id)
            && !removed.contains(h)
            && state
                .objects
                .get(id)
                .is_none_or(|o| o.item.parent.is_some())
        {
            members.insert(*h, (id.clone(), Arc::clone(s)));
        }
    }
    Ok(SeriesMembers {
        id,
        source,
        parent: old.item.parent,
        members,
    })
}
fn reflow(
    sources: &BTreeMap<WireId, (Handle, Arc<Op>)>,
    state: &mut LayoutState,
    removed: &[Handle],
    handle: Handle,
    build: &mut Build<'_>,
) -> Result<usize, PrepareError> {
    build.check_cancel()?;
    let prefix = series_members(sources, state, removed, handle)?;
    let Op::Chart(chart) = &*prefix.source else {
        return Err(invariant());
    };
    let series: Vec<_> = prefix
        .members
        .values()
        .filter_map(|(_, s)| {
            if let Op::Series(s) = &**s {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    let (plot, drawing) = header(chart, &series, build)?;
    let n = series
        .iter()
        .map(|s| s.data.len())
        .max()
        .unwrap_or(0)
        .max(chart.categories.len())
        .max(1);
    // shortcut: grouped numeric-x bars assume even x spacing (scene.md); upgrade on uneven data.
    let group = plot.frame.size.x / f32::from(count(n)) * 0.76;
    let width = if chart.stacked {
        group
    } else {
        group / f32::from(count(series.len().max(1)))
    };
    let scale = if chart.kind == ChartKind::Pie {
        series
            .iter()
            .flat_map(|s| s.data.iter())
            .map(|p| point(p, 0).1)
            .fold(0.0, f64::max)
    } else {
        0.0
    };
    let total: f64 = series.iter().map(|s| pie_total(s, scale)).sum();
    let mut angle = -std::f64::consts::FRAC_PI_2;
    let mut stack_totals = vec![[0.0_f64; 2]; n];
    install(
        state,
        prefix.id,
        Arc::clone(&prefix.source),
        handle,
        prefix.parent,
        drawing,
        None,
    );
    if prefix.members.is_empty() {
        return Ok(0);
    }
    let mut bytes = plot.payload_bytes();
    let mut palette = 0;
    for (h, (id, source)) in prefix.members {
        let Op::Series(series) = &*source else {
            return Err(invariant());
        };
        let mut baselines = Vec::new();
        if chart.kind == ChartKind::Bar && chart.stacked {
            baselines.clone_from(&stack_totals);
            for (i, p) in series.data.iter().enumerate() {
                let y = point(p, i).1;
                stack_totals[i][usize::from(y < 0.0)] += y;
            }
        }
        let span = if total > 0.0 {
            std::f64::consts::TAU * (pie_total(series, scale) / total)
        } else {
            0.0
        };
        let basis = Arc::new(Basis {
            plot: Arc::clone(&plot),
            palette,
            group: [
                -group * 0.5
                    + if chart.stacked {
                        0.0
                    } else {
                        f32::from(count(palette)) * width
                    },
                (width - 1.0).max(0.1),
            ],
            baselines: baselines.into_boxed_slice(),
            pie: (chart.kind == ChartKind::Pie && series.data.iter().any(|p| point(p, 0).1 > 0.0))
                .then_some([angle, span]),
        });
        palette += if chart.kind == ChartKind::Pie {
            series.data.len()
        } else {
            1
        };
        angle += span;
        bytes += basis.bytes();
        let drawing = draw(series, &basis, build)?;
        install(state, id, source, h, Some(handle), drawing, Some(basis));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::axis::compact;
    use super::*;
    use crate::layout::{
        Glide, PrepareInput, PreparedStep, SurfaceGeometry, heap, prepare, state::Motions,
    };
    use crate::{
        Admission, GlobalBudget, Record, SceneState, StopCut,
        display::{MeasureError, TextMeasure},
        parse_record,
        record::Chart,
        state::work::Motion,
        text::Bounded,
        theme::Theme,
    };
    use std::sync::atomic::AtomicBool;
    pub(super) struct Measure;
    impl TextMeasure for Measure {
        fn measure(
            &mut self,
            text: &str,
            style: TextStyle,
            width: Option<f32>,
        ) -> Result<[f32; 2], MeasureError> {
            Ok([
                (f32::from(count(text.chars().count())) * 7.0).min(width.unwrap_or(10_000.0)),
                style.size * 1.3,
            ])
        }
    }
    fn series(data: &str) -> Series {
        serde_json::from_str(&format!(r#"{{"id":"s","data":{data}}}"#)).expect("series")
    }
    fn picture(
        scene: &mut SceneState,
        budget: &mut GlobalBudget,
        before: &LayoutState,
        source: &str,
    ) -> PreparedStep {
        let Record::Step(step) = parse_record(source.as_bytes()).expect("step") else {
            panic!("step");
        };
        let Admission::New(candidate) = scene.admit(&step, budget).expect("admit") else {
            panic!("new");
        };
        let ticket = scene.commit(candidate, budget).expect("commit").preparation;
        let input = PrepareInput::new(
            ticket,
            before.clone(),
            SurfaceGeometry::default(),
            Arc::new(Theme::default()),
        )
        .expect("input");
        prepare(input, &mut Measure, &AtomicBool::new(false)).expect("picture")
    }
    fn basis(kind: ChartKind) -> Basis {
        Basis {
            plot: Arc::new(Plot {
                frame: Rect {
                    origin: Vec2::default(),
                    size: Vec2::new(200.0, 100.0),
                },
                kind,
                x: Domain {
                    min: 0.0,
                    max: 2.0,
                    log: false,
                },
                y: Domain {
                    min: -10.0,
                    max: 10.0,
                    log: false,
                },
                categories: Vec::new(),
            }),
            palette: 0,
            group: [-10.0, 20.0],
            baselines: Box::new([]),
            pie: Some([-std::f64::consts::FRAC_PI_2, std::f64::consts::PI]),
        }
    }
    #[test]
    fn every_kind_draws_empty_single_and_huge_data() {
        let theme = Theme::default();
        let cancel = AtomicBool::new(false);
        let scales = BTreeMap::new();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &cancel,
            scales: &scales,
            screen: None,
        };
        for kind in [
            ChartKind::Bar,
            ChartKind::Line,
            ChartKind::Area,
            ChartKind::Scatter,
            ChartKind::Pie,
        ] {
            let basis = basis(kind);
            let mut check = |data, has_huge_value| {
                let out = draw(&series(data), &basis, &mut build).expect("draw");
                assert_eq!(
                    out.output.is_empty(),
                    data == "[]",
                    "{kind:?} {data}: only empty data has no drawing"
                );
                assert!(out.bounds.within_canvas(), "{kind:?} {data} bounds");
                if has_huge_value && kind != ChartKind::Pie {
                    assert!(
                        out.output.iter().any(|p| match p {
                            Prim::Rect { rect, .. } if kind == ChartKind::Bar =>
                                rect.size.y > 0.0 && rect.origin.y.abs() < 1e-5,
                            Prim::Rect { rect, .. } => rect.center().y.abs() < 1e-5,
                            Prim::Line { points, .. } | Prim::Fill { points, .. } =>
                                points.iter().any(|point| point.y.abs() < 1e-5),
                            Prim::Text { .. } => false,
                        }),
                        "{kind:?} {data}: the huge value projects to the top of the plot"
                    );
                }
            };
            check("[]", false);
            check("[1]", false);
            check("[0,1e300,2]", true);
            if kind != ChartKind::Pie {
                check("[-12,0,1e300]", true);
            }
        }
    }
    #[test]
    fn sign_changes_select_the_retained_negative_baseline() {
        let mut basis = basis(ChartKind::Bar);
        basis.baselines = vec![[5.0, -4.0]].into_boxed_slice();
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
            screen: None,
        };
        let out = draw(&series("[-2]"), &basis, &mut build).expect("draw");
        let Prim::Rect { rect, .. } = out.output[0] else {
            panic!("bar");
        };
        assert!(
            (rect.origin.y - 70.0).abs() < 1e-5 && (rect.size.y - 10.0).abs() < 1e-5,
            "negative stack occupies -4 to -6"
        );
    }
    #[test]
    fn category_at_limit_keeps_all_text_without_percent_suffix() {
        let category: Text = "x"
            .repeat(crate::limits::MAX_TEXT_CHARS)
            .try_into()
            .expect("category");
        assert_eq!(pie_label(Some(&category), 0.5).expect("label"), category);
        assert_eq!(compact(1.0, Some(&category)).expect("unit"), category);
    }
    #[test]
    fn stacks_include_every_series_and_pairs_use_numeric_x() {
        let a = series("[[10,1e300],[20,-1e300]]");
        let b = a.clone();
        let chart: Chart = serde_json::from_str(r#"{"id":"c","box":[0,0,400,300],"stacked":true}"#)
            .expect("chart");
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
            screen: None,
        };
        let (plot, _) = header(&chart, &[&a, &b], &mut build).expect("header");
        assert!(
            plot.y.min <= -2e300 && plot.y.max >= 2e300,
            "the domain includes both signed stack totals"
        );
        for (x, single, combined) in [(10.0, 1e300, 2e300), (20.0, -1e300, -2e300)] {
            assert!(
                (plot.at(x, single).y - plot.at(x, combined).y).abs() >= 1.0,
                "both series remain visually distinct from just one at x={x}"
            );
        }
        let left = plot.at(10.0, 0.0).x;
        let right = plot.at(20.0, 0.0).x;
        assert!(
            plot.frame.origin.x < left && right - left >= 1.0 && right < plot.frame.max().x,
            "numeric x values remain distinct and inside the plot"
        );
    }
    #[test]
    fn area_keeps_only_the_top_stroke_and_single_points_are_native_dots() {
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
            screen: None,
        };
        assert!(
            draw(&series("[]"), &basis(ChartKind::Area), &mut build)
                .expect("empty")
                .output
                .is_empty(),
            "empty area has no primitives"
        );
        let single = draw(&series("[1]"), &basis(ChartKind::Area), &mut build).expect("single");
        assert_eq!(single.output.len(), 1);
        assert!(
            matches!(single.output[0], Prim::Rect { .. }),
            "one point is a native dot"
        );
        let out = draw(&series("[1,2]"), &basis(ChartKind::Area), &mut build).expect("area");
        let lines: Vec<_> = out
            .output
            .iter()
            .filter_map(|p| {
                if let Prim::Line { points, closed, .. } = p {
                    Some((points.len(), *closed))
                } else {
                    None
                }
            })
            .collect();
        let fills: Vec<_> = out
            .output
            .iter()
            .filter_map(|p| {
                if let Prim::Fill { points, .. } = p {
                    Some(points.len())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(lines, [(2, false)]);
        assert_eq!(fills, [4]);
    }
    #[test]
    fn pie_uses_its_retained_span_and_empty_allocation_can_become_positive() {
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
            screen: None,
        };
        let mut basis = basis(ChartKind::Pie);
        let slice_bounds = |out: &Drawing, label| {
            assert_eq!(
                out.output
                    .iter()
                    .filter_map(|p| match p {
                        Prim::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                [label],
                "one positive slice has one percentage label"
            );
            out.output
                .iter()
                .filter_map(|p| match p {
                    Prim::Fill { points, .. } => Some(points.iter().copied()),
                    _ => None,
                })
                .flatten()
                .map(|origin| Rect {
                    origin,
                    size: Vec2::default(),
                })
                .reduce(Rect::union)
                .expect("visible slice")
        };
        let center = basis.plot.frame.center();
        let half = slice_bounds(
            &draw(&series("[1]"), &basis, &mut build).expect("half pie"),
            "50%",
        );
        assert!(
            (half.origin.x - center.x).abs() < 1e-4
                && half.size.x > 0.0
                && (half.center().y - center.y).abs() <= 0.05 * half.size.y
                && (half.size.y - 2.0 * half.size.x).abs() <= 0.1 * half.size.y,
            "the retained half spans the right side of the pie"
        );
        basis.pie = None;
        let full = slice_bounds(
            &draw(&series("[1]"), &basis, &mut build).expect("new positive pie"),
            "100%",
        );
        assert!(
            full.size.x > 0.0
                && (full.size.y - full.size.x).abs() <= 0.1 * full.size.x
                && (full.center().x - center.x).abs() <= 0.05 * full.size.x
                && (full.center().y - center.y).abs() <= 0.05 * full.size.y,
            "the fallback covers a full disc around the center"
        );
    }
    #[test]
    fn cancellation_and_text_measurement_failure_propagate() {
        struct Fail;
        impl TextMeasure for Fail {
            fn measure(
                &mut self,
                _: &str,
                _: TextStyle,
                _: Option<f32>,
            ) -> Result<[f32; 2], MeasureError> {
                Err(MeasureError)
            }
        }
        let cancel = AtomicBool::new(false);
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Fail,
            cancel: &cancel,
            scales: &BTreeMap::new(),
            screen: None,
        };
        assert!(
            matches!(
                draw(&series("[1]"), &basis(ChartKind::Pie), &mut build),
                Err(PrepareError::TextMeasure)
            ),
            "measurement error propagates"
        );
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            matches!(
                draw(&series("[1]"), &basis(ChartKind::Pie), &mut build),
                Err(PrepareError::Cancelled)
            ),
            "cancellation propagates"
        );
    }
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one lifecycle pins attached, cut and updated residency and inherited opacity"
    )]
    fn residency_and_opacity_follow_the_chart_lifecycle() {
        let Record::Begin(header) =
            parse_record(br#"{"begin":{"v":1,"scene":"resident","surface":"inline","alt":""}}"#)
                .expect("header")
        else {
            panic!("begin");
        };
        let mut scene = SceneState::begin(header).expect("scene");
        let mut budget = GlobalBudget::default();
        let before = LayoutState::new(&scene);
        let [c, s, t]: [WireId; 3] =
            ["c", "s", "t"].map(|id| id.to_owned().try_into().expect("id"));
        let arc = heap::ARC_HEADER;
        // Both trees fit one leaf, each one-byte id has two heap copies and the version is one Arc.
        let index = |objects: usize| {
            heap::btree::<WireId, Placed>(objects)
                + heap::btree::<Handle, WireId>(objects)
                + 2 * objects
                + arc
        };
        // A series source owns its id, "c" and two points.
        let series = arc + size_of::<Op>() + 2 + 2 * size_of::<Point>();
        let motion = arc + size_of::<Motions>() + heap::btree::<&str, Motion>(1);
        let own = |step: &PreparedStep| {
            step.display.items.capacity() * size_of::<Item>()
                + step.removed.capacity() * size_of::<Handle>()
                + step.glides.capacity() * size_of::<Glide>()
                + arc
                + size_of::<BTreeMap<usize, usize>>()
        };
        // Three primitive arrays and six two-point lines, then the plot and both bases.
        let full = 18 * size_of::<Prim>()
            + 12 * size_of::<Vec2>()
            + 10
            + 9 * arc
            + index(3)
            + arc
            + size_of::<Plot>()
            + 2 * size_of::<Text>()
            + 3
            + 2 * (arc + size_of::<Basis>() + 2 * size_of::<[f64; 2]>());
        let bases = |state: &LayoutState| {
            [&s, &t].map(|id| Arc::clone(state.objects[id].chart_basis.as_ref().expect("basis")))
        };
        let opacity = |state: &LayoutState| {
            let object = &state.objects[&s];
            (
                object.item.opacity.to_bits(),
                object.rest.opacity.map(f32::to_bits),
                object.inherited_opacity.to_bits(),
            )
        };
        let bits = |own: f32, rest: f32, inherited: f32| {
            (own.to_bits(), Some(rest.to_bits()), inherited.to_bits())
        };
        let first = picture(
            &mut scene,
            &mut budget,
            &before,
            r#"{"step":{"id":"first","put":[{"chart":{"id":"c","kind":"bar","stacked":true,"box":[0,0,400,300],"opacity":0.5,"categories":["ab","c"],"x":{"min":-0.5,"max":1.5},"y":{"min":-5,"max":5}}},{"series":{"id":"s","in":"c","opacity":0.5,"data":[1,2]}},{"series":{"id":"t","in":"c","data":[2,1]}},{"anim":{"target":"s","track":"opacity","keys":[[0,0.75]]}}]}}"#,
        );
        // The chart source owns its id, two category texts and their three bytes.
        let chart = arc + size_of::<Op>() + 4 + 2 * size_of::<Text>();
        assert_eq!(
            first.resident_bytes,
            full + chart + 2 * series + motion + own(&first),
            "every source once, and s's track owns a motion map"
        );
        let initial = bases(&first.after);
        assert!(
            Arc::ptr_eq(&initial[0].plot, &initial[1].plot),
            "series share one plot"
        );
        assert_eq!(
            first.after.objects[&c].item.opacity.to_bits(),
            0.5_f32.to_bits()
        );
        assert_eq!(opacity(&first.after), bits(0.5, 0.75, 1.0));
        let second = picture(
            &mut scene,
            &mut budget,
            &first.after,
            r#"{"step":{"id":"color","set":[{"target":"s","color":"red"}]}}"#,
        );
        assert_eq!(
            second.resident_bytes,
            full + series + own(&second),
            "the refit shares s's motion map"
        );
        let refit = bases(&second.after);
        assert!(
            !Arc::ptr_eq(&initial[0], &refit[0])
                && !Arc::ptr_eq(&initial[1], &refit[1])
                && Arc::ptr_eq(&refit[0].plot, &refit[1].plot),
            "an attached refit replaces both bases around one new plot"
        );
        let revealed = [&s, &t].map(|id| second.after.handle(id).expect("handle"));
        scene
            .stop(
                &StopCut {
                    activated: 2,
                    revealed: Bounded::new(revealed.to_vec()).expect("cut"),
                },
                &mut budget,
            )
            .expect("stop");
        let detached = second.after.cut(&scene).expect("detached layout");
        let frozen = bases(&detached);
        assert!(
            Arc::ptr_eq(&frozen[0], &refit[0])
                && Arc::ptr_eq(&frozen[1], &refit[1])
                && Arc::ptr_eq(&frozen[0].plot, &frozen[1].plot),
            "the cut keeps both bases and their plot"
        );
        assert_eq!(
            opacity(&detached),
            bits(0.5, 0.75, 0.5),
            "the cut moves the chart's 0.5 into the inherited factor, not the own base"
        );
        let next = picture(
            &mut scene,
            &mut budget,
            &detached,
            r#"{"step":{"id":"data","put":[{"anim":{"target":"s","track":"opacity","keys":[[0,0.25]]}}],"set":[{"target":"s","data":[2,1]}]}}"#,
        );
        assert_eq!(
            next.resident_bytes,
            arc + 2 * size_of::<Prim>() + index(2) + series + motion + own(&next),
            "the new track copies s's motion map instead of writing through the detached prefix"
        );
        let envelope = |state: &LayoutState| {
            let motion = state.objects[&s]
                .motion
                .as_ref()
                .expect("s owns an opacity track");
            motion["opacity"].min[0].to_bits()
        };
        assert_eq!(
            (envelope(&detached), envelope(&next.after)),
            (0.75_f32.to_bits(), 0.25_f32.to_bits()),
            "the detached prefix keeps its own envelope"
        );
        let kept = bases(&next.after);
        assert!(
            Arc::ptr_eq(&kept[0], &frozen[0]) && Arc::ptr_eq(&kept[1], &frozen[1]),
            "a detached update allocates no new basis"
        );
        assert_eq!(
            opacity(&next.after),
            bits(0.5, 0.25, 0.5),
            "a detached update keeps its base and inherited factor; the new track owns the rest"
        );
    }
}
