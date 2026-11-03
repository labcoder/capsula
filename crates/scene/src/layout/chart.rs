use super::{
    PrepareError,
    primitives::{Build, rect},
    state::{LayoutState, Placed},
};
use crate::{
    Handle, Op, Text, WireId,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    record::{Align, Axis, AxisScale, Chart, ChartKind, Point, Series},
};
use std::{
    collections::{BTreeMap, BTreeSet},
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

#[derive(Clone, Copy, Debug)]
struct Domain {
    min: f64,
    max: f64,
    log: bool,
}
impl Domain {
    fn fraction(self, value: f64) -> f32 {
        let value = if self.log {
            value.max(f64::from_bits(1)).log10()
        } else {
            value
        };
        if value <= self.min {
            return 0.0;
        }
        if value >= self.max {
            return 1.0;
        }
        let span = self.max - self.min;
        fraction(if span.is_finite() {
            (value - self.min) / span
        } else {
            (value * 0.5 - self.min * 0.5) / (self.max * 0.5 - self.min * 0.5)
        })
    }
    fn value(self, value: f64) -> f64 {
        if self.log {
            10.0_f64.powf(value).min(f64::MAX)
        } else {
            value
        }
    }
    fn ticks(self) -> Vec<f64> {
        let step = tick_step(self, TickBounds::Inside);
        let first = (self.min / step).ceil();
        (0..6)
            .map(|i| (first + f64::from(i)) * step)
            .filter(|v| v.is_finite() && *v >= self.min && *v <= self.max)
            .collect()
    }
}
#[derive(Clone, Copy, Debug)]
enum TickBounds {
    Inside,
    Outside,
}
fn tick_step(range: Domain, bounds: TickBounds) -> f64 {
    let raw = (range.max / 5.0 - range.min / 5.0).max(f64::from_bits(1));
    let base = 10.0_f64.powf(raw.log10().floor()).max(f64::from_bits(1));
    let (mut best, mut error) = (raw, f64::INFINITY);
    for scale in [0.1, 1.0, 10.0] {
        for factor in [1.0, 2.0, 5.0] {
            let step = base * scale * factor;
            if !step.is_finite() || step <= 0.0 {
                continue;
            }
            let lo = range.min / step;
            let hi = range.max / step;
            let ticks = match bounds {
                TickBounds::Inside => hi.floor() - lo.ceil(),
                TickBounds::Outside => hi.ceil() - lo.floor(),
            } + 1.0;
            let candidate = (ticks - 5.0).abs();
            if candidate < error {
                best = step;
                error = candidate;
            }
        }
    }
    best
}
fn domain(
    mut min: f64,
    mut max: f64,
    axis: &Axis,
    baseline: Option<f64>,
    padding: Option<usize>,
) -> Domain {
    let log = axis.scale == AxisScale::Log;
    if !min.is_finite() {
        min = if log { 1.0 } else { 0.0 };
        max = if log { 10.0 } else { 1.0 };
    }
    if let Some(baseline) = baseline
        && !log
    {
        min = min.min(baseline);
        max = max.max(baseline);
    }
    if log {
        min = min.log10();
        max = max.log10();
    }
    let transform = |v: crate::text::Number| if log { v.get().log10() } else { v.get() };
    if let Some(n) = padding {
        let pad = if n > 1 {
            (max / 2.0 - min / 2.0) / f64::from(count(n - 1))
        } else {
            0.5
        };
        if (min - pad).is_finite() {
            min -= pad;
        }
        if (max + pad).is_finite() {
            max += pad;
        }
    }
    min = axis.min.map_or(min, transform);
    max = axis.max.map_or(max, transform);
    if max <= min {
        let pad = (min.abs().max(max.abs()) * 0.1).max(1.0);
        if axis.min.is_some() {
            max = (min + pad).min(f64::MAX);
        } else if axis.max.is_some() {
            min = (max - pad).max(-f64::MAX);
        } else {
            min = (min - pad).max(-f64::MAX);
            max = (max + pad).min(f64::MAX);
        }
    }
    let mut range = Domain { min, max, log };
    if padding.is_none() {
        let step = tick_step(range, TickBounds::Outside);
        let lo = (min / step).floor() * step;
        let hi = (max / step).ceil() * step;
        if axis.min.is_none() && lo.is_finite() {
            range.min = lo;
        }
        if axis.max.is_none() && hi.is_finite() {
            range.max = hi;
        }
    }
    if log {
        range.min = range.min.max(f64::from_bits(1).log10());
        range.max = range.max.min(f64::MAX.log10());
    }
    range
}
fn compact(value: f64, unit: Option<&Text>) -> Result<Text, PrepareError> {
    let (scaled, suffix) = [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "k")]
        .into_iter()
        .find(|(scale, _)| value.abs() >= *scale)
        .map_or((value, ""), |(s, t)| (value / s, t));
    let raw = if scaled.abs() >= 1e6 || (scaled != 0.0 && scaled.abs() < 0.01) {
        format!("{scaled:.1e}")
    } else {
        let scaled = if scaled == 0.0 { 0.0 } else { scaled };
        let raw = if suffix.is_empty() {
            format!("{scaled:.2}")
        } else {
            format!("{scaled:.1}")
        };
        raw.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    let number = format!("{raw}{suffix}");
    let label = format!("{number}{}", unit.map_or("", Text::as_str));
    if label.chars().count() > crate::limits::MAX_TEXT_CHARS {
        return Ok(unit.ok_or(PrepareError::Stale)?.clone());
    }
    Ok(label.try_into()?)
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
        size_of::<Self>()
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
impl fmt::Debug for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChartBasis { .. }")
    }
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
    series
        .color
        .as_ref()
        .map_or_else(|| build.theme.series(palette), |c| build.theme.resolve(c))
        .into()
}

#[expect(
    clippy::too_many_lines,
    reason = "chart header measurement resolves the single shared plot"
)]
fn header(
    chart: &Chart,
    series: &[&Series],
    build: &mut Build<'_>,
) -> Result<(Arc<Plot>, Drawing), PrepareError> {
    let frame = chart.bounds.map_or(
        Rect {
            origin: build.viewport.origin,
            size: Vec2::new(build.viewport.size.x, build.viewport.size.y.min(300.0)),
        },
        rect,
    );
    let mut out = Drawing::new(frame);
    out.rect(frame, 10.0, Paint::from(build.theme.surface).faded(0.92));
    let style = TextStyle {
        size: build.theme.font_size * 0.8,
        bold: false,
    };
    let muted = Paint::from(build.theme.muted);
    let mut top = frame.origin.y + 12.0;
    if let Some(title) = &chart.title {
        let size = out.label(
            title.clone(),
            Vec2::new(frame.origin.x + 12.0, top),
            frame.size.x - 24.0,
            Align::Left,
            TextStyle {
                size: build.theme.font_size,
                bold: true,
            },
            build.theme.fg.into(),
            build,
        )?;
        top += size.y + 8.0;
    }
    if series.iter().filter(|s| s.name.is_some()).count() >= 2 {
        let mut x = frame.origin.x + 12.0;
        let mut height = style.size * 1.3 + 8.0;
        let mut palette = 0;
        for s in series {
            let slot = palette;
            palette += if chart.kind == ChartKind::Pie {
                s.data.len()
            } else {
                1
            };
            let Some(name) = &s.name else {
                continue;
            };
            let width = build
                .measure(name, style, None)?
                .x
                .min((frame.size.x - 46.0).max(1.0));
            if x > frame.origin.x + 12.0 && x + width + 18.0 > frame.max().x - 12.0 {
                top += height;
                x = frame.origin.x + 12.0;
                height = style.size * 1.3 + 8.0;
            }
            out.rect(
                Rect {
                    origin: Vec2::new(x, top + 3.0),
                    size: Vec2::new(9.0, 9.0),
                },
                2.0,
                color(s, slot, build),
            );
            let size = out.label(
                name.clone(),
                Vec2::new(x + 14.0, top),
                width,
                Align::Left,
                style,
                muted,
                build,
            )?;
            height = height.max(size.y + 8.0);
            x += width + 28.0;
        }
        top += height;
    }
    let n = series
        .iter()
        .map(|s| s.data.len())
        .max()
        .unwrap_or(0)
        .max(chart.categories.len());
    let (mut xmin, mut xmax, mut ymin, mut ymax) = (
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    );
    let mut stacks = vec![[0.0_f64; 2]; n];
    for s in series {
        for (i, p) in s.data.iter().enumerate() {
            let (x, y) = point(p, i);
            xmin = xmin.min(x);
            xmax = xmax.max(x);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
            if chart.kind == ChartKind::Bar && chart.stacked {
                let base = &mut stacks[i][usize::from(y < 0.0)];
                *base += y;
                ymin = ymin.min(*base);
                ymax = ymax.max(*base);
            }
        }
    }
    if !chart.categories.is_empty() && chart.x.scale != AxisScale::Log {
        xmin = xmin.min(0.0);
        xmax = xmax.max(f64::from(count(chart.categories.len() - 1)));
    }
    let x = domain(
        xmin,
        xmax,
        &chart.x,
        None,
        (chart.kind == ChartKind::Bar).then_some(n),
    );
    let y = domain(
        ymin,
        ymax,
        &chart.y,
        matches!(chart.kind, ChartKind::Bar | ChartKind::Area).then_some(0.0),
        None,
    );
    let mut plot = Plot {
        frame: Rect {
            origin: Vec2::new(frame.origin.x + 12.0, top),
            size: Vec2::new(
                (frame.size.x - 24.0).max(0.0),
                (frame.max().y - top - 12.0).max(0.0),
            ),
        },
        kind: chart.kind,
        x,
        y,
        categories: chart.categories.to_vec(),
    };
    if chart.kind == ChartKind::Pie {
        return Ok((Arc::new(plot), out));
    }
    if let Some(label) = &chart.y.label {
        let size = out.label(
            label.clone(),
            Vec2::new(frame.origin.x + 12.0, top),
            frame.size.x - 24.0,
            Align::Left,
            style,
            muted,
            build,
        )?;
        top += size.y + 6.0;
    }
    let labels = y
        .ticks()
        .into_iter()
        .map(|tick| Ok((tick, compact(y.value(tick), chart.y.unit.as_ref())?)))
        .collect::<Result<Vec<_>, PrepareError>>()?;
    let mut left = 0.0_f32;
    for (_, label) in &labels {
        left = left.max(build.measure(label, style, None)?.x);
    }
    left = (left + 22.0).min(frame.size.x * 0.4);
    let plot_width = (frame.size.x - left - 18.0).max(0.0);
    let xlabel_height = chart
        .x
        .label
        .as_ref()
        .map(|label| build.measure(label, style, Some(plot_width.max(1.0))))
        .transpose()?
        .map_or(0.0, |size| size.y + 6.0);
    let bottom = 16.0 + style.size * 1.3 + xlabel_height;
    plot.frame = Rect {
        origin: Vec2::new(frame.origin.x + left, top + 4.0),
        size: Vec2::new(
            (frame.size.x - left - 18.0).max(0.0),
            (frame.max().y - top - bottom - 4.0).max(0.0),
        ),
    };
    if plot.frame.size.x < 1.0 || plot.frame.size.y < 1.0 {
        return Ok((Arc::new(plot), out));
    }
    for (tick, label) in labels {
        let at = plot.at(x.value(x.min), y.value(tick));
        out.line(
            Arc::from([at, Vec2::new(plot.frame.max().x, at.y)]),
            Paint::from(build.theme.line).faded(0.16),
            1.0,
        );
        out.label(
            label,
            Vec2::new(frame.origin.x + 6.0, at.y - style.size * 0.65),
            left - 14.0,
            Align::Right,
            style,
            muted,
            build,
        )?;
    }
    let labels = if chart.categories.is_empty() {
        x.ticks()
            .into_iter()
            .map(|v| Ok((x.value(v), compact(x.value(v), chart.x.unit.as_ref())?)))
            .collect::<Result<Vec<_>, PrepareError>>()?
    } else {
        chart
            .categories
            .iter()
            .enumerate()
            .map(|(i, t)| (f64::from(count(i)), t.clone()))
            .collect()
    };
    let mut right = f32::NEG_INFINITY;
    for (value, label) in labels {
        let transformed = if x.log {
            value.max(f64::from_bits(1)).log10()
        } else {
            value
        };
        if transformed < x.min || transformed > x.max {
            continue;
        }
        let width = build.measure(&label, style, None)?.x.min(plot.frame.size.x);
        let left = (plot.at(value, 0.0).x - width * 0.5)
            .clamp(plot.frame.origin.x, plot.frame.max().x - width);
        if left < right + 8.0 {
            continue;
        }
        out.label(
            label,
            Vec2::new(left, plot.frame.max().y + 7.0),
            width,
            Align::Center,
            style,
            muted,
            build,
        )?;
        right = left + width;
    }
    if let Some(label) = &chart.x.label {
        out.label(
            label.clone(),
            Vec2::new(plot.frame.origin.x, frame.max().y - xlabel_height - 2.0),
            plot.frame.size.x,
            Align::Center,
            style,
            muted,
            build,
        )?;
    }
    let baseline = plot.at(x.value(x.min), 0.0).y;
    out.line(
        Arc::from([
            Vec2::new(plot.frame.origin.x, baseline),
            Vec2::new(plot.frame.max().x, baseline),
        ]),
        build.color(chart.color.as_ref()).faded(0.4),
        1.0,
    );
    Ok((Arc::new(plot), out))
}

fn pie_label(category: Option<&Text>, share: f64) -> Result<Text, PrepareError> {
    let percent = format!("{:.0}%", share * 100.0);
    let label = category.map_or_else(|| percent.clone(), |c| format!("{} {percent}", c.as_str()));
    if label.chars().count() > crate::limits::MAX_TEXT_CHARS {
        return Ok(category.ok_or(PrepareError::Stale)?.clone());
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
    let opacity = old.map_or_else(
        || {
            crate::state::object::visual(&source)
                .and_then(|v| v.opacity)
                .map_or(1.0, crate::Finite::get)
        },
        |o| o.item.opacity,
    );
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
                if let Some(old) = state
                    .objects
                    .get(id)
                    .filter(|o| o.item.parent.is_none() && o.chart_basis.is_some())
                {
                    let basis = Arc::clone(old.chart_basis.as_ref().ok_or(PrepareError::Stale)?);
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
                            .ok_or(PrepareError::Stale)?,
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
    let id = state
        .handles
        .get(&handle)
        .ok_or(PrepareError::Stale)?
        .clone();
    let old = state.objects.get(&id).ok_or(PrepareError::Stale)?;
    let source = Arc::clone(sources.get(&id).map_or(&old.source, |(_, s)| s));
    let Op::Chart(chart) = &*source else {
        return Err(PrepareError::Stale);
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
        return Err(PrepareError::Stale);
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
            return Err(PrepareError::Stale);
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
        bytes += size_of::<Basis>() + basis.baselines.len() * size_of::<[f64; 2]>();
        let drawing = draw(series, &basis, build)?;
        install(state, id, source, h, Some(handle), drawing, Some(basis));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        display::{MeasureError, TextMeasure},
        theme::Theme,
    };
    use std::sync::atomic::AtomicBool;
    struct Measure;
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
    fn ticks_are_nice_and_ranges_cover_data() {
        for (min, max) in [
            (0.0, 91.0),
            (-7.0, 13.0),
            (42.0, 42.0),
            (-1e12, 1e12),
            (0.001, 0.009),
        ] {
            let range = domain(min, max, &Axis::default(), None, None);
            assert!(
                range.min <= min && range.max >= max && range.min < range.max,
                "range covers data"
            );
            assert!((4..=6).contains(&range.ticks().len()), "nice tick count");
            let step = tick_step(range, TickBounds::Inside);
            let magnitude = 10.0_f64.powf(step.log10().floor());
            let factor = step / magnitude;
            assert!(
                [1.0_f64, 2.0, 5.0, 10.0]
                    .iter()
                    .any(|n| (factor - n).abs() < 1e-8),
                "tick step uses 1/2/5"
            );
        }
        assert_eq!(
            compact(1234.0, Some(&"ms".to_owned().try_into().expect("text")))
                .expect("label")
                .as_str(),
            "1.2kms"
        );
        assert_eq!(compact(3.4e6, None).expect("label").as_str(), "3.4M");
    }
    #[test]
    fn extremes_and_log_domains_remain_finite_and_clamped() {
        for value in [f64::MAX, -f64::MAX, f64::from_bits(1), 0.0] {
            let range = domain(value, value, &Axis::default(), None, None);
            assert!(
                range.min.is_finite() && range.max.is_finite() && range.max > range.min,
                "finite expanded domain"
            );
        }
        let axis: Axis =
            serde_json::from_str(r#"{"min":1,"max":100,"scale":"log"}"#).expect("axis");
        let range = domain(1.0, 100.0, &axis, Some(0.0), None);
        assert!((range.fraction(10.0) - 0.5).abs() < 1e-6, "log midpoint");
        assert_eq!(range.fraction(-1.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(range.fraction(1e300).to_bits(), 1.0_f32.to_bits());
        for value in [f64::from_bits(1), f64::MAX] {
            let axis = Axis {
                scale: AxisScale::Log,
                ..Axis::default()
            };
            let range = domain(value, value, &axis, Some(0.0), None);
            assert!(
                range.min.is_finite() && range.max.is_finite() && range.max > range.min,
                "finite log extremes"
            );
            assert!(
                range.ticks().iter().all(|v| range.value(*v).is_finite()),
                "finite log ticks"
            );
        }
        let axis = Axis {
            min: Some(f64::MAX.try_into().expect("number")),
            ..Axis::default()
        };
        let range = domain(f64::INFINITY, f64::NEG_INFINITY, &axis, None, None);
        assert_eq!(range.min.to_bits(), f64::MAX.to_bits());
        assert_eq!(range.max.to_bits(), f64::MAX.to_bits());
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
        };
        for kind in [
            ChartKind::Bar,
            ChartKind::Line,
            ChartKind::Area,
            ChartKind::Scatter,
            ChartKind::Pie,
        ] {
            for data in ["[]", "[1]", "[0,1e300,2]"] {
                let out = draw(&series(data), &basis(kind), &mut build).expect("draw");
                assert!(out.bounds.within_canvas(), "finite output bounds");
                if data != "[]" {
                    assert!(!out.output.is_empty(), "nonempty series draws");
                }
            }
            if kind != ChartKind::Pie {
                let out =
                    draw(&series("[-12,0,1e300]"), &basis(kind), &mut build).expect("signed draw");
                assert!(
                    out.bounds.within_canvas() && !out.output.is_empty(),
                    "signed and huge values draw"
                );
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
        let a = series("[[10,1e300],[20,-3]]");
        let b = a.clone();
        let chart: Chart = serde_json::from_str(
            r#"{"id":"c","box":[0,0,400,300],"stacked":true,"x":{"min":5,"max":25}}"#,
        )
        .expect("chart");
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
        };
        let (plot, _) = header(&chart, &[&a, &b], &mut build).expect("header");
        assert!(
            plot.y.max >= 2e300 && plot.y.min <= -6.0,
            "both signed stacks set the range"
        );
        assert_eq!(plot.x.min.to_bits(), 5.0_f64.to_bits());
        assert_eq!(plot.x.max.to_bits(), 25.0_f64.to_bits());
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
        };
        let mut basis = basis(ChartKind::Pie);
        let out = draw(&series("[1]"), &basis, &mut build).expect("half pie");
        let labels: Vec<_> = out
            .output
            .iter()
            .filter_map(|p| {
                if let Prim::Text { text, .. } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(labels, ["50%"]);
        let Prim::Fill { points, .. } = &out.output[0] else {
            panic!("slice");
        };
        assert_eq!(points.len(), crate::path::PIE_ARC_SEGMENTS + 2);
        assert!(
            (points[1].y - 4.0).abs() < 1e-5 && (points[points.len() - 1].y - 96.0).abs() < 1e-5,
            "half-circle endpoints"
        );
        basis.pie = None;
        let out = draw(&series("[1]"), &basis, &mut build).expect("new positive pie");
        assert!(
            out.output
                .iter()
                .any(|p| matches!(p, Prim::Text { text, .. } if text.as_str() == "100%")),
            "empty allocation falls back to a full circle"
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
}
