use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{AppContext as _, Context, IntoElement, Render, Styled as _, Window, canvas};
use kumochi_scene::{
    display::{Paint, PaintWork, Prim},
    geom::{Affine, Rect, Vec2},
    record::Rgb,
};

use super::{configure_surface, line::on_next_frame, options, paint_paths, scaled};
use crate::platform::{self, PresenceSurface};

const BLOCK: u64 = 4;
const REPEATS: usize = 2;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Empty,
    Overlap,
    Dash,
    Fill,
    Crossing,
    Outline,
    Mixed,
}

struct Case {
    kind: Kind,
    candidate: usize,
    primitives: Vec<(Prim, Affine)>,
    work: PaintWork,
}

fn color() -> Paint {
    Paint {
        rgb: Rgb::try_from(0x0052_a2df).expect("RGB"),
        opacity: 0.5,
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "manual probe counts and coordinates are bounded by 32768"
)]
fn case(kind: Kind, candidate: usize) -> Case {
    let mut primitives = Vec::new();
    if matches!(kind, Kind::Overlap) {
        primitives.push((
            Prim::Line {
                points: Arc::from([
                    Vec2::new(-800.0, -500.0),
                    Vec2::new(800.0, 500.0),
                    Vec2::new(-800.0, 500.0),
                    Vec2::new(800.0, -500.0),
                ]),
                closed: false,
                color: color(),
                width: 50.0,
                dashed: false,
            },
            Affine::pose(1.0, 0.0, Vec2::new(1280.0, 720.0)),
        ));
    }
    if matches!(kind, Kind::Dash | Kind::Mixed) {
        let mut left = candidate;
        while left >= 2 {
            let count = left.min(2002);
            let y = primitives.len() as f32 * 50.0;
            primitives.push((
                Prim::Line {
                    points: Arc::from([Vec2::new(0.0, y), Vec2::new((count - 2) as f32 * 10.0, y)]),
                    closed: false,
                    color: color(),
                    width: 40.0,
                    dashed: true,
                },
                Affine::pose(0.12, 0.0, Vec2::new(40.0, 40.0)),
            ));
            left -= count;
        }
    }
    if matches!(kind, Kind::Fill | Kind::Crossing | Kind::Mixed) {
        let polygon = |count: usize| {
            let mut stride = (count - 1) / 2;
            loop {
                let (mut a, mut b) = (count, stride);
                while b > 0 {
                    (a, b) = (b, a % b);
                }
                if a == 1 {
                    break;
                }
                stride -= 1;
            }
            Prim::Fill {
                points: (0..count)
                    .map(|index| {
                        let angle =
                            index as f32 * std::f32::consts::TAU * stride as f32 / count as f32;
                        Vec2::new(angle.cos() * 50.0, angle.sin() * 50.0)
                    })
                    .collect::<Arc<[Vec2]>>(),
                color: color(),
            }
        };
        let pose = Affine::pose(20.0, 0.0, Vec2::new(1280.0, 720.0));
        let mut left = candidate;
        if !matches!(kind, Kind::Fill) {
            let mut largest = polygon(3);
            let mut charged = PaintWork::of(std::slice::from_ref(&largest)).fill_vertices;
            for count in 4..=208 {
                let prim = polygon(count);
                let charge = PaintWork::of(std::slice::from_ref(&prim)).fill_vertices;
                if charge > candidate {
                    break;
                }
                largest = prim;
                charged = charge;
            }
            primitives.push((largest, pose));
            left -= charged;
        }
        let prim = polygon(16);
        let charge = PaintWork::of(std::slice::from_ref(&prim)).fill_vertices;
        assert!(charge > 0);
        for _ in 0..left / charge {
            primitives.push((prim.clone(), pose));
        }
    }
    if matches!(kind, Kind::Outline | Kind::Mixed) {
        let prim = Prim::Rect {
            rect: Rect {
                origin: Vec2::new(0.0, 0.0),
                size: Vec2::new(40.0, 40.0),
            },
            radius: 8.0,
            fill: None,
            border: Some((color(), 0.1)),
        };
        let charge = PaintWork::of(std::slice::from_ref(&prim)).outline_points;
        assert!(charge > 0);
        for _ in 0..candidate / charge {
            primitives.push((prim.clone(), Affine::pose(64.0, 0.01, Vec2::new(0.0, 0.0))));
        }
    }
    let work = primitives
        .iter()
        .fold(PaintWork::default(), |sum, (prim, _)| {
            sum + PaintWork::of(std::slice::from_ref(prim))
        });
    assert!(
        work.dash_pieces <= candidate
            && work.fill_vertices <= candidate
            && work.outline_points <= candidate
    );
    Case {
        kind,
        candidate,
        primitives,
        work,
    }
}

struct Row {
    case: usize,
    seconds: f32,
    nanos: u128,
    allocated: usize,
    released: usize,
}

struct Probe {
    cases: Rc<[Case]>,
    epoch: Instant,
    lifetime: Duration,
    rows: Rc<RefCell<Vec<Row>>>,
}

impl Probe {
    fn frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.epoch.elapsed() < self.lifetime {
            cx.notify();
            on_next_frame(window, cx.weak_entity(), Self::frame);
        }
    }
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let cases = Rc::clone(&self.cases);
        let rows = Rc::clone(&self.rows);
        let epoch = self.epoch;
        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let elapsed = epoch.elapsed();
                let block =
                    usize::try_from(elapsed.as_secs() / BLOCK).expect("bounded probe clock");
                let pass = block / cases.len();
                let index = if pass.is_multiple_of(2) {
                    block % cases.len()
                } else {
                    cases.len() - 1 - block % cases.len()
                };
                let case = &cases[index];
                let start = Instant::now();
                let ((), _, [allocated, released]) = crate::allocation::usage(|| {
                    for (prim, transform) in &case.primitives {
                        let paths = paint_paths::paths(
                            prim,
                            *transform,
                            0.99,
                            elapsed.as_secs_f32().fract(),
                        )
                        .expect("bounded diagnostic geometry builds");
                        for (path, paint) in paths {
                            let mut color = gpui_kit::rgb(paint.rgb.get());
                            color.a *= paint.opacity;
                            window.paint_path(path, color);
                        }
                    }
                });
                let nanos = start.elapsed().as_nanos();
                let mut rows = rows.borrow_mut();
                assert!(rows.len() < 80_000, "fixed-duration repaint bound");
                rows.push(Row {
                    case: index,
                    seconds: elapsed.as_secs_f32(),
                    nanos,
                    allocated,
                    released,
                });
            },
        )
        .size_full()
    }
}

#[test]
fn probe_candidates_are_bounded_and_exercise_all_three_charges() {
    for kind in [
        Kind::Dash,
        Kind::Fill,
        Kind::Crossing,
        Kind::Outline,
        Kind::Mixed,
    ] {
        let case = case(kind, 512);
        let work = case.work;
        match kind {
            Kind::Dash => assert_eq!(
                (work.dash_pieces, work.fill_vertices, work.outline_points),
                (512, 0, 0)
            ),
            Kind::Fill => assert_eq!(
                (work.dash_pieces, work.fill_vertices, work.outline_points),
                (0, 448, 0)
            ),
            Kind::Crossing => assert_eq!(
                (work.dash_pieces, work.fill_vertices, work.outline_points),
                (0, 480, 0)
            ),
            Kind::Outline => assert_eq!(
                (work.dash_pieces, work.fill_vertices, work.outline_points),
                (0, 0, 436)
            ),
            Kind::Mixed => assert_eq!(
                (work.dash_pieces, work.fill_vertices, work.outline_points),
                (512, 480, 436)
            ),
            Kind::Empty | Kind::Overlap => unreachable!(),
        }
    }
    let crossing = case(Kind::Overlap, 0);
    assert_eq!(crossing.primitives.len(), 1);
    let Prim::Line {
        points,
        color,
        dashed,
        ..
    } = &crossing.primitives[0].0
    else {
        panic!("crossing stroke");
    };
    assert_eq!(points.len(), 4);
    assert_eq!((points[0] + points[1]) * 0.5, Vec2::new(0.0, 0.0));
    assert_eq!((points[2] + points[3]) * 0.5, Vec2::new(0.0, 0.0));
    assert_eq!(color.opacity.to_bits(), 0.5_f32.to_bits());
    assert!(!dashed);
}

#[expect(
    clippy::too_many_lines,
    reason = "one bounded native probe setup and content-free output"
)]
fn run(cases: Vec<Case>, repeats: usize) {
    let timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(480));
        std::process::exit(124);
    });
    drop(timeout);
    let cases: Rc<[Case]> = cases.into();
    let lifetime =
        Duration::from_secs(u64::try_from(cases.len() * repeats).expect("bounded cases") * BLOCK);
    let saved = Rc::new(RefCell::new(Vec::with_capacity(80_000)));
    let rows = Rc::clone(&saved);
    let output = Rc::clone(&cases);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(crate::Strings::english());
            let display = cx.primary_display().expect("primary display").bounds().size;
            let (events, _receiver) = async_channel::unbounded();
            let epoch = Instant::now();
            println!(
                "paint_work_epoch unix_ms={}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_millis()
            );
            let (handle, fixture) = gpui_kit::open_window(options(), cx, |_, cx| {
                cx.new(|_| Probe {
                    cases,
                    epoch,
                    lifetime,
                    rows,
                })
            })
            .expect("diagnostic window");
            handle
                .update(cx, |_, window, cx| {
                    configure_surface(window, PresenceSurface::Overlay, events, cx)
                        .expect("click-through fixture");
                    let scale = window.scale_factor();
                    println!(
                        "paint_work_display scale={scale} logical_width={} logical_height={}",
                        f32::from(display.width),
                        f32::from(display.height)
                    );
                    println!(
                        "paint_work_display refresh_ns={:?}",
                        window
                            .refresh_interval(cx)
                            .map(|interval| interval.as_nanos())
                    );
                    platform::place_presence_window(
                        window,
                        [
                            0,
                            0,
                            scaled(display.width.into(), scale),
                            scaled(display.height.into(), scale),
                        ],
                    )
                    .expect("show primary fixture without activation");
                    window.bounds_changed(cx);
                    fixture.update(cx, |_, cx| {
                        on_next_frame(window, cx.weak_entity(), Probe::frame);
                        cx.notify();
                    });
                })
                .expect("place fixture");
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(lifetime + Duration::from_secs(1))
                    .await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    let rows = saved.borrow();
    for (index, case) in output.iter().enumerate() {
        println!(
            "paint_work_case index={index} kind={:?} candidate={} dash={} fill={} outline={} primitives={}",
            case.kind,
            case.candidate,
            case.work.dash_pieces,
            case.work.fill_vertices,
            case.work.outline_points,
            case.primitives.len()
        );
        assert!(
            rows.iter().any(|row| row.case == index),
            "every bounded candidate painted"
        );
    }
    for row in rows.iter() {
        println!(
            "paint_work_sample case={} seconds={:.6} nanos={} allocated_B={} released_B={}",
            row.case, row.seconds, row.nanos, row.allocated, row.released
        );
    }
    println!(
        "diagnostic-only paint callback timing and allocation; no activation, StepShown, RSS or presentation-latency claim"
    );
}

#[test]
#[ignore = "manual input-free five-second harness dry run; its numbers do not count"]
fn smoke_primary_paint_work() {
    run(vec![case(Kind::Mixed, 512)], 1);
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "bounded diagnostic coordinates"
)]
fn small_primitives(candidate: usize) -> Case {
    let mut mixed = case(Kind::Dash, candidate);
    mixed.kind = Kind::Mixed;
    for index in 0..candidate / 3 {
        mixed.primitives.push((
            Prim::Fill {
                points: Arc::from([
                    Vec2::new(0.0, 0.0),
                    Vec2::new(8.0, 0.0),
                    Vec2::new(0.0, 8.0),
                ]),
                color: color(),
            },
            Affine::pose(
                1.0,
                0.0,
                Vec2::new((index % 100) as f32 * 20.0, (index / 100) as f32 * 20.0),
            ),
        ));
    }
    for index in 0..candidate / 5 {
        mixed.primitives.push((
            Prim::Rect {
                rect: Rect {
                    origin: Vec2::new(0.0, 0.0),
                    size: Vec2::new(8.0, 8.0),
                },
                radius: 0.0,
                fill: None,
                border: Some((color(), 1.0)),
            },
            Affine::pose(
                1.0,
                0.01,
                Vec2::new(
                    (index % 100) as f32 * 20.0,
                    300.0 + (index / 100) as f32 * 20.0,
                ),
            ),
        ));
    }
    mixed.work = mixed
        .primitives
        .iter()
        .fold(PaintWork::default(), |sum, (prim, _)| {
            sum + PaintWork::of(std::slice::from_ref(prim))
        });
    mixed
}

#[test]
fn the_small_primitive_control_spends_the_three_budgets_independently() {
    let mixed = small_primitives(512);
    assert_eq!(mixed.primitives.len(), 1 + 170 + 102);
    assert_eq!(
        (
            mixed.work.dash_pieces,
            mixed.work.fill_vertices,
            mixed.work.outline_points
        ),
        (512, 510, 510)
    );
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "manual bounded metadata-cost calibration"
)]
fn measure_preparation_work() {
    use kumochi_scene::limits::{MAX_GLOBAL_PATH_SEGMENTS, MAX_PATH_SEGMENTS};
    let convex = |count: usize| Prim::Fill {
        points: (0..count)
            .map(|index| {
                let angle = index as f64 * std::f64::consts::TAU / count as f64;
                Vec2::new(
                    (angle.cos() * 20_000.0) as f32,
                    (angle.sin() * 20_000.0) as f32,
                )
            })
            .collect(),
        color: color(),
    };
    for total in [512, 1024, MAX_PATH_SEGMENTS, MAX_GLOBAL_PATH_SEGMENTS] {
        let mut left = total;
        let mut primitives = Vec::new();
        while left > 0 {
            let count = left.min(MAX_PATH_SEGMENTS);
            primitives.push(convex(count));
            left -= count;
        }
        for trial in 0..3 {
            let start = Instant::now();
            let work = std::hint::black_box(PaintWork::of(std::hint::black_box(&primitives)));
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(work.fill_vertices, total);
            println!(
                "paint_work_metadata points={total} items={} trial={trial} nanos={elapsed}; metadata-only, not full preparation",
                primitives.len()
            );
        }
    }
    for total in [MAX_PATH_SEGMENTS, MAX_GLOBAL_PATH_SEGMENTS] {
        let primitives: Vec<_> = (0..total.div_ceil(MAX_PATH_SEGMENTS))
            .map(|item| {
                let count = (total - item * MAX_PATH_SEGMENTS).min(MAX_PATH_SEGMENTS);
                Prim::Fill {
                    points: (0..count)
                        .map(|index| {
                            let x = if index % 2 == 0 { -10_000.0 } else { 10_000.0 };
                            Vec2::new(x, x + index as f32 * 0.01)
                        })
                        .collect(),
                    color: color(),
                }
            })
            .collect();
        for trial in 0..3 {
            let start = Instant::now();
            let work = std::hint::black_box(PaintWork::of(std::hint::black_box(&primitives)));
            println!(
                "paint_work_parallel_metadata points={total} items={} trial={trial} nanos={} fill={}; metadata-only, not full preparation",
                primitives.len(),
                start.elapsed().as_nanos(),
                work.fill_vertices
            );
        }
    }
}

#[test]
#[ignore = "manual quiet-Rez metadata and small-primitive budget control"]
fn measure_primary_paint_work_followup() {
    measure_preparation_work();
    let mut cases = Vec::new();
    for count in [256, 1024, 512] {
        cases.push(case(Kind::Mixed, count));
        cases.push(small_primitives(count));
    }
    run(cases, REPEATS);
}

#[test]
#[ignore = "manual quiet-Rez diagnostic-only fixed-duration native painter budget calibration"]
fn measure_primary_paint_work_candidates() {
    let mut cases = Vec::new();
    for count in [512, 4096, 1024, 8192, 2048, 16384, 32768] {
        cases.push(case(Kind::Empty, 0));
        cases.push(case(Kind::Overlap, 0));
        for kind in [
            Kind::Dash,
            Kind::Fill,
            Kind::Crossing,
            Kind::Outline,
            Kind::Mixed,
        ] {
            cases.push(case(kind, count));
        }
    }
    for (dash, fill, outline) in [(512, 8192, 2048), (1024, 16384, 4096), (2048, 32768, 8192)] {
        let mut mixed = case(Kind::Dash, dash);
        mixed.kind = Kind::Mixed;
        for other in [case(Kind::Crossing, fill), case(Kind::Outline, outline)] {
            mixed.work = mixed.work + other.work;
            mixed.primitives.extend(other.primitives);
        }
        cases.push(mixed);
    }
    measure_preparation_work();
    run(cases, REPEATS);
}
