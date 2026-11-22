//! Worker-local GPUI shaping; Noop tests cover arithmetic, not real font performance.

use std::sync::Arc;

use gpui_kit::{Font, FontWeight, SharedString, TextRun, TextSystem, WindowTextSystem, px};
use kumochi_scene::display::{MeasureError, TextMeasure, TextStyle};
use kumochi_scene::limits::MAX_COORD;

pub(crate) struct Measure {
    text: WindowTextSystem,
    family: SharedString,
}

impl Measure {
    pub(crate) fn new(text: Arc<TextSystem>, family: SharedString) -> Self {
        Self {
            text: WindowTextSystem::new(text),
            family,
        }
    }
}

impl TextMeasure for Measure {
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        if !style.size.is_finite()
            || style.size <= 0.0
            || style.size > MAX_COORD
            || max_width
                .is_some_and(|width| !width.is_finite() || width <= 0.0 || width > MAX_COORD)
        {
            return Err(MeasureError);
        }
        let font = Font {
            family: self.family.clone(),
            weight: if style.bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            ..Font::default()
        };
        // shortcut: no resolvable host font aborts the shell; revisit if drawings name fonts or GPUI gains fallible resolve.
        let font_id = self.text.resolve_font(&font);
        let size = px(style.size);
        let height = f32::from(self.text.ascent(font_id, size)).abs()
            + f32::from(self.text.descent(font_id, size)).abs();
        if !height.is_finite() || height <= 0.0 {
            return Err(MeasureError);
        }
        let runs = [TextRun {
            len: text.len(),
            font,
            ..TextRun::default()
        }];
        let lines = self
            .text
            .shape_text(text.to_owned().into(), size, &runs, max_width.map(px), None)
            .map_err(|_| MeasureError)?;
        let mut measured = [0.0_f32, 0.0_f32];
        for line in lines {
            let size = line.size(px(height));
            measured[0] = measured[0].max(f32::from(size.width));
            measured[1] += f32::from(size.height);
        }
        if measured
            .iter()
            .all(|value| value.is_finite() && (0.0..=MAX_COORD).contains(value))
        {
            Ok(measured)
        } else {
            Err(MeasureError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual quiet-Rez real-font worker measurement, no window or input"]
    fn measure_native_fonts_on_the_gpui_background_executor() {
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };

        let app = gpui_kit::application();
        let text_system = app.text_system();
        let (sender, receiver) = mpsc::sync_channel(1);
        app.background_executor()
            .spawn(async move {
                let mut measure = Measure::new(text_system, Font::default().family);
                let text = "A wrapped line with Unicode Ω 中 🙂. ".repeat(10);
                let mut samples = Vec::new();
                let mut dimensions = Vec::new();
                let mut cold_ns = 0;
                for sample in 0..24 {
                    let text = format!("{text}{sample}");
                    let start = Instant::now();
                    let sizes = [false, true].map(|bold| {
                        measure
                            .measure(&text, TextStyle { size: 15.0, bold }, Some(360.0))
                            .expect("native worker shaping")
                    });
                    let elapsed = start.elapsed().as_nanos();
                    if sample == 0 {
                        cold_ns = elapsed;
                        dimensions.extend(sizes);
                    }
                    if sample >= 8 {
                        samples.push(elapsed);
                    }
                }
                sender
                    .send((cold_ns, dimensions, samples))
                    .expect("measurement receiver");
            })
            .detach();
        let (cold_ns, dimensions, samples) = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("bounded native worker completion");
        for size in dimensions {
            assert!(size[0] > 0.0 && size[0] <= 360.0);
            assert!(size[1] > 15.0 && size[1].is_finite());
        }
        println!(
            "native_text_worker regular_and_bold=true unique_text_per_sample=true logical_width=360 font_size=15 warmups=8 samples=16 cold_ns={cold_ns}"
        );
        for (sample, elapsed_ns) in samples.into_iter().enumerate() {
            println!("native_text_worker sample={sample} elapsed_ns={elapsed_ns}");
        }
    }

    /// The brain's wide measure bounds the shipped font: on this corpus its lines, width and height
    /// are never below the shell's, nor its width on a run of any one printable ASCII character,
    /// and the shell's lines are at most 1.5 em.
    #[test]
    fn the_brain_measure_bounds_the_shipped_font() {
        use kumochi_scene::display::WideMeasure;
        const CORPUS: &[&str] = &[
            "Validate the input",
            "WWWWWWWWWWWWWWWWWWWW",
            "aaaaaaaaaaa bb",
            "aaaaa-bbbbb-ccccc-ddddd-eeeee",
            "state-of-the-art non-English well\u{2026} don\u{2019}t \u{201C}quoted\u{201D}",
            "/api/v1/users/{id}/permissions?expand=roles&limit=20",
            "zed-industries/zed a=1&b=2 foo?b=2 (retry) [x] 100% #tag @name",
            "supercalifragilisticexpialidocious_and_then_some",
            "a\n\nb\n",
            "\u{6570}\u{636E}\u{5E93}\u{8FDE}\u{63A5}\u{6C60}\u{5DF2}\u{6EE1}\u{FF0C}\u{8BF7}\u{7A0D}\u{540E}\u{91CD}\u{8BD5}",
            "\u{3053}\u{3093}\u{306B}\u{3061}\u{306F}\u{30AB}\u{30BF}\u{30AB}\u{30CA}",
            "\u{D55C}\u{AD6D}\u{C5B4} \u{D14C}\u{C2A4}\u{D2B8}",
            "\u{41F}\u{440}\u{43E}\u{432}\u{435}\u{440}\u{43A}\u{430} \u{42E}\u{43D}\u{438}\u{43A}\u{43E}\u{434}\u{430} \u{429}\u{428}\u{416}",
            "\u{645}\u{631}\u{62D}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645} \u{633}\u{634}\u{635}\u{636}",
            "\u{1F680} Launch \u{2705} Done \u{26A0}\u{FE0F} Careful \u{2764}\u{FE0F}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{1F1EF}\u{1F1F5} \u{1F44D}\u{1F3FD}",
            "e\u{301}t\u{e9} n\u{303}o \u{1EA0}\u{1EA1} \u{1D1}",
            "\u{2030} \u{2031} \u{2E3A} \u{2E3B} \u{FDFD} \u{2014}\u{2014} \u{2192} \u{2190}",
            "\ttabbed\tcolumns\t",
            "\u{1C4}\u{1F1} \u{478} \u{2DBC} \u{A51E} \u{17D8}",
        ];
        let app = gpui_kit::application();
        let mut shell = Measure::new(
            app.text_system(),
            gpui_kit::component::Theme::default().font_family,
        );
        let regular = TextStyle {
            size: 15.0,
            bold: false,
        };
        for (text, least, font) in [("\u{4E2D}", 0.9, "CJK"), ("\u{1F680}", 1.2, "emoji")] {
            let width = shell.measure(text, regular, None).expect("shaped")[0];
            assert!(
                width >= least * 15.0,
                "the {font} fallback font is missing: {width} px at 15"
            );
        }
        for (size, bold) in [(15.0, true), (15.0, false), (13.0, false)] {
            let style = TextStyle { size, bold };
            let line = shell.measure("", style, None).expect("an empty line")[1];
            assert!(line <= 1.5 * size, "line height {line} at {size}");
            for text in CORPUS {
                for width in [None, Some(180.0), Some(220.0), Some(40.0)] {
                    let real = shell.measure(text, style, width).expect("shaped");
                    let wide = WideMeasure.measure(text, style, width).expect("measured");
                    let lines = [real[1] / line, wide[1] / (1.5 * size)].map(f32::round);
                    assert!(
                        real[0] <= wide[0] && real[1] <= wide[1] && lines[0] <= lines[1],
                        "{text:?} at {size} bold {bold} width {width:?}: shell {real:?}, brain {wide:?}"
                    );
                }
            }
            for ch in ' '..='~' {
                let text = ch.to_string().repeat(20);
                let real = shell.measure(&text, style, None).expect("shaped")[0];
                let wide = WideMeasure.measure(&text, style, None).expect("measured")[0];
                assert!(
                    real <= wide,
                    "{ch:?} at {size} bold {bold}: shell {real}, brain {wide}"
                );
            }
        }
    }

    #[gpui::test]
    fn the_conformance_font_is_the_one_drawings_measure_with(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::component::ActiveTheme as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            assert_eq!(
                cx.theme().font_family,
                gpui_kit::component::Theme::default().font_family
            );
        });
    }

    fn measure() -> Measure {
        Measure::new(
            Arc::new(TextSystem::new(Arc::new(gpui_kit::NoopTextSystem::new()))),
            Font::default().family,
        )
    }

    #[test]
    fn invalid_host_dimensions_fail_before_shaping() {
        let mut measure = measure();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_COORD + 1.0] {
            assert_eq!(
                measure.measure("text", TextStyle { size, bold: false }, None),
                Err(MeasureError)
            );
        }
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_COORD + 1.0] {
            assert_eq!(
                measure.measure(
                    "text",
                    TextStyle {
                        size: 10.0,
                        bold: false
                    },
                    Some(width)
                ),
                Err(MeasureError)
            );
        }
    }

    #[test]
    fn multiline_measurement_uses_the_widest_line_and_combined_height() {
        for (text, width, bold) in [
            ("ab\nc", None, false),
            ("abcd", Some(12.0), false),
            ("abcd", Some(12.0), true),
        ] {
            let style = TextStyle { size: 10.0, bold };
            assert_eq!(measure().measure(text, style, width), Ok([12.0, 26.0]));
        }
    }

    #[test]
    fn empty_and_blank_lines_keep_line_height_and_oversized_results_fail() {
        let mut measure = measure();
        let style = TextStyle {
            size: 10.0,
            bold: false,
        };
        assert_eq!(measure.measure("", style, None), Ok([0.0, 13.0]));
        assert_eq!(measure.measure("\n", style, None), Ok([0.0, 26.0]));
        assert_eq!(
            measure.measure(
                "a",
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
