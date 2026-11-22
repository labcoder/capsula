//! The brain's text estimate for drawing admission, independent of the shell's font backend.

use super::{MeasureError, TextMeasure, TextMetricsV1, TextStyle};
use crate::limits::MAX_COORD;

/// Line height in ems. The shell's Segoe UI is 1.33.
const LINE: f32 = 1.5;

/// Measures text at least as large as a shell whose text layout keeps three promises: no
/// character wider than [`advance`] gives it, lines no taller than 1.5 em, and breaks exactly
/// where this rule would break with the shell's own character widths. Under those promises every
/// label's lines, width and height come out at least the shell's, so every node, chip and
/// callout is at least as large, and so is the fill and outline charged for it.
///
/// The rule mirrors `compute_wrap_boundaries` in the shell's text layout at its pinned revision,
/// which shapes labels; its `wrap_line` breaks differently. Each `\n` starts a line. A
/// line may break before a word character that follows a space, or before any other character
/// but a space, once the line holds something besides spaces. When a line passes the width, it
/// breaks at the last such place since the previous break, or else before the character that
/// passed it.
///
/// shortcut: advances come from Segoe UI and its bold, Segoe UI Symbol, Emoji and Historic,
/// `Microsoft YaHei`, Yu Gothic, Malgun Gothic, Ebrima, Gadugi, Leelawadee UI, `MingLiU`, `SimSun`
/// and Arial on Windows 11. A font outside that set (macOS's are unmeasured), cuneiform (Segoe UI
/// Historic draws some at up to 4.6 em), breaks hidden inside one glyph (a ligature or an emoji
/// sequence) and a character drawn as several glyphs (the split vowels of Tamil, Malayalam or
/// Bengali, which give the shell a break inside it) can still under-count, and the shell's guard refuses that scene after the call
/// ended. A negotiated profile replaces these fallback numbers, not the shell's guard.
///
/// shortcut: dashes stay outside this bound, because a dashed edge's length follows the layout
/// rather than growing with it: a scene within a few dozen pieces of the dash limit whose dashed
/// edges come out longer under the shell's font can still be refused after the call ended. Draw
/// that overflow solid, or share one measure, once a late dash refusal is seen in use.
#[derive(Debug, Clone, Copy, Default)]
pub struct WideMeasure;

impl TextMeasure for WideMeasure {
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let size = dimensions(text, style, max_width, LINE, |ch| advance(ch, None))?;
        within_geometry(size).then_some(size).ok_or(MeasureError)
    }
}

/// An admission measure frozen to one validated connection profile, or the built-in table.
#[derive(Debug)]
pub struct ProfileMeasure {
    profile: Option<TextMetricsV1>,
    exceeded: bool,
}

impl ProfileMeasure {
    /// A missing or invalid profile uses the built-in table in its entirety.
    pub fn new(profile: Option<TextMetricsV1>) -> Self {
        Self {
            profile: profile.filter(TextMetricsV1::valid),
            exceeded: false,
        }
    }

    /// Whether a valid negotiated profile measured text beyond the scene geometry cap.
    pub fn geometry_exceeded(&self) -> bool {
        self.exceeded
    }
}

impl TextMeasure for ProfileMeasure {
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        let Some(profile) = self.profile.as_ref() else {
            return WideMeasure.measure(text, style, max_width);
        };
        let size = dimensions(text, style, max_width, em(profile.line_height), |ch| {
            advance(ch, Some(profile))
        })?;
        if within_geometry(size) {
            Ok(size)
        } else {
            self.exceeded = true;
            Err(MeasureError)
        }
    }
}

fn em(units: u16) -> f32 {
    f32::from(units) / 1024.0
}

fn within_geometry(size: [f32; 2]) -> bool {
    size.into_iter()
        .all(|value| (0.0..=MAX_COORD).contains(&value))
}

fn dimensions(
    text: &str,
    style: TextStyle,
    max_width: Option<f32>,
    line_height: f32,
    advance: impl Fn(char) -> f32,
) -> Result<[f32; 2], MeasureError> {
    if !style.size.is_finite()
        || style.size <= 0.0
        || style.size > MAX_COORD
        || max_width.is_some_and(|width| !width.is_finite() || width <= 0.0 || width > MAX_COORD)
    {
        return Err(MeasureError);
    }
    let limit = max_width.map_or(f32::INFINITY, |width| width / style.size);
    let mut width = 0.0_f32;
    let mut lines = 0.0_f32;
    for line in text.split('\n') {
        let (full, breaks) = wrap(line, limit, &advance);
        width = width.max(full.min(limit));
        lines += breaks + 1.0;
    }
    Ok([width * style.size, lines * line_height * style.size])
}

/// One line's unwrapped width in ems, and how many breaks the rule makes in it at `limit` ems.
fn wrap(line: &str, limit: f32, advance: &impl Fn(char) -> f32) -> (f32, f32) {
    let mut x = 0.0_f32;
    let mut start = (0, 0.0_f32);
    let mut candidate = None;
    let mut breaks = 0.0_f32;
    let mut text = false;
    let mut previous = '\0';
    for (index, ch) in line.chars().enumerate() {
        let opens = if word(ch) { previous == ' ' } else { ch != ' ' };
        if opens && text {
            candidate = Some((index, x));
        }
        text |= ch != ' ';
        let width = advance(ch);
        x += width;
        if x - start.1 > limit && index > start.0 {
            start = candidate.take().unwrap_or((index, x - width));
            breaks += 1.0;
        }
        previous = ch;
    }
    (x, breaks)
}

/// Characters the shell's text layout keeps with the one before: letters and digits of Latin,
/// Cyrillic, Vietnamese and Bengali text, and punctuation that joins or closes a word.
fn word(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '\u{00C0}'..='\u{024F}'
                | '\u{0300}'..='\u{036F}'
                | '\u{0400}'..='\u{04FF}'
                | '\u{0980}'..='\u{09FF}'
                | '\u{1E00}'..='\u{1EFF}'
                | '-'
                | '_'
                | '.'
                | '\''
                | '\u{2018}'
                | '\u{2019}'
                | '$'
                | '%'
                | '@'
                | '#'
                | '^'
                | '~'
                | ','
                | '='
                | ':'
                | ';'
                | '!'
                | ')'
                | ']'
                | '}'
                | '"'
                | '\u{201D}'
                | '\u{00BB}'
                | '\u{2026}'
                | '\u{22EF}'
                | '\u{202F}'
                | '\u{00A0}'
                | '\u{2011}'
        )
}

/// Printable ASCII from the space to `~`, in ems: the wider of regular and bold Segoe UI's
/// advance plus a tenth, rounded up to a hundredth, at most 1 em unless Segoe UI's own is wider
/// (bold `W` is 1.005 em).
///
/// These are the fallback advances when the shell doesn't supply a valid profile.
const ASCII: [f32; 95] = [
    0.31, 0.36, 0.55, 0.66, 0.64, 0.96, 0.94, 0.33, 0.41, 0.41, 0.51, 0.78, 0.30, 0.45, 0.30, 0.49,
    0.64, 0.64, 0.64, 0.64, 0.64, 0.64, 0.64, 0.64, 0.64, 0.64, 0.30, 0.30, 0.78, 0.78, 0.78, 0.50,
    1.00, 0.78, 0.71, 0.69, 0.82, 0.59, 0.58, 0.79, 0.85, 0.35, 0.49, 0.72, 0.57, 1.00, 0.87, 0.84,
    0.68, 0.84, 0.72, 0.62, 0.65, 0.80, 0.74, 1.01, 0.73, 0.67, 0.67, 0.41, 0.48, 0.41, 0.78, 0.46,
    0.35, 0.60, 0.69, 0.53, 0.69, 0.60, 0.43, 0.69, 0.67, 0.32, 0.32, 0.62, 0.32, 1.00, 0.67, 0.68,
    0.69, 0.69, 0.44, 0.49, 0.43, 0.67, 0.60, 0.88, 0.61, 0.60, 0.53, 0.41, 0.36, 0.41, 0.78,
];

/// The widest advance in ems the measured fonts give `ch`: printable ASCII from [`ASCII`], CJK
/// ideographs, kana, hangul and full-width forms at most 1, the rest of the Basic Multilingual
/// Plane at most 1.5, and the few wider characters, private use and everything past the plane at
/// most 2.
fn advance(ch: char, profile: Option<&TextMetricsV1>) -> f32 {
    let (range, built_in) = match ch {
        ' '..='~' => {
            let index = usize::from(u8::try_from(ch).expect("printable ASCII") - b' ');
            return profile.map_or(ASCII[index], |profile| em(profile.advances[index]));
        }
        '\u{1100}'..='\u{11FF}'
        | '\u{3040}'..='\u{30FF}'
        | '\u{3130}'..='\u{318F}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}' => (0, 1.0),
        '\0'..='\u{1F}'
        | '\u{7F}'
        | '\u{0603}'..='\u{0604}'
        | '\u{17D8}'
        | '\u{2031}'
        | '\u{2DBC}'
        | '\u{2E3B}'
        | '\u{A51E}'
        | '\u{E000}'..='\u{F8FF}'
        | '\u{FDF4}'
        | '\u{FDFC}'
        | '\u{10000}'.. => (2, 2.0),
        _ => (1, 1.5),
    };
    profile.map_or(built_in, |profile| em(profile.range_maxima[range]))
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "the helper rounds results to a thousandth of a pixel"
)]
mod tests {
    use super::*;

    /// Width and height at 10 px, to a thousandth of a pixel.
    fn measure(text: &str, max_width: Option<f32>) -> [f32; 2] {
        WideMeasure
            .measure(
                text,
                TextStyle {
                    size: 10.0,
                    bold: true,
                },
                max_width,
            )
            .expect("valid text")
            .map(|value| (value * 1000.0).round() / 1000.0)
    }

    #[test]
    fn lines_follow_newlines_and_each_line_is_one_and_a_half_ems() {
        assert_eq!(measure("", None), [0.0, 15.0]);
        assert_eq!(measure("m\n\nmmm", None), [30.0, 45.0]);
        assert_eq!(measure("mmm\n", Some(20.0)), [20.0, 45.0]);
    }

    #[test]
    fn characters_are_charged_at_their_widest_measured_advance() {
        assert_eq!(measure("i.m", None), [16.2, 15.0]);
        assert_eq!(measure("WW", None), [20.2, 15.0]);
        assert_eq!(measure("\u{2019}\u{4E2D}\u{042E}", None), [40.0, 15.0]);
        assert_eq!(measure("\u{1F680} \u{2E3B}\t\u{E000}", None), [83.1, 15.0]);
    }

    #[test]
    fn a_profile_changes_wrap_height_and_all_three_ranges() {
        let mut custom = ProfileMeasure::new(Some(TextMetricsV1 {
            advances: [2048; 95],
            line_height: 2048,
            range_maxima: [2048, 3072, 4096],
        }));
        let style = TextStyle {
            size: 10.0,
            bold: false,
        };
        assert_eq!(custom.measure("WW", style, Some(25.0)), Ok([25.0, 40.0]));
        assert_eq!(measure("WW", Some(25.0)), [20.2, 15.0]);
        assert_eq!(custom.measure("中Ω🚀", style, None), Ok([90.0, 20.0]));
    }

    #[test]
    fn only_valid_negotiated_dimensions_can_signal_geometry_overflow() {
        let metrics = TextMetricsV1 {
            advances: [8192; 95],
            line_height: 4096,
            range_maxima: [8192; 3],
        };
        let mut custom = ProfileMeasure::new(Some(metrics));
        let style = TextStyle {
            size: 10.0,
            bold: true,
        };
        assert_eq!(
            custom.measure(&"W".repeat(250), style, None),
            Ok([MAX_COORD, 40.0])
        );
        assert!(!custom.geometry_exceeded());
        assert_eq!(
            custom.measure(&"W".repeat(251), style, None),
            Err(MeasureError)
        );
        assert!(custom.geometry_exceeded());

        let mut invalid = metrics;
        invalid.advances[0] = 8193;
        for profile in [None, Some(invalid)] {
            let mut measure = ProfileMeasure::new(profile);
            assert_eq!(
                measure.measure("WW", style, None),
                WideMeasure.measure("WW", style, None)
            );
            assert!(!measure.geometry_exceeded());
        }
    }

    #[test]
    fn a_word_that_passes_the_width_alone_is_broken_before_the_character_that_passes_it() {
        assert_eq!(measure("mmmmmmmmmmm mm", Some(100.0)), [100.0, 30.0]);
        assert_eq!(measure("mmmmm-mmmmm-mmmmm", Some(100.0)), [100.0, 30.0]);
        assert_eq!(measure(&"m".repeat(21), Some(100.0)), [100.0, 45.0]);
    }

    #[test]
    fn a_line_breaks_at_its_last_opening_before_the_width() {
        assert_eq!(measure("mmmm mmmm mmmm", Some(100.0)), [100.0, 30.0]);
        assert_eq!(measure("mmmmmm/mmmmmm", Some(100.0)), [100.0, 30.0]);
        assert_eq!(measure("mmmmmm?mmmmmm/mmmmmm", Some(100.0)), [100.0, 45.0]);
        assert_eq!(
            measure(
                "\u{6570}\u{636E}\u{5E93}\u{8FDE}\u{63A5}\u{6C60}",
                Some(50.0)
            ),
            [50.0, 30.0]
        );
    }

    #[test]
    fn leading_spaces_open_no_break_and_text_that_fits_keeps_its_own_width() {
        assert_eq!(measure("     mmmmmmmmmm", Some(100.0)), [100.0, 30.0]);
        assert_eq!(measure("mmm mmm", None), [63.1, 15.0]);
    }

    #[test]
    fn invalid_sizes_and_widths_and_oversized_results_fail() {
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_COORD + 1.0] {
            assert_eq!(
                WideMeasure.measure("m", TextStyle { size, bold: false }, None),
                Err(MeasureError)
            );
        }
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_COORD + 1.0] {
            assert_eq!(
                WideMeasure.measure(
                    "m",
                    TextStyle {
                        size: 10.0,
                        bold: false
                    },
                    Some(width)
                ),
                Err(MeasureError)
            );
        }
        assert_eq!(
            WideMeasure.measure(
                "m",
                TextStyle {
                    size: MAX_COORD,
                    bold: false
                },
                None
            ),
            Err(MeasureError)
        );
    }
}
