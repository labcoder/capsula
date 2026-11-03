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

    #[test]
    fn the_app_text_system_can_cross_to_a_worker_without_a_window() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<TextSystem>();
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
        let measured = measure()
            .measure(
                "ab\nc",
                TextStyle {
                    size: 10.0,
                    bold: false,
                },
                None,
            )
            .expect("GPUI test shaping");
        assert_eq!(
            measured.map(f32::to_bits),
            [12.0_f32, 26.0].map(f32::to_bits)
        );
    }

    #[test]
    fn worker_local_noop_shaping_wraps_regular_and_bold_dimensions() {
        let text = Arc::new(TextSystem::new(Arc::new(gpui_kit::NoopTextSystem::new())));
        let results = std::thread::spawn(move || {
            let mut measure = Measure::new(text, Font::default().family);
            [false, true]
                .map(|bold| measure.measure("abcd", TextStyle { size: 10.0, bold }, Some(12.0)))
        })
        .join()
        .expect("worker completed");
        assert_eq!(results, [Ok([12.0, 26.0]), Ok([12.0, 26.0])]);
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
