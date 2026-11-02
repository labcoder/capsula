use super::{
    PrepareError,
    primitives::{Build, rect},
    state::{LayoutState, Placed},
};
use crate::{
    Handle, Op, Text, WireId,
    display::{Item, Paint, Prim, TextStyle},
    geom::{Affine, Rect, Vec2},
    record::{Align, Cell, Row, Table},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 7.0;

fn number(value: f64) -> String {
    let rounded = format!("{:.2}", if value.abs() < 0.005 { 0.0 } else { value });
    let trimmed = rounded.trim_end_matches('0').trim_end_matches('.');
    let (integer, fraction) = trimmed.split_once('.').unwrap_or((trimmed, ""));
    let (sign, digits) = integer
        .strip_prefix('-')
        .map_or(("", integer), |digits| ("-", digits));
    let mut result = String::with_capacity(rounded.len() + digits.len() / 3);
    result.push_str(sign);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    if !fraction.is_empty() {
        result.push('.');
        result.push_str(fraction);
    }
    if result.len() > crate::limits::MAX_TEXT_CHARS {
        format!("{value:.2e}")
    } else {
        result
    }
}

fn cell(value: Option<&Cell>) -> Result<Text, PrepareError> {
    Ok(match value {
        Some(Cell::Text(text)) => return Ok(text.clone()),
        Some(Cell::Number(value)) => number(value.get()),
        Some(Cell::Bool(value)) => value.to_string(),
        None => String::new(),
    }
    .try_into()?)
}

fn count(value: usize) -> f32 {
    f32::from(u16::try_from(value).expect("table counts are bounded by eight columns"))
}

#[expect(
    clippy::float_cmp,
    reason = "water filling reduces exactly tied column widths together"
)]
fn fit(widths: &mut [f32], textual: &[bool], available: f32, font: f32) {
    let mut excess = (widths.iter().sum::<f32>() - available).max(0.0);
    let minimum = (available / count(widths.len().max(1))).min(PAD_X * 2.0 + font * 2.0);
    for text_only in [true, false] {
        for _ in 0..widths.len() {
            let Some(widest) = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| **w > minimum && (!text_only || textual[*i]))
                .map(|(_, w)| *w)
                .max_by(f32::total_cmp)
            else {
                break;
            };
            let next = widths
                .iter()
                .enumerate()
                .filter(|(i, _)| !text_only || textual[*i])
                .map(|(_, w)| *w)
                .filter(|w| *w < widest)
                .fold(minimum, f32::max);
            let tied = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| **w == widest && (!text_only || textual[*i]))
                .count();
            let reduction = (excess / count(tied)).min(widest - next);
            for (i, width) in widths.iter_mut().enumerate() {
                if *width == widest && (!text_only || textual[i]) {
                    *width -= reduction;
                }
            }
            excess = (excess - reduction * count(tied)).max(0.0);
            if excess <= f32::EPSILON {
                break;
            }
        }
    }
    if excess > 0.0 {
        let total = widths.iter().sum::<f32>();
        for width in widths {
            *width *= available / total;
        }
    }
}

struct Layout {
    widths: Vec<f32>,
    align: Vec<Align>,
    cells: Vec<Vec<Text>>,
    rows: Vec<(f32, f32)>,
    title_height: f32,
    header_height: f32,
    size: Vec2,
}

impl Layout {
    #[expect(
        clippy::too_many_lines,
        reason = "column fitting and wrapped row heights share one deterministic measurement pass"
    )]
    fn new(
        table: &Table,
        rows: &[&Row],
        width: f32,
        build: &mut Build<'_>,
    ) -> Result<Self, PrepareError> {
        let normal = TextStyle {
            size: build.theme.font_size,
            bold: false,
        };
        let header = TextStyle {
            size: normal.size * 0.9,
            bold: true,
        };
        let mut cells = Vec::new();
        for row in rows {
            cells.push(
                (0..table.cols.len())
                    .map(|column| cell(row.cells.get(column)))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        let mut widths = Vec::new();
        let mut align = Vec::new();
        let mut textual = Vec::new();
        let mut headers = Vec::new();
        let mut measured = vec![Vec::new(); table.cols.len()];
        for (column, label) in table.cols.iter().enumerate() {
            let size = build.measure(label, header, None)?;
            headers.push(size);
            let mut width = size.x + PAD_X * 2.0;
            let mut numeric = !rows.is_empty();
            for (r, row) in rows.iter().enumerate() {
                let size = build.measure(
                    &cells[r][column],
                    TextStyle {
                        bold: row.emphasis,
                        ..normal
                    },
                    None,
                )?;
                measured[column].push(size);
                width = width.max(size.x + PAD_X * 2.0);
                numeric &= matches!(row.cells.get(column), Some(Cell::Number(_)));
            }
            widths.push(width);
            textual.push(!numeric);
            align.push(table.align.get(column).copied().unwrap_or(if numeric {
                Align::Right
            } else {
                Align::Left
            }));
        }
        fit(&mut widths, &textual, width, normal.size);
        let width = if widths.is_empty() {
            width
        } else {
            widths.iter().sum::<f32>()
        };
        let title_height = table
            .title
            .as_ref()
            .map(|title| {
                build.measure(
                    title,
                    TextStyle {
                        bold: true,
                        ..normal
                    },
                    Some((width - PAD_X * 2.0).max(1.0)),
                )
            })
            .transpose()?
            .map_or(0.0, |size| size.y + PAD_Y * 2.0);
        let mut header_height = header.size * 1.3 + PAD_Y * 2.0;
        for (column, label) in table.cols.iter().enumerate() {
            let max = (widths[column] - PAD_X * 2.0).max(1.0);
            let size = if headers[column].x > max {
                build.measure(label, header, Some(max))?
            } else {
                headers[column]
            };
            header_height = header_height.max(size.y + PAD_Y * 2.0);
        }
        let mut geometry = Vec::new();
        let mut y = title_height + header_height;
        for (r, row) in rows.iter().enumerate() {
            let mut height = normal.size * 1.3 + PAD_Y * 2.0;
            for (column, width) in widths.iter().enumerate() {
                let max = (width - PAD_X * 2.0).max(1.0);
                let size = if measured[column][r].x > max {
                    build.measure(
                        &cells[r][column],
                        TextStyle {
                            bold: row.emphasis,
                            ..normal
                        },
                        Some(max),
                    )?
                } else {
                    measured[column][r]
                };
                height = height.max(size.y + PAD_Y * 2.0);
            }
            geometry.push((y, height));
            y += height;
        }
        Ok(Self {
            widths,
            align,
            cells,
            rows: geometry,
            title_height,
            header_height,
            size: Vec2::new(width, y),
        })
    }
    fn header(&self, table: &Table, origin: Vec2, build: &Build<'_>) -> Vec<Prim> {
        let mut output = vec![Prim::Rect {
            rect: Rect {
                origin,
                size: self.size,
            },
            radius: 10.0,
            fill: Some(Paint::from(build.theme.surface).faded(0.95)),
            border: None,
        }];
        if let Some(title) = &table.title {
            output.push(Prim::Text {
                origin: origin + Vec2::new(PAD_X, PAD_Y),
                text: title.clone(),
                style: TextStyle {
                    size: build.theme.font_size,
                    bold: true,
                },
                color: build.theme.fg.into(),
                max_width: Some((self.size.x - PAD_X * 2.0).max(1.0)),
                align: Align::Left,
            });
        }
        let mut x = origin.x;
        for (column, width) in self.widths.iter().enumerate() {
            output.push(Prim::Text {
                origin: Vec2::new(x + PAD_X, origin.y + self.title_height + PAD_Y),
                text: table.cols[column].clone(),
                style: TextStyle {
                    size: build.theme.font_size * 0.9,
                    bold: true,
                },
                color: build.theme.muted.into(),
                max_width: Some((width - PAD_X * 2.0).max(1.0)),
                align: self.align[column],
            });
            x += width;
        }
        let y = origin.y + self.title_height + self.header_height;
        output.push(Prim::Line {
            points: Arc::from([Vec2::new(origin.x, y), Vec2::new(origin.x + self.size.x, y)]),
            closed: false,
            color: build.color(table.color.as_ref()).faded(0.65),
            width: 1.0,
            dashed: false,
        });
        output
    }
    fn row(&self, row: &Row, index: usize, origin: Vec2, build: &Build<'_>) -> (Rect, Vec<Prim>) {
        let (y, height) = self.rows[index];
        let bounds = Rect {
            origin: origin + Vec2::new(0.0, y),
            size: Vec2::new(self.size.x, height),
        };
        let mut output = Vec::new();
        if row.emphasis {
            output.push(Prim::Rect {
                rect: bounds,
                radius: 5.0,
                fill: Some(build.color(row.color.as_ref()).faded(0.1)),
                border: None,
            });
        }
        let mut x = origin.x;
        for (column, width) in self.widths.iter().enumerate() {
            output.push(Prim::Text {
                origin: Vec2::new(x + PAD_X, bounds.origin.y + PAD_Y),
                text: self.cells[index][column].clone(),
                style: TextStyle {
                    size: build.theme.font_size,
                    bold: row.emphasis,
                },
                color: row
                    .color
                    .as_ref()
                    .map_or(build.theme.fg, |color| build.theme.resolve(color))
                    .into(),
                max_width: Some((width - PAD_X * 2.0).max(1.0)),
                align: self.align[column],
            });
            x += width;
        }
        output.push(Prim::Line {
            points: Arc::from([Vec2::new(origin.x, bounds.max().y), bounds.max()]),
            closed: false,
            color: Paint::from(build.theme.line).faded(0.2),
            width: 1.0,
            dashed: false,
        });
        (bounds, output)
    }
}

fn install(
    state: &mut LayoutState,
    id: WireId,
    source: Arc<Op>,
    handle: Handle,
    parent: Option<Handle>,
    bounds: Rect,
    primitives: Vec<Prim>,
) {
    let previous = state.objects.get(&id);
    let mut object = Placed::new(
        Item {
            handle,
            parent,
            bounds,
            primitives: primitives.into(),
            transform: Affine::default(),
            opacity: crate::state::object::visual(&source)
                .and_then(|visual| visual.opacity)
                .map_or(1.0, crate::Finite::get),
        },
        source,
        previous,
    );
    object.anchors = [Vec2::default(); 2];
    object.rank = None;
    object.initialized = true;

    state.handles.insert(handle, id.clone());
    state.objects.insert(id, object);
}

pub(super) fn place(
    sources: &BTreeMap<WireId, (Handle, Arc<Op>)>,
    before: &LayoutState,
    state: &mut LayoutState,
    removed: &[Handle],
    build: &mut Build<'_>,
) -> Result<(), PrepareError> {
    let mut dirty = BTreeSet::new();
    for (id, (handle, source)) in sources {
        match &**source {
            Op::Table(_) => {
                dirty.insert(*handle);
            }
            Op::Row(row) => {
                if let Some(old) = state
                    .objects
                    .get(id)
                    .filter(|old| old.item.parent.is_none())
                {
                    let mut output = old.item.primitives.to_vec();
                    output.retain(|primitive| !matches!(primitive, Prim::Rect { .. }));
                    let mut column = 0;
                    let mut bounds = old.item.bounds;
                    for primitive in &mut output {
                        if let Prim::Text {
                            text,
                            origin,
                            style,
                            max_width,
                            color,
                            ..
                        } = primitive
                        {
                            *text = cell(row.cells.get(column))?;
                            style.bold = row.emphasis;
                            let size = build.measure(text, *style, *max_width)?;
                            bounds = bounds.union(Rect {
                                origin: *origin,
                                size,
                            });
                            *color = row
                                .color
                                .as_ref()
                                .map_or(build.theme.fg, |color| build.theme.resolve(color))
                                .into();
                            column += 1;
                        }
                    }
                    if row.emphasis {
                        output.insert(
                            0,
                            Prim::Rect {
                                rect: bounds,
                                radius: 5.0,
                                fill: Some(build.color(row.color.as_ref()).faded(0.1)),
                                border: None,
                            },
                        );
                    }
                    install(
                        state,
                        id.clone(),
                        Arc::clone(source),
                        *handle,
                        None,
                        bounds,
                        output,
                    );
                } else {
                    let parent = row
                        .parent
                        .as_ref()
                        .and_then(|id| state.handle(id))
                        .ok_or(PrepareError::Stale)?;
                    dirty.insert(parent);
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
            && matches!(&*old.source, Op::Row(_))
            && let Some(parent) = old.item.parent
            && state.handles.contains_key(&parent)
        {
            dirty.insert(parent);
        }
    }
    for handle in dirty {
        place_table(sources, state, handle, build)?;
    }
    Ok(())
}

fn place_table(
    sources: &BTreeMap<WireId, (Handle, Arc<Op>)>,
    state: &mut LayoutState,
    handle: Handle,
    build: &mut Build<'_>,
) -> Result<(), PrepareError> {
    build.check_cancel()?;
    let id = state
        .handles
        .get(&handle)
        .ok_or(PrepareError::Stale)?
        .clone();
    let old = state.objects.get(&id).ok_or(PrepareError::Stale)?;
    let source = Arc::clone(sources.get(&id).map_or(&old.source, |(_, source)| source));
    let Op::Table(table) = &*source else {
        return Err(PrepareError::Stale);
    };
    let parent = old.item.parent;
    let viewport = table.bounds.map_or(build.viewport, rect);
    let mut rows: BTreeMap<_, _> = state
        .objects
        .iter()
        .filter(|(_, object)| {
            matches!(&*object.source, Op::Row(_)) && object.item.parent == Some(handle)
        })
        .map(|(id, object)| (object.item.handle, (id.clone(), Arc::clone(&object.source))))
        .collect();
    for (id, (row_handle, row)) in sources {
        if let Op::Row(value) = &**row
            && value.parent.as_ref() == Some(&table.id)
            && state
                .objects
                .get(id)
                .is_none_or(|old| old.item.parent.is_some())
        {
            rows.insert(*row_handle, (id.clone(), Arc::clone(row)));
        }
    }
    let values: Vec<_> = rows
        .values()
        .filter_map(|(_, source)| {
            if let Op::Row(row) = &**source {
                Some(row)
            } else {
                None
            }
        })
        .collect();
    let layout = Layout::new(table, &values, viewport.size.x, build)?;
    let primitives = layout.header(table, viewport.origin, build);
    let bounds = Rect {
        origin: viewport.origin,
        size: layout.size,
    };
    install(
        state,
        id,
        Arc::clone(&source),
        handle,
        parent,
        bounds,
        primitives,
    );
    for (index, (row_handle, (id, source))) in rows.into_iter().enumerate() {
        let Op::Row(row) = &*source else {
            unreachable!("row sources were filtered");
        };
        let (bounds, primitives) = layout.row(row, index, viewport.origin, build);
        install(
            state,
            id,
            source,
            row_handle,
            Some(handle),
            bounds,
            primitives,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_are_grouped_rounded_and_trimmed() {
        for (value, expected) in [
            (1234.5, "1,234.5"),
            (-0.25, "-0.25"),
            (12.0, "12"),
            (1000.0, "1,000"),
            (0.0, "0"),
            (-0.001, "0"),
            (999.999, "1,000"),
        ] {
            assert_eq!(number(value), expected);
        }
        assert_eq!(number(1.0e308), "1.00e308");
    }
}
