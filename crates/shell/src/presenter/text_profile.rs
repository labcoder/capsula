//! The host font selected for a connection and the fixed-corpus startup sample.

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::{
    App, FontWeight, Global, PlatformTextSystem, SharedString, TextRun, TextSystem,
    WindowTextSystem, px,
};
use kumochi_protocol::message::TextMetricsV1;
use kumochi_shell_core::link::DrawingText;

use super::{drawing_font, resolve_font};

const PROBE_BUDGET: Duration = Duration::from_millis(250);
const PROBES: [&[&str]; 3] = [
    &["中", "カ", "한", "Ｗ"],
    &["Ω", "Ж", "م", "‰", "⸺"],
    &["‱", "⸻", "🚀", "👍"],
];

pub(crate) struct PairFont {
    family: SharedString,
    backend: Arc<dyn PlatformTextSystem>,
}

impl Global for PairFont {}

impl PairFont {
    pub(crate) fn pin(family: &Arc<str>, cx: &mut App) {
        if let Some(font) = cx.try_global::<Self>()
            && font.family.as_str() != family.as_ref()
        {
            let backend = Arc::clone(&font.backend);
            cx.set_global(Self {
                family: SharedString::from(Arc::clone(family)),
                backend,
            });
        }
    }

    pub(crate) fn selection(cx: &App) -> (&SharedString, Option<&Arc<dyn PlatformTextSystem>>) {
        cx.try_global::<Self>().map_or_else(
            || (&cx.theme().font_family, None),
            |font| (&font.family, Some(&font.backend)),
        )
    }
}

pub(crate) fn text_profiles(
    cx: &mut App,
    backend: Arc<dyn PlatformTextSystem>,
) -> impl FnMut(Instant) -> DrawingText + Send + 'static + use<> {
    let initial = cx.theme().font_family.clone();
    cx.set_global(PairFont {
        family: initial.clone(),
        backend: Arc::clone(&backend),
    });
    let (changed, updates) = async_channel::bounded(1);
    let mut observed = initial.clone();
    cx.observe_global::<Theme>(move |cx| {
        let family = &cx.theme().font_family;
        if family != &observed {
            observed = family.clone();
            let _published = changed.force_send(Arc::<str>::from(family.as_str()));
        }
    })
    .detach();

    let mut family: Arc<str> = Arc::from(initial.as_str());
    let mut cached = None;
    let mut outstanding: Option<mpsc::Receiver<Option<TextMetricsV1>>> = None;
    let text = Arc::clone(cx.text_system());
    let executor = cx.background_executor().clone();
    move |connect_by| {
        if let Ok(latest) = updates.try_recv()
            && family != latest
        {
            family = latest;
            cached = None;
        }
        let probe_by = connect_by.min(Instant::now() + PROBE_BUDGET);
        if outstanding
            .as_ref()
            .is_some_and(|done| !matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)))
        {
            outstanding = None;
        }
        if cached.is_none() && outstanding.is_none() && Instant::now() < probe_by {
            let (done, result) = mpsc::sync_channel(1);
            let family = SharedString::from(Arc::clone(&family));
            let (text, backend) = (Arc::clone(&text), Arc::clone(&backend));
            executor
                .spawn(async move {
                    let _delivered = done.send(sample(text, backend.as_ref(), &family, probe_by));
                })
                .detach();
            match result.recv_timeout(probe_by.saturating_duration_since(Instant::now())) {
                Ok(profile) => cached = profile,
                Err(mpsc::RecvTimeoutError::Timeout) => outstanding = Some(result),
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }
        DrawingText {
            profile: cached,
            font_family: Some(Arc::clone(&family)),
        }
    }
}

fn sample(
    text: Arc<TextSystem>,
    backend: &dyn PlatformTextSystem,
    family: &SharedString,
    deadline: Instant,
) -> Option<TextMetricsV1> {
    let shaper = WindowTextSystem::new(text);
    let mut advances = [0.0_f32; 95];
    let mut line_em = 0.0_f32;
    let mut ranges = [0.0_f32; 3];
    let base = kumochi_scene::theme::Theme::default().font_size;
    for weight in [FontWeight::NORMAL, FontWeight::BOLD] {
        if Instant::now() >= deadline {
            return None;
        }
        let font = drawing_font(family.clone(), weight);
        let id = resolve_font(&shaper, Some(backend), &font).ok()?;
        for size in [base, base - 2.0, base * 0.8] {
            let size_px = px(size);
            let line = shaper.ascent(id, size_px).abs() + shaper.descent(id, size_px).abs();
            let height = f32::from(line) / size;
            if !height.is_finite() || height <= 0.0 {
                return None;
            }
            line_em = line_em.max(height);
            let mut width = |text: &str| {
                if Instant::now() >= deadline {
                    return None;
                }
                let runs = [TextRun {
                    len: text.len(),
                    font: font.clone(),
                    ..TextRun::default()
                }];
                let lines = shaper
                    .shape_text(SharedString::new(text), size_px, &runs, None, None)
                    .ok()?;
                let line = lines.first()?;
                let mut glyphs = line.runs().iter().flat_map(|run| &run.glyphs).peekable();
                let width = f32::from(line.width()) / size;
                if glyphs.peek().is_none()
                    || glyphs.any(|glyph| glyph.id.0 == 0)
                    || !width.is_finite()
                    || width <= 0.0
                {
                    return None;
                }
                let height = f32::from(line.ascent().abs() + line.descent().abs()) / size;
                if !height.is_finite() || height <= 0.0 {
                    return None;
                }
                line_em = line_em.max(height);
                Some(width)
            };
            for (byte, widest) in (b' '..=b'~').zip(&mut advances) {
                let repeated = [byte; 4];
                let text = std::str::from_utf8(&repeated).ok()?;
                *widest = widest.max(width(text.get(..1)?)?).max(width(text)? / 4.0);
            }
            for (samples, widest) in PROBES.into_iter().zip(&mut ranges) {
                for text in samples {
                    *widest = widest.max(width(text)?);
                }
            }
        }
    }
    let mut profile = TextMetricsV1 {
        advances: [0; 95],
        line_height: padded(line_em, 4096)?,
        range_maxima: [1024, 1536, 2048],
    };
    for (value, measured) in profile.advances.iter_mut().zip(advances) {
        *value = padded(measured, 8192)?;
    }
    for (floor, measured) in profile.range_maxima.iter_mut().zip(ranges) {
        *floor = (*floor).max(padded(measured, 8192)?);
    }
    (Instant::now() < deadline && profile.valid()).then_some(profile)
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the rounded value is checked against a u16 range before conversion"
)]
fn padded(em: f32, max: u16) -> Option<u16> {
    let units = (f64::from(em) * 1024.0 * 1.10).ceil();
    if !(1.0..=f64::from(max)).contains(&units) {
        return None;
    }
    Some(units as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::NoopTextSystem;
    use kumochi_scene::display::{ProfileMeasure, TextMeasure, TextStyle};

    #[test]
    fn fixed_corpus_profile_uses_upward_padding_and_wide_floors() {
        let backend: Arc<dyn PlatformTextSystem> = Arc::new(NoopTextSystem::new());
        let text = Arc::new(TextSystem::new(Arc::clone(&backend)));
        let family = Theme::default().font_family;
        assert_eq!(
            sample(Arc::clone(&text), backend.as_ref(), &family, Instant::now()),
            None
        );
        let profile = sample(
            text,
            backend.as_ref(),
            &family,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("the deterministic shaper covers the complete corpus");
        assert_eq!(profile.advances, [676; 95]);
        assert_eq!(profile.line_height, 1465);
        assert_eq!(profile.range_maxima, [1024, 1536, 2048]);
        let got = ProfileMeasure::new(Some(profile))
            .measure(
                "WW",
                TextStyle {
                    size: 10.0,
                    bold: false,
                },
                None,
            )
            .expect("the scene consumes the negotiated profile");
        assert_eq!(
            got.map(f32::to_bits),
            [13.203_125_f32, 14.306_641_f32].map(f32::to_bits)
        );
    }

    #[gpui::test]
    fn a_theme_change_selects_the_next_attempt_without_changing_the_pair(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let mut profiles = cx.update(|cx| {
            gpui_kit::init(cx);
            text_profiles(cx, Arc::new(NoopTextSystem::new()))
        });
        let first = profiles(Instant::now()).font_family.unwrap();
        cx.update(|cx| {
            PairFont::pin(&first, cx);
            Theme::update(cx, |theme| theme.font_family = "next font".into());
        });
        let next = profiles(Instant::now()).font_family.unwrap();
        assert_eq!(next.as_ref(), "next font");
        cx.update(|cx| {
            assert_eq!(PairFont::selection(cx).0.as_str(), first.as_ref());
            PairFont::pin(&next, cx);
            assert_eq!(PairFont::selection(cx).0.as_str(), "next font");
        });
    }
}
