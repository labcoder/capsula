//! The diagnostic-only animated painter fixture: real resting frame items from one native-font
//! preparation, painted by the production `paint_item` in a click-through window over the
//! primary display. It never activates a view, conducts or sends `StepShown`; its motion is
//! fixture-local translation, opacity, reveal and dash flow on cloned items, never font size.

use std::sync::{Arc, atomic::AtomicBool};
#[cfg(windows)]
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

#[cfg(windows)]
use gpui_kit::component::{ActiveTheme as _, Theme as UiTheme, theme::ThemeMode};
#[cfg(windows)]
use gpui_kit::{AppContext as _, Context, IntoElement, Render, Styled as _, Window, canvas};
#[cfg(windows)]
use kumochi_protocol::message::MaxFrameRate;
use kumochi_scene::{
    Admission, Frame, FrameItem, GlobalBudget, Record, SceneState, SceneView,
    display::{PaintWork, Prim, TextMeasure},
    geom::{Affine, Vec2},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    parse_record,
    theme::Theme,
};

#[cfg(windows)]
use super::{
    frame_rate::FrameLoop, line::on_next_frame, options, overlay, paint_cache::PaintScene,
};

#[cfg(windows)]
const LIFETIME: Duration = Duration::from_secs(60);
/// Fixture-local motion runs only between these seconds; outside them nothing asks for frames.
#[cfg(windows)]
const START: f32 = 10.0;
#[cfg(windows)]
const END: f32 = 40.0;

type PaintUnit = (FrameItem, Arc<[Prim]>);

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Dry,
    #[cfg(windows)]
    Rest,
    #[cfg(windows)]
    Animated,
    #[cfg(windows)]
    Capped,
    #[cfg(windows)]
    Hide,
    /// Four copies of the animated picture, each item with its own paint cache, capped at 144.
    #[cfg(windows)]
    FullRate,
    /// The at-caps picture, capped at 144.
    #[cfg(windows)]
    CappedFullRate,
}

impl Mode {
    fn capped(self) -> bool {
        match self {
            Self::Dry => false,
            #[cfg(windows)]
            Self::Capped | Self::CappedFullRate => true,
            #[cfg(windows)]
            Self::Rest | Self::Animated | Self::Hide | Self::FullRate => false,
        }
    }
}

/// A rounded bordered card with a label, a rounded rect the fixture turns, a translucent
/// self-crossing stroke, a dashed line, a translucent star fill and a note the fixture turns.
const STEP: &str = r##"{"step":{"id":"fixture","put":[{"shape":{"id":"card","kind":"rect","box":[120,120,360,220],"radius":8,"fill":"#1d1b26","color":"#3b82f6","width":3,"label":"Upright label"}},{"shape":{"id":"tilted","kind":"rect","box":[560,140,220,140],"radius":8,"fill":"#7c3aed","color":"#e5e7eb","width":2}},{"line":{"id":"cross","from":[120,420],"via":[[520,620],[520,420]],"to":[120,620],"width":24,"color":"#22c55e","opacity":0.5}},{"line":{"id":"dashed","from":[600,420],"to":[1000,560],"width":3,"color":"#f59e0b","dash":true}},{"shape":{"id":"star","kind":"polygon","points":[[800,700],[860,880],[700,770],[900,770],[740,880]],"fill":"#ef4444","opacity":0.5}},{"text":{"id":"note","at":[120,720],"text":"Turned anchor, upright glyphs","size":18,"bold":true}}]}}"##;

/// Item indexes in handle order the fixture turns, each about its own pivot.
const TURNED: [(usize, Vec2); 2] = [
    (1, Vec2 { x: 670.0, y: 210.0 }),
    (5, Vec2 { x: 300.0, y: 730.0 }),
];

/// The fixture step prepared with `measure` at rest: each paint unit with its own primitives,
/// in paint order, and the rest's paint work.
pub(super) fn resting(
    measure: &mut dyn TextMeasure,
    default_canvas: [f32; 2],
    mode: Mode,
) -> (Vec<PaintUnit>, PaintWork) {
    let header =
        br#"{"begin":{"v":1,"scene":"fixture","surface":"overlay","alt":"painter fixture"}}"#;
    let Ok(Record::Begin(begin)) = parse_record(header) else {
        panic!("a begin record");
    };
    let step = if mode.capped() {
        crate::presenter::tests::cap_measurements::drawing(0, 40)
    } else {
        let Ok(Record::Step(step)) = parse_record(STEP.as_bytes()) else {
            panic!("a step record");
        };
        step
    };
    let mut budget = GlobalBudget::default();
    let mut state = SceneState::begin(begin).expect("fixture state");
    let empty = LayoutState::new(&state);
    let Admission::New(admitted) = state.admit(&step, &budget).expect("a legal fixture") else {
        panic!("a new step");
    };
    let committed = state.commit(admitted, &mut budget).expect("fixture commit");
    let surface = SurfaceGeometry::Overlay { default_canvas };
    let theme = Arc::new(Theme::default());
    let Ok(input) = PrepareInput::new(committed.preparation, empty, surface, theme) else {
        panic!("the first step follows the empty layout");
    };
    let rest = prepare(input, measure, &AtomicBool::new(false))
        .expect("fixture preparation")
        .after;
    let view = SceneView::new(&rest, state.activated());
    let mut frame = Frame::new();
    view.frame(0, &mut frame).expect("a fresh frame binds");
    let items = frame.items();
    let painted = frame
        .paint_items()
        .map(|(item, _, prims)| {
            let index = items
                .iter()
                .position(|each| std::ptr::eq(each, item))
                .expect("a paint unit of a frame item");
            let mut item = item.clone();
            if !mode.capped()
                && let Some((_, pivot)) = TURNED.iter().find(|(turned, _)| *turned == index)
            {
                item.transform = Affine::pose(1.0, 0.4, *pivot)
                    .compose(Affine::pose(1.0, 0.0, Vec2::new(-pivot.x, -pivot.y)))
                    .compose(item.transform);
            }
            (item, Arc::from(prims))
        })
        .collect();
    (painted, rest.paint_work())
}

#[cfg(windows)]
struct Row {
    seconds: f32,
    nanos: u128,
    allocated: usize,
    released: usize,
}

#[cfg(windows)]
struct Picture {
    items: Vec<PaintUnit>,
    /// Each unit's fills, staged once before it paints, as a committed view's are.
    scenes: Vec<PaintScene>,
    scratch: super::paint_stroke::Scratch,
    epoch: Instant,
    animated: bool,
    rows: Vec<Row>,
    failures: usize,
}

#[cfg(windows)]
struct Probe {
    picture: Rc<RefCell<Picture>>,
    frames: FrameLoop,
    rate: MaxFrameRate,
}

#[cfg(windows)]
impl Probe {
    fn frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let seconds = self.picture.borrow().epoch.elapsed().as_secs_f32();
        let refresh = window.refresh_interval(cx);
        if self
            .frames
            .frame(Instant::now(), self.rate, refresh)
            .is_some()
        {
            cx.notify();
        }
        if seconds < END {
            on_next_frame(window, cx.weak_entity(), Self::frame);
        } else {
            println!(
                "overlay_demand active=false seconds={seconds:.6} unix_ms={}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("wall clock")
                    .as_millis()
            );
            self.frames.idle();
            cx.notify();
        }
    }
}

#[cfg(windows)]
impl Render for Probe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let picture = Rc::clone(&self.picture);
        canvas(
            |_, _, _| (),
            move |_, (), window, cx| {
                let mut picture = picture.borrow_mut();
                let seconds = picture.epoch.elapsed().as_secs_f32();
                let moving = picture.animated && (START..END).contains(&seconds);
                let start = Instant::now();
                let (failures, _, [allocated, released]) = crate::allocation::usage(|| {
                    let mut failures = 0;
                    let Picture {
                        items,
                        scenes,
                        scratch,
                        ..
                    } = &mut *picture;
                    for ((original, prims), scene) in items.iter().zip(scenes) {
                        let mut item = original.clone();
                        if moving {
                            let offset = Vec2::new(seconds.sin() * 8.0, seconds.cos() * 8.0);
                            item.transform = Affine::pose(1.0, 0.0, offset).compose(item.transform);
                            item.opacity *= 0.65 + 0.35 * seconds.sin();
                            item.draw = 0.5 + 0.5 * seconds.sin();
                            item.flow = seconds.fract();
                        }
                        if overlay::paint_item(&item, 0, prims, scene, scratch, window, cx).is_err()
                        {
                            failures += 1;
                        }
                    }
                    failures
                });
                let nanos = start.elapsed().as_nanos();
                assert!(picture.rows.len() < 20_000, "fixed-duration repaint bound");
                picture.failures += failures;
                picture.rows.push(Row {
                    seconds,
                    nanos,
                    allocated,
                    released,
                });
            },
        )
        .size_full()
        .bg(cx.theme().background)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one bounded native probe and its content-free samples"
)]
#[cfg(windows)]
fn run(mode: Mode) {
    let animated = matches!(
        mode,
        Mode::Animated | Mode::Capped | Mode::FullRate | Mode::CappedFullRate
    );
    let lifetime = match mode {
        Mode::Dry => Duration::from_secs(5),
        Mode::Hide => Duration::from_secs(7),
        Mode::Rest | Mode::Animated | Mode::Capped | Mode::FullRate | Mode::CappedFullRate => {
            LIFETIME
        }
    };
    let rate = if matches!(mode, Mode::FullRate | Mode::CappedFullRate) {
        MaxFrameRate::cap(144).expect("144 is a frame rate cap")
    } else {
        MaxFrameRate::AUTOMATIC
    };
    let timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        std::process::exit(124);
    });
    drop(timeout);
    let picture = Rc::new(RefCell::new(Picture {
        items: Vec::new(),
        scenes: Vec::new(),
        scratch: super::paint_stroke::Scratch::new(),
        epoch: Instant::now(),
        animated,
        rows: Vec::with_capacity(20_000),
        failures: 0,
    }));
    let shared = Rc::clone(&picture);
    let cleared = Rc::clone(&picture);
    gpui_kit::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            UiTheme::change(ThemeMode::Dark, None, cx);
            cx.set_global(crate::Strings::english());
            let display = cx.primary_display().expect("primary display").bounds().size;
            let mut measure = crate::presenter::Measure::new(
                Arc::clone(cx.text_system()),
                cx.theme().font_family.clone(),
            );
            let logical = [f32::from(display.width), f32::from(display.height)];
            let (mut items, work) = resting(&mut measure, logical, mode);
            if matches!(mode, Mode::FullRate) {
                let one = std::mem::take(&mut items);
                for offset in [(0.0, 0.0), (1100.0, 0.0), (0.0, 520.0), (1100.0, 520.0)] {
                    items.extend(one.iter().map(|(item, prims)| {
                        let mut item = item.clone();
                        let offset = Vec2::new(offset.0, offset.1);
                        item.transform = Affine::pose(1.0, 0.0, offset).compose(item.transform);
                        (item, Arc::clone(prims))
                    }));
                }
            }
            println!(
                "overlay_fixture items={} dash={} fill={} outline={}",
                items.len(),
                work.dash_pieces,
                work.fill_vertices,
                work.outline_points
            );
            {
                let mut picture = shared.borrow_mut();
                picture.scenes = items
                    .iter()
                    .map(|(item, prims)| PaintScene::of(item.handle, prims))
                    .collect();
                picture.items = items;
                picture.epoch = Instant::now();
            }
            let (events, _receiver) = async_channel::unbounded();
            let mut input_windows = crate::platform::OverlayWindows::default();
            let input = crate::presenter::overlay_input::OverlayOwner::default();
            let (dock, _) = gpui_kit::open_window(options(), cx, |_, cx| {
                cx.new(|_| super::Dock::new())
            })
            .expect("fixture dock window");
            dock.update(cx, |_, window, cx| {
                super::overlay_input::configure_dock(
                    window,
                    events.clone(),
                    &input,
                    &mut input_windows,
                    cx,
                )?;
                crate::platform::place_presence_window(window, [0, 0, 16, 16])?;
                window.bounds_changed(cx);
                Ok::<_, crate::platform::PresenceError>(())
            })
            .expect("fixture dock window")
            .expect("configure the fixture dock through production setup");
            let (window, probe) = gpui_kit::open_window(options(), cx, |_, cx| {
                cx.new(|_| Probe {
                    picture: shared,
                    frames: FrameLoop::default(),
                    rate,
                })
            })
            .expect("fixture window");
            window
                .update(cx, |_, window, cx| {
                    println!(
                        "overlay_fixture display scale={} logical_width={} logical_height={} refresh_ns={:?}",
                        window.scale_factor(),
                        logical[0],
                        logical[1],
                        window.refresh_interval(cx).map(|interval| interval.as_nanos())
                    );
                    super::overlay_input::configure_drawing(
                        window,
                        events,
                        &input,
                        &mut input_windows,
                        cx,
                    )
                })
                .expect("fixture window")
                .expect("click-through fixture");
            super::overlay_input::show_drawing(window, &input, &mut input_windows, cx)
                .expect("show and restack the fixture without activation");
            window
                .update(cx, |_, drawing, cx| {
                    dock.update(cx, |_, dock, _| {
                        crate::platform::OverlayWindows::check_drawing(dock, drawing)
                    })
                    .expect("fixture dock stays open")
                })
                .expect("fixture drawing stays open")
                .expect("native drawing styles and order follow production setup");
            probe.update(cx, |_, cx| cx.notify());
            if matches!(mode, Mode::Hide) {
                let repaint = probe.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    println!("overlay_capture phase=shown");
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    cleared.borrow_mut().items.clear();
                    cx.update(|cx| repaint.update(cx, |_, cx| cx.notify()));
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    println!("overlay_capture phase=empty");
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    cx.update(|cx| overlay::hide(window, cx)).expect("the fixture hides");
                    println!("overlay_capture phase=hidden");
                })
                .detach();
            }
            if animated {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs_f32(START))
                        .await;
                    window
                        .update(cx, |_, window, cx| {
                            probe.update(cx, |probe, cx| {
                                println!("overlay_demand active=true seconds={:.6} unix_ms={}", probe.picture.borrow().epoch.elapsed().as_secs_f32(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("wall clock").as_millis());
                                probe.frames.want(Instant::now());
                                on_next_frame(window, cx.weak_entity(), Probe::frame);
                                cx.notify();
                            });
                        })
                        .expect("the fixture window stays open until its deadline");
                })
                .detach();
            }
            cx.spawn(async move |cx| {
                cx.background_executor().timer(lifetime).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    let picture = picture.borrow();
    assert!(!picture.rows.is_empty(), "the shown fixture painted");
    for row in &picture.rows {
        println!(
            "overlay_paint seconds={:.6} nanos={} allocated_B={} released_B={}",
            row.seconds, row.nanos, row.allocated, row.released
        );
    }
    let between = |from: f32, to: f32| {
        picture
            .rows
            .iter()
            .filter(|row| (from..to).contains(&row.seconds))
            .count()
    };
    println!(
        "overlay_fixture rest_paints_before_motion={} paints_in_motion={} paints_after_motion={}",
        between(1.0, START),
        between(START, END),
        between(END + 1.0, f32::MAX)
    );
    assert_eq!(picture.failures, 0, "every fixture primitive paints");
    if animated {
        assert!(between(START, END) > 0, "the fixture-local motion painted");
    }
    assert_eq!(between(END + 1.0, f32::MAX), 0, "no frames after motion");
    println!(
        "diagnostic-only animated painter fixture={animated} fixed_seconds={} max_frame_rate={rate:?} backdrop=opaque_dark_theme_background; no conductor, activation, StepShown, connection, RSS or presentation claim",
        lifetime.as_secs()
    );
}

#[cfg(windows)]
#[test]
#[ignore = "manual input-free five-second native item-painter dry run; numbers do not count"]
fn smoke_primary_overlay() {
    run(Mode::Dry);
}

#[cfg(windows)]
#[test]
#[ignore = "manual input-free seven-second capture: star, ring, empty repaint, then hide"]
fn capture_fill_rules_then_hide() {
    run(Mode::Hide);
}

#[cfg(windows)]
#[test]
#[ignore = "manual quiet-Rez fixed 60 s production item painter at all paint caps, no input"]
fn measure_primary_overlay_at_paint_caps() {
    run(Mode::Capped);
}

#[cfg(windows)]
#[test]
#[ignore = "manual quiet-Rez fixed 60 s resting overlay through production paint_item, no input"]
fn measure_primary_overlay_at_rest() {
    run(Mode::Rest);
}

#[cfg(windows)]
#[test]
#[ignore = "manual quiet-Rez fixed 60 s diagnostic-only animated painter fixture, no input"]
fn measure_primary_overlay_animated() {
    run(Mode::Animated);
}

#[cfg(windows)]
#[test]
#[ignore = "manual quiet-Rez fixed 60 s animated painter fixture, four copies at a 144 cap, no input"]
fn measure_primary_overlay_animated_four_at_full_rate() {
    run(Mode::FullRate);
}

#[cfg(windows)]
#[test]
#[ignore = "manual quiet-Rez fixed 60 s production item painter at all paint caps, capped at 144, no input"]
fn measure_primary_overlay_at_paint_caps_at_full_rate() {
    run(Mode::CappedFullRate);
}
