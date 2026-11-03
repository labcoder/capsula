use super::{Drawing, Plot, color, count, fraction, point};
use crate::{
    Text,
    display::{Paint, TextStyle},
    geom::{Rect, Vec2},
    layout::{
        PrepareError,
        primitives::{Build, rect},
    },
    limits::CHART_LABEL_INSET,
    record::{Align, Axis, AxisScale, Chart, ChartKind, Series},
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub(super) struct Domain {
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) log: bool,
}
impl Domain {
    pub(super) fn fraction(self, value: f64) -> f32 {
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
    pub(super) fn value(self, value: f64) -> f64 {
        if self.log {
            10.0_f64.powf(value).min(f64::MAX)
        } else {
            value
        }
    }
    fn ticks(self) -> Vec<f64> {
        let step = tick_step(self, TickBounds::Inside);
        let first = (self.min / step).ceil();
        (0..7)
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
    if range.log { best.ceil() } else { best }
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
pub(super) fn compact(value: f64, unit: Option<&Text>) -> Result<Text, PrepareError> {
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
        return Ok(unit.ok_or(crate::ErrorCode::TooLong)?.clone());
    }
    Ok(label.try_into()?)
}

#[expect(
    clippy::too_many_lines,
    reason = "chart header measurement resolves the single shared plot"
)]
pub(super) fn header(
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
    let mut top = frame.origin.y + CHART_LABEL_INSET;
    if let Some(title) = &chart.title {
        let size = out.label(
            title.clone(),
            Vec2::new(frame.origin.x + CHART_LABEL_INSET, top),
            frame.size.x - 2.0 * CHART_LABEL_INSET,
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
        let mut x = frame.origin.x + CHART_LABEL_INSET;
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
            if x > frame.origin.x + CHART_LABEL_INSET
                && x + width + 18.0 > frame.max().x - CHART_LABEL_INSET
            {
                top += height;
                x = frame.origin.x + CHART_LABEL_INSET;
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
            origin: Vec2::new(frame.origin.x + CHART_LABEL_INSET, top),
            size: Vec2::new(
                (frame.size.x - 2.0 * CHART_LABEL_INSET).max(0.0),
                (frame.max().y - top - CHART_LABEL_INSET).max(0.0),
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
            Vec2::new(frame.origin.x + CHART_LABEL_INSET, top),
            frame.size.x - 2.0 * CHART_LABEL_INSET,
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
            .min(plot.frame.max().x - width)
            .max(plot.frame.origin.x);
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

#[cfg(test)]
mod tests {
    use super::super::tests::Measure;
    use super::*;
    use crate::{display::Prim, theme::Theme};
    use std::{collections::BTreeMap, sync::atomic::AtomicBool};
    #[test]
    fn seven_tick_windows_keep_the_upper_tick() {
        for log in [false, true] {
            assert_eq!(
                Domain {
                    min: 0.0,
                    max: 12.0,
                    log
                }
                .ticks(),
                [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0]
            );
        }
    }

    #[test]
    fn explicit_log_window_without_a_decade_has_no_y_labels() {
        let chart: Chart = serde_json::from_str(
            r#"{"id":"c","box":[0,0,400,300],"kind":"line","y":{"min":2,"max":8,"scale":"log"}}"#,
        )
        .expect("chart");
        let series: Series = serde_json::from_str(r#"{"id":"s","data":[2,8]}"#).expect("series");
        let mut build = Build {
            viewport: Rect::default(),
            theme: &Theme::default(),
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
        };
        let (plot, out) = header(&chart, &[&series], &mut build).expect("header");
        assert_eq!(plot.y.min.to_bits(), 2.0_f64.log10().to_bits());
        assert_eq!(plot.y.max.to_bits(), 8.0_f64.log10().to_bits());
        assert_eq!(
            out.output
                .iter()
                .filter(|p| matches!(
                    p,
                    Prim::Text {
                        align: Align::Right,
                        ..
                    }
                ))
                .count(),
            0
        );
        assert_eq!(plot.frame.origin.x.to_bits(), 22.0_f32.to_bits());
    }

    #[test]
    fn log_x_labels_keep_both_edge_decades() {
        let chart: Chart = serde_json::from_str(
            r#"{"id":"c","box":[0,0,800,300],"kind":"scatter","x":{"scale":"log"}}"#,
        )
        .expect("chart");
        let series: Series =
            serde_json::from_str(r#"{"id":"s","data":[[0.00001,0],[100000,1]]}"#).expect("series");
        let mut build = Build {
            viewport: Rect::default(),
            theme: &Theme::default(),
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
        };
        let (plot, out) = header(&chart, &[&series], &mut build).expect("header");
        let labels: Vec<_> = out
            .output
            .iter()
            .filter_map(|p| match p {
                Prim::Text {
                    origin,
                    text,
                    align: Align::Center,
                    ..
                } => Some((origin.x, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
            ["1.0e-6", "1.0e-4", "0.01", "1", "100", "10k", "1M"]
        );
        assert_eq!(labels[0].0.to_bits(), plot.frame.origin.x.to_bits());
        // The "1M" label measures two glyphs at 7 px each.
        assert_eq!((labels[6].0 + 14.0).to_bits(), plot.frame.max().x.to_bits());
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
    fn log_ticks_and_rounding_use_whole_decades() {
        let axis = Axis {
            scale: AxisScale::Log,
            ..Axis::default()
        };
        let range = domain(2.0, 5.0, &axis, None, None);
        assert_eq!(range.min.to_bits(), 0.0_f64.to_bits());
        assert_eq!(range.max.to_bits(), 1.0_f64.to_bits());
        let chart: Chart = serde_json::from_str(
            r#"{"id":"c","box":[0,0,400,300],"kind":"line","y":{"min":1,"max":100,"scale":"log"}}"#,
        )
        .expect("chart");
        let series: Series =
            serde_json::from_str(r#"{"id":"s","data":[1,10,100]}"#).expect("series");
        let theme = Theme::default();
        let mut build = Build {
            viewport: Rect::default(),
            theme: &theme,
            measure: &mut Measure,
            cancel: &AtomicBool::new(false),
            scales: &BTreeMap::new(),
        };
        let (plot, out) = header(&chart, &[&series], &mut build).expect("header");
        let mut texts = Vec::new();
        let mut centres = Vec::new();
        for p in &out.output {
            if let Prim::Text {
                origin,
                text,
                style,
                max_width,
                align: Align::Right,
                ..
            } = p
            {
                texts.push(text.as_str());
                centres.push(origin.y + style.size * 0.65);
                assert_eq!(max_width.map(f32::to_bits), Some(29.0_f32.to_bits()));
            }
        }
        assert_eq!(texts, ["1", "10", "100"]);
        let frame = plot.frame;
        for (centre, expected) in centres.into_iter().zip([
            frame.max().y,
            frame.origin.y + frame.size.y * 0.5,
            frame.origin.y,
        ]) {
            assert!(
                (centre - expected).abs() < 1e-4,
                "decade label centred on its tick"
            );
        }
    }
}
