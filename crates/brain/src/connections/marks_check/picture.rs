//! The check's pictures: six made-up layouts drawn at the display's size, with every thing's box
//! known, shrunk and encoded the way the screen worker does it.

use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;

use super::DISPLAY;

const QUALITY: u8 = 85;

const BUTTONS: [&str; 6] = ["SAVE", "EXPORT", "SHARE", "PRINT", "DELETE", "SEND"];
const FIELDS: [&str; 6] = [
    "SEARCH ORDERS",
    "FIND A FILE",
    "TYPE A NAME",
    "FILTER ROWS",
    "SEARCH MAIL",
    "ENTER CITY",
];
const PANELS: [&str; 6] = [
    "FILTERS", "DETAILS", "HISTORY", "SETTINGS", "PEOPLE", "NOTES",
];
const TEXTS: [&str; 12] = [
    "WEEKLY REPORT",
    "DUE ON FRIDAY",
    "TOTAL 1240",
    "LAST SYNC 9 AM",
    "THREE NEW ITEMS",
    "DRAFT SAVED",
    "SHIPPED TODAY",
    "OPEN TICKETS 12",
    "BUDGET Q3",
    "MEETING AT NOON",
    "STORAGE 82 PERCENT",
    "VERSION 4 2",
];
/// A 5×7 capital font: A to Z, then 0 to 9; each row's low five bits, left to right.
const FONT: [[u8; 7]; 36] = [
    [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
    [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
    [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
    [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
    [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
    [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
    [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
    [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
    [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
    [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
    [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
    [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
    [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
    [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
    [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
    [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
    [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Button,
    Field,
    Panel,
    Row,
    /// A line of text this many pixels tall.
    Text(u32),
}

/// One thing in a layout: its kind, its label and its box in display pixels.
pub(super) struct Thing {
    pub(super) kind: Kind,
    pub(super) label: &'static str,
    pub(super) rect: [u32; 4],
}

impl Thing {
    pub(super) fn word(&self) -> String {
        match self.kind {
            Kind::Button => "button".to_owned(),
            Kind::Field => "field".to_owned(),
            Kind::Panel => "panel".to_owned(),
            Kind::Row => "row".to_owned(),
            Kind::Text(height) => format!("text{height}"),
        }
    }

    pub(super) fn named(&self) -> String {
        match self.kind {
            Kind::Button => format!("the {} button", self.label),
            Kind::Field => format!("the {} field", self.label),
            Kind::Panel => format!("the {} panel", self.label),
            Kind::Row => format!("the {} row", self.label),
            Kind::Text(_) => format!("the line that reads {}", self.label),
        }
    }
}

pub(super) fn index(value: u32) -> usize {
    usize::try_from(value).expect("a small index")
}

/// Layout `k`'s ten things: three buttons, a field, a panel, three rows and two lines of text,
/// the lines 11 to 16 pixels tall across the layouts.
pub(super) fn layout(k: u32) -> Vec<Thing> {
    let mut things = Vec::with_capacity(10);
    for b in 0..3 {
        things.push(Thing {
            kind: Kind::Button,
            label: BUTTONS[index((k * 3 + b) % 6)],
            rect: [1500 + b * 210 + k * 13, 24 + k * 8, 180, 44],
        });
    }
    things.push(Thing {
        kind: Kind::Field,
        label: FIELDS[index(k)],
        rect: [120 + k * 20, 140 + k * 10, 640, 48],
    });
    things.push(Thing {
        kind: Kind::Panel,
        label: PANELS[index(k)],
        rect: [60, 260, 420, 700 + k * 20],
    });
    for (r, label) in ["INVOICE 2041", "INVOICE 2042", "INVOICE 2043"]
        .into_iter()
        .enumerate()
    {
        let r = u32::try_from(r).expect("three rows");
        things.push(Thing {
            kind: Kind::Row,
            label,
            rect: [560, 300 + r * 72 + k * 4, 1400, 56],
        });
    }
    for t in 0..2 {
        let height = 11 + (k + t * 3) % 6;
        let label = TEXTS[index(k * 2 + t)];
        things.push(Thing {
            kind: Kind::Text(height),
            label,
            rect: [600, 560 + t * 60 + k * 7, text_width(label, height), height],
        });
    }
    things
}

/// The ink width of `label` drawn `height` pixels tall: six cells a character, less the last gap.
pub(super) fn text_width(label: &str, height: u32) -> u32 {
    let cells = u32::try_from(label.chars().count() * 6 - 1).expect("a short label");
    (cells * height).div_ceil(7)
}

fn glyph(c: char) -> [u8; 7] {
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .find(c)
        .map_or([0; 7], |at| FONT[at])
}

pub(super) struct Canvas {
    pub(super) rgb: Vec<u8>,
    width: u32,
}

impl Canvas {
    pub(super) fn new([width, height]: [u32; 2], ground: [u8; 3]) -> Self {
        let pixels = index(width * height);
        Self {
            rgb: ground.repeat(pixels),
            width,
        }
    }

    fn set(&mut self, x: u32, y: u32, color: [u8; 3]) {
        let at = index((y * self.width + x) * 3);
        self.rgb[at..at + 3].copy_from_slice(&color);
    }

    fn fill(&mut self, [x, y, w, h]: [u32; 4], color: [u8; 3]) {
        for py in y..y + h {
            for px in x..x + w {
                self.set(px, py, color);
            }
        }
    }

    fn frame(&mut self, [x, y, w, h]: [u32; 4], color: [u8; 3]) {
        self.fill([x, y, w, 2], color);
        self.fill([x, y + h - 2, w, 2], color);
        self.fill([x, y, 2, h], color);
        self.fill([x + w - 2, y, 2, h], color);
    }

    /// Draws `label` with its ink's top-left at `x`, `y`, `height` pixels tall: exactly the box
    /// [`text_width`] gives, since every capital and digit touches its cell's four edges.
    pub(super) fn text(&mut self, x: u32, y: u32, height: u32, label: &str, color: [u8; 3]) {
        let glyphs: Vec<[u8; 7]> = label.chars().map(glyph).collect();
        for px in 0..text_width(label, height) {
            let cell = index(px * 7 / height);
            let (Some(rows), column) = (glyphs.get(cell / 6), cell % 6) else {
                continue;
            };
            if column == 5 {
                continue;
            }
            for py in 0..height {
                let row = index((py * 7 / height).min(6));
                if rows[row] >> (4 - column) & 1 == 1 {
                    self.set(x + px, y + py, color);
                }
            }
        }
    }
}

/// Layout `k` drawn at the display's size: light grounds for even layouts, dark for odd.
pub(super) fn picture(k: u32, things: &[Thing]) -> Vec<u8> {
    let dark = k % 2 == 1;
    let (ground, ink, field, row) = if dark {
        ([30, 31, 34], [230, 230, 232], [50, 52, 56], [44, 46, 50])
    } else {
        (
            [240, 241, 243],
            [24, 24, 26],
            [255, 255, 255],
            [226, 229, 234],
        )
    };
    let mut canvas = Canvas::new(DISPLAY.size, ground);
    for thing in things {
        let [left, top, width, height] = thing.rect;
        match thing.kind {
            Kind::Button => {
                canvas.fill(thing.rect, [52, 110, 235]);
                let inset = (width - text_width(thing.label, 14)) / 2;
                canvas.text(
                    left + inset,
                    top + (height - 14) / 2,
                    14,
                    thing.label,
                    [255, 255, 255],
                );
            }
            Kind::Field => {
                canvas.fill(thing.rect, field);
                canvas.frame(thing.rect, [150, 150, 150]);
                canvas.text(
                    left + 16,
                    top + (height - 14) / 2,
                    14,
                    thing.label,
                    [130, 130, 130],
                );
            }
            Kind::Panel => {
                canvas.frame(thing.rect, [120, 120, 120]);
                canvas.text(left + 16, top + 16, 16, thing.label, ink);
            }
            Kind::Row => {
                canvas.fill(thing.rect, row);
                canvas.text(left + 20, top + (height - 14) / 2, 14, thing.label, ink);
            }
            Kind::Text(size) => canvas.text(left, top, size, thing.label, ink),
        }
    }
    canvas.rgb
}

/// Area-averages RGB from `from` to `to` pixels, as the hold's encoder does.
pub(super) fn shrink(rgb: &[u8], from: [u32; 2], to: [u32; 2]) -> Vec<u8> {
    if from == to {
        return rgb.to_vec();
    }
    let [sw, sh] = from.map(u64::from);
    let [dw, dh] = to.map(u64::from);
    let mut out = Vec::with_capacity(index(to[0] * to[1] * 3));
    for y in 0..dh {
        let (y0, y1) = (y * sh, (y + 1) * sh);
        for x in 0..dw {
            let (x0, x1) = (x * sw, (x + 1) * sw);
            let mut sums = [0_u64; 3];
            for sy in y0 / dh..y1.div_ceil(dh) {
                let wy = y1.min((sy + 1) * dh) - y0.max(sy * dh);
                for sx in x0 / dw..x1.div_ceil(dw) {
                    let wx = x1.min((sx + 1) * dw) - x0.max(sx * dw);
                    let at = usize::try_from((sy * sw + sx) * 3).expect("inside the picture");
                    for (sum, value) in sums.iter_mut().zip(&rgb[at..at + 3]) {
                        *sum += u64::from(*value) * wx * wy;
                    }
                }
            }
            let area = sw * sh;
            out.extend(sums.map(|sum| u8::try_from((sum + area / 2) / area).unwrap_or(u8::MAX)));
        }
    }
    out
}

pub(super) fn jpeg(rgb: &[u8], [width, height]: [u32; 2]) -> Option<Vec<u8>> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, QUALITY)
        .encode(rgb, width, height, ExtendedColorType::Rgb8)
        .ok()?;
    Some(jpeg)
}
