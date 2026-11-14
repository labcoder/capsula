//! §10.4's caption rules: where on a host a drop counts, and which caption buttons nothing of
//! Kumo's may cover. Pure geometry over the raw reads in `platform::host`.

use crate::platform::host::{Facts, HostFacts, HostId, Label, PhysicalRect};

/// Where a rectangle came from. Both kinds are honoured; the kind tells the desk checks which
/// path ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Reported,
    Inferred,
}

/// Where a drop counts: the title bar, without the caption buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caption {
    pub(crate) rect: PhysicalRect,
    pub(crate) source: Source,
}

/// At most three button rectangles: the report as read, the same report scaled from host
/// units while their units are unpinned, and the inferred strip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exclusions {
    pub(crate) rects: [Option<(PhysicalRect, Source)>; 3],
}

impl Exclusions {
    fn push(&mut self, rect: PhysicalRect, source: Source) {
        if let Some(slot) = self.rects.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((rect, source));
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = PhysicalRect> + '_ {
        self.rects.iter().flatten().map(|(rect, _)| *rect)
    }
}

/// A host's caption and exclusions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) caption: Caption,
    pub(crate) exclusions: Exclusions,
}

/// A window Kumo can perch on, in physical screen pixels. Its `label` prints only its length.
#[derive(Clone, Copy)]
pub struct WindowTarget {
    pub id: HostId,
    pub frame: PhysicalRect,
    pub work_area: PhysicalRect,
    pub monitor_dpi: u32,
    pub caption: Caption,
    pub exclusions: Exclusions,
    pub label: Label,
}

impl std::fmt::Debug for WindowTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowTarget")
            .field("id", &self.id)
            .field("frame", &self.frame)
            .field("work_area", &self.work_area)
            .field("monitor_dpi", &self.monitor_dpi)
            .field("caption", &self.caption)
            .field("exclusions", &self.exclusions)
            .field("label", &self.label)
            .finish()
    }
}

/// The perch target `facts` describe, or `None` when it has no caption to drop on.
pub(crate) fn target_of(facts: &HostFacts) -> Option<WindowTarget> {
    let Geometry {
        caption,
        exclusions,
    } = geometry(&facts.caption)?;
    Some(WindowTarget {
        id: facts.id,
        frame: facts.caption.frame,
        work_area: facts.caption.work_area,
        monitor_dpi: facts.caption.monitor_dpi,
        caption,
        exclusions,
        label: facts.label,
    })
}

/// The perch target `facts` describe when `point` is a drop on it: inside its half-open
/// caption, which already ends left of every caption-button exclusion. A frame hit alone
/// isn't a drop.
pub fn drop_target(facts: &HostFacts, [x, y]: [i32; 2]) -> Option<WindowTarget> {
    target_of(facts).filter(|target| {
        let caption = target.caption.rect;
        (caption.left..caption.right).contains(&x) && (caption.top..caption.bottom).contains(&y)
    })
}

/// §10.4's caption geometry. When the title bar and buttons agree, the caption is the reported
/// bar left of the leftmost exclusion. Otherwise it's the standard band, scaled from host units
/// at `monitor_dpi / layout_dpi` rounding up, from `bounds`' top for a captioned host and from
/// `frame`'s top for one without a caption (revision 12's guess), cut to the frame and work
/// area. A usable reported button always stays an exclusion, read both ways while the DPIs
/// differ.
pub(crate) fn geometry(facts: &Facts) -> Option<Geometry> {
    let Facts {
        per_monitor_v2,
        captioned,
        bounds,
        frame,
        work_area,
        layout_dpi,
        monitor_dpi,
        title_bar,
        buttons,
        top_inset,
        button_width,
    } = *facts;
    if !per_monitor_v2 || layout_dpi == 0 || monitor_dpi == 0 {
        return None;
    }
    let scaled = layout_dpi != monitor_dpi;
    let length = |value: i32| scale(value, monitor_dpi, layout_dpi, Round::Up);
    let visible = frame.intersect(work_area);
    let usable = |rect: &PhysicalRect| rect.usable() && frame.contains(*rect);
    let origin = [bounds.left, bounds.top];
    let physical = buttons.map(|rect| rect.offset(origin)).filter(usable);
    let host_units = buttons
        .filter(|_| scaled)
        .map(|rect| {
            PhysicalRect {
                left: scale(rect.left, monitor_dpi, layout_dpi, Round::Down),
                top: scale(rect.top, monitor_dpi, layout_dpi, Round::Down),
                right: scale(rect.right, monitor_dpi, layout_dpi, Round::Up),
                bottom: scale(rect.bottom, monitor_dpi, layout_dpi, Round::Up),
            }
            .offset(origin)
        })
        .filter(usable);
    let top = if captioned { bounds.top } else { frame.top };
    let band = PhysicalRect {
        left: frame.left,
        top,
        right: frame.right,
        bottom: top.saturating_add(length(top_inset)),
    }
    .intersect(visible);
    let strip = PhysicalRect {
        left: frame
            .right
            .saturating_sub(length(button_width.saturating_mul(3))),
        ..band
    }
    .intersect(visible);
    let agreed = title_bar
        .zip(physical)
        .filter(|(bar, buttons)| bar.usable() && bar.contains(*buttons));
    let mut exclusions = Exclusions::default();
    for reading in [physical, host_units].into_iter().flatten() {
        exclusions.push(reading, Source::Reported);
    }
    if (agreed.is_none() || scaled) && strip.usable() {
        exclusions.push(strip, Source::Inferred);
    }
    let (area, source) =
        agreed.map_or((band, Source::Inferred), |(bar, _)| (bar, Source::Reported));
    let right = exclusions
        .iter()
        .map(|rect| rect.left)
        .fold(area.right, i32::min);
    let rect = PhysicalRect { right, ..area };
    rect.usable().then_some(Geometry {
        caption: Caption { rect, source },
        exclusions,
    })
}

#[derive(Clone, Copy)]
enum Round {
    Up,
    Down,
}

/// `value × to / from` in integer arithmetic.
fn scale(value: i32, to: u32, from: u32, round: Round) -> i32 {
    let product = i64::from(value) * i64::from(to);
    let from = i64::from(from);
    let scaled = match round {
        Round::Up => -(-product).div_euclid(from),
        Round::Down => product.div_euclid(from),
    };
    i32::try_from(scaled).unwrap_or(if scaled < 0 { i32::MIN } else { i32::MAX })
}

#[cfg(test)]
mod tests;
