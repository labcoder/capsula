//! The GPU side on Direct3D 12's software adapter: charges worked by hand from each fixture's
//! mesh, range tables that outlive the mesh's next build, and pixels read back from the
//! resolved image, each expectation worked by hand from the fixture's geometry and the 4x
//! standard sample pattern, whose samples sit at 1/8, 3/8, 5/8 and 7/8 of a pixel across.

use std::{
    sync::{Mutex, MutexGuard, OnceLock, PoisonError},
    thread,
    time::Duration,
};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    display::{Paint, Prim},
    geom::{Rect, Vec2},
    record::Rgb,
};

use super::{BuildInput, FrameBuf, Mesh, Record, build, cache, filler, label};
use crate::{
    Built, Color, Drawn, GeometryId, Gpu, GpuFailure, View,
    limits::{RECORDS, UPLOADED},
};

/// Room for every test's resources on the shared device.
const BUDGET: u64 = 64 * 1024 * 1024;
/// The record buffer every device charges first: 3,000 records of 64 bytes.
const RECORDS_BYTES: u64 = 192_000;
/// An 8 by 8 target: four samples and the resolved pixel, four bytes each.
const TARGET_8_BYTES: u64 = 8 * 8 * 4 * 5;
/// A 1 by 1 target.
const PIXEL_BYTES: u64 = 4 * 5;
/// A filled square's upload: lyon fills it with 4 vertices of 28 bytes and 2 triangles of
/// 4-byte indices, and it paints 1 piece of 32 bytes.
const SQUARE_BYTES: u64 = 4 * 28 + 6 * 4 + 32;
const ORIGIN: View = View {
    origin: [0.0, 0.0],
    scale: 1.0,
};
const CLEAR: [u8; 4] = [0, 0, 0, 0];

fn software(budget: u64) -> Gpu {
    let gpu = Gpu::software(budget)
        .expect("GPU tests need Direct3D 12's software adapter, which this machine lacks");
    assert!(gpu.on_cpu(), "the software adapter is a CPU adapter");
    gpu
}

/// The one device every pixel case draws on, and the lock every device test holds: the software
/// adapter faults when two devices in one process work on different threads at once. A test
/// with its own device takes it before opening one, so it is released after that device drops.
fn shared() -> MutexGuard<'static, Gpu> {
    static DEVICE: OnceLock<Mutex<Gpu>> = OnceLock::new();
    DEVICE
        .get_or_init(|| Mutex::new(software(BUDGET)))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn paint(rgb: u32, opacity: f32) -> Paint {
    Paint {
        rgb: Rgb::try_from(rgb).expect("fixture colour"),
        opacity,
    }
}

fn square(at: [f32; 2], side: f32, fill: Paint, border: Option<(Paint, f32)>) -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(at[0], at[1]),
            size: Vec2::new(side, side),
        },
        radius: 0.0,
        fill: Some(fill),
        border,
    }
}

fn stroke(points: Vec<Vec2>, color: Paint) -> Prim {
    Prim::Line {
        points: points.into(),
        closed: false,
        color,
        width: 2.0,
        dashed: false,
    }
}

/// Builds geometry `id` of `objects` objects holding `prims` through `mesh`.
fn built(mesh: &mut Mesh, id: u64, objects: usize, prims: &[Prim]) {
    build(&mut BuildInput::new(), mesh, &cache(id, objects, prims)).expect("it builds");
}

/// Geometry `id`, built through `mesh` and uploaded.
fn upload(gpu: &mut Gpu, mesh: &mut Mesh, id: u64, objects: usize, prims: &[Prim]) -> Built {
    built(mesh, id, objects, prims);
    gpu.upload(mesh).expect("it uploads")
}

/// A 2 by 2 opaque red square at `at`.
fn red_square(at: [f32; 2]) -> Prim {
    square(at, 2.0, paint(0xFF_00_00, 1.0), None)
}

fn posed(object: u32, first: u32, count: u32, at: [f32; 2]) -> Record {
    Record {
        transform: [1.0, 0.0, 0.0, 1.0, at[0], at[1]],
        ..filler(object, first, count)
    }
}

fn frame(pictures: &[(GeometryId, &[Record])]) -> FrameBuf {
    let mut buf = FrameBuf::new();
    for (scene, (geometry, records)) in (1..).zip(pictures) {
        buf.picture(SceneId(scene), *geometry, |list| {
            records.iter().try_for_each(|record| list.push(*record))
        })
        .expect("the frame fits");
    }
    buf
}

#[test]
fn an_upload_charges_its_buffers_to_the_byte_and_nothing_when_refused() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(RECORDS_BYTES + SQUARE_BYTES - 1);
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Unfinished),
        "nothing built"
    );
    built(&mut mesh, 1, 1, &[red_square([0.0, 0.0])]);
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Budget),
        "one byte short"
    );
    assert_eq!(gpu.charged(), RECORDS_BYTES, "a refusal charges nothing");
    assert_eq!(gpu.free_tables(), UPLOADED, "and holds no slot");
    let exact = RECORDS_BYTES + PIXEL_BYTES + SQUARE_BYTES;
    let mut gpu = software(exact);
    let target = gpu.target(1, 1).expect("a target");
    let one = gpu.upload(&mesh).expect("it fits to the byte");
    assert_eq!(gpu.charged(), exact);
    mesh.begin(GeometryId(2));
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Unfinished),
        "a build is open"
    );
    built(&mut mesh, 2, 1, &[red_square([0.0, 0.0])]);
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Budget),
        "no room while the first is held"
    );
    gpu.release_built(one).expect("its own geometry");
    assert_eq!(gpu.free_tables(), UPLOADED, "the slot comes back at once");
    assert_eq!(
        gpu.charged(),
        exact,
        "the GPU side holds the buffers until a submission carries their writes"
    );
    gpu.draw(&target, &FrameBuf::new(), None, &[], ORIGIN)
        .expect("it clears");
    // Each refused upload polls once without waiting, which runs finished work's callbacks.
    let two = (0..300)
        .find_map(|_| {
            let two = gpu.upload(&mesh).ok();
            if two.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            two
        })
        .expect("the charge comes back within three seconds of the submission that carried it");
    assert_eq!(gpu.charged(), exact, "only the new geometry is charged");
    gpu.release_built(two).expect("its own geometry");
    gpu.release_target(target).expect("its own target");
}

#[test]
fn an_empty_geometry_uploads_one_padding_item_of_each() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let empty = upload(&mut gpu, &mut mesh, 11, 0, &[]);
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES + 28 + 4 + 32,
        "one vertex, one index and one piece of padding"
    );
    gpu.release_built(empty).expect("its own geometry");
}

#[test]
fn a_write_refused_after_a_queued_one_holds_the_whole_charge() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    built(&mut mesh, 10, 1, &[red_square([0.0, 0.0])]);
    gpu.refuse_second();
    assert_eq!(gpu.upload(&mesh).err(), Some(GpuFailure::Refused));
    assert_eq!(gpu.free_tables(), UPLOADED, "the slot comes back at once");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES + SQUARE_BYTES,
        "the vertices' write is queued, so the whole charge waits"
    );
    assert!(!gpu.flagged(), "a refusal a scope caught is no loss");
}

#[test]
fn uploads_fit_their_slots_and_one_more_waits_for_a_release() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let mut held: Vec<Built> = (0..UPLOADED)
        .map(|_| upload(&mut gpu, &mut mesh, 3, 1, &[red_square([0.0, 0.0])]))
        .collect();
    let charged = gpu.charged();
    assert_eq!(
        charged,
        RECORDS_BYTES + 5 * SQUARE_BYTES,
        "five slots, each a square"
    );
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Budget),
        "every slot is held"
    );
    assert_eq!(gpu.charged(), charged, "the refusal charges nothing");
    let mut other = software(BUDGET);
    let elsewhere = other.upload(&mesh).expect("another device's slot");
    let elsewhere = gpu
        .release_built(elsewhere)
        .expect_err("another device's geometry is handed back");
    assert_eq!(gpu.free_tables(), 0, "a foreign release frees nothing here");
    assert_eq!(gpu.charged(), charged);
    other
        .release_built(elsewhere)
        .expect("its own device takes it back");
    assert_eq!(other.free_tables(), UPLOADED, "its slot is back");
    assert_eq!(
        other.charged(),
        RECORDS_BYTES + SQUARE_BYTES,
        "its charge waits there, not lost"
    );
    let first = other.target(1, 1).expect("a target");
    other
        .draw(&first, &FrameBuf::new(), None, &[], ORIGIN)
        .expect("it clears");
    other.settle();
    let second = other.target(1, 1).expect("a target");
    assert_eq!(
        other.charged(),
        RECORDS_BYTES + 2 * PIXEL_BYTES,
        "and comes back there once the submission that carried it retires"
    );
    other.release_target(first).expect("its own target");
    other.release_target(second).expect("its own target");
    let first = held.pop().expect("an upload");
    gpu.release_built(first).expect("its own geometry");
    held.push(gpu.upload(&mesh).expect("the released slot takes the next"));
    for built in held {
        gpu.release_built(built).expect("its own geometry");
    }
}

#[test]
fn an_upload_keeps_its_range_tables_through_the_mesh_s_next_build() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let red = red_square([0.0, 0.0]);
    let first = upload(&mut gpu, &mut mesh, 4, 2, std::slice::from_ref(&red));
    let record = filler(1, 0, 1);
    assert_eq!(
        first.range(&record),
        Some(6..12),
        "object 1's square: the second six"
    );
    built(&mut mesh, 5, 2, &[red.clone(), red]);
    assert_eq!(
        mesh.range(&record),
        Some(12..18),
        "the mesh now holds two squares an object"
    );
    assert_eq!(
        first.range(&record),
        Some(6..12),
        "the upload still resolves its own"
    );
    assert_eq!(first.geometry(), GeometryId(4));
    gpu.release_built(first).expect("its own geometry");
}

#[test]
fn a_charge_waits_for_the_last_draw_that_reads_it() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let built = upload(&mut gpu, &mut mesh, 23, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(8, 8).expect("a target");
    let buf = frame(&[(built.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    gpu.settle();
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws again");
    gpu.release_built(built).expect("its own geometry");
    gpu.release_target(target).expect("its own target");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES + TARGET_8_BYTES + SQUARE_BYTES,
        "the first draw retired, the second may still read both"
    );
    gpu.settle();
    let target = gpu.target(1, 1).expect("a target");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES + PIXEL_BYTES,
        "both come back once the second retires"
    );
    gpu.release_target(target).expect("its own target");
}

#[test]
fn a_lost_device_refuses_every_later_call() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(RECORDS_BYTES + TARGET_8_BYTES + SQUARE_BYTES);
    let one = upload(&mut gpu, &mut mesh, 6, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(8, 8).expect("a target");
    let buf = frame(&[(one.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    gpu.draw(&target, &buf, None, &[&one], ORIGIN)
        .expect("it draws");
    gpu.destroy();
    let lost = (0..300).any(|_| {
        let lost = gpu.step();
        if !lost {
            thread::sleep(Duration::from_millis(10));
        }
        lost
    });
    assert!(lost, "the loss arrives within three seconds");
    for _ in 0..2 {
        assert_eq!(
            gpu.upload(&mesh).err(),
            Some(GpuFailure::Lost),
            "a lost device says so before its full budget is asked"
        );
        assert_eq!(
            gpu.target(8, 8).err(),
            Some(GpuFailure::Lost),
            "nor a target"
        );
        assert_eq!(
            gpu.draw(&target, &buf, None, &[&one], ORIGIN),
            Err(GpuFailure::Lost)
        );
    }
    gpu.release_target(target).expect("a release still works");
    gpu.release_built(one).expect("a release still works");
    assert_eq!(gpu.free_tables(), UPLOADED);
}

#[test]
fn an_upload_after_a_loss_not_yet_reported_answers_lost() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    built(&mut mesh, 9, 1, &[red_square([0.0, 0.0])]);
    gpu.destroy();
    assert!(!gpu.flagged(), "the loss is reported only when polled");
    assert_eq!(gpu.upload(&mesh).err(), Some(GpuFailure::Lost));
    assert!(gpu.flagged(), "the unexplained refusal latches it");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES,
        "nothing was queued: refunded at once"
    );
    assert_eq!(gpu.free_tables(), UPLOADED);
}

#[test]
fn a_draw_after_a_loss_not_yet_reported_answers_lost() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let built = upload(&mut gpu, &mut mesh, 24, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(4, 4).expect("a target");
    let buf = frame(&[(built.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    gpu.destroy();
    assert!(!gpu.flagged(), "the loss is reported only when polled");
    assert_eq!(
        gpu.draw(&target, &buf, None, &[&built], ORIGIN),
        Err(GpuFailure::Lost),
        "the records' write is refused with both scopes clean"
    );
    assert!(gpu.flagged(), "the unexplained refusal latches it");
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_released_upload_comes_back_without_a_draw() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(RECORDS_BYTES + SQUARE_BYTES);
    let one = upload(&mut gpu, &mut mesh, 25, 1, &[red_square([0.0, 0.0])]);
    gpu.release_built(one).expect("its own geometry");
    // Each short reservation polls once without waiting; the first also submits the writes.
    let target = (0..300)
        .find_map(|_| {
            let target = gpu.target(1, 1).ok();
            if target.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
            target
        })
        .expect("the charge comes back within three seconds with no draw to submit its writes");
    assert_eq!(gpu.charged(), RECORDS_BYTES + PIXEL_BYTES);
    gpu.release_target(target).expect("its own target");
}

#[test]
fn a_reservation_that_finds_a_loss_answers_lost_and_credits_nothing() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let exact = RECORDS_BYTES + PIXEL_BYTES + SQUARE_BYTES;
    let mut gpu = software(exact);
    let target = gpu.target(1, 1).expect("a target");
    let one = upload(&mut gpu, &mut mesh, 26, 1, &[red_square([0.0, 0.0])]);
    gpu.destroy();
    gpu.draw(&target, &FrameBuf::new(), None, &[], ORIGIN)
        .expect("a clear stages no write that could notice the loss");
    gpu.release_built(one).expect("its own geometry");
    assert_eq!(
        gpu.target(1, 1).err(),
        Some(GpuFailure::Lost),
        "the reservation's poll finds the loss"
    );
    assert_eq!(
        gpu.charged(),
        exact,
        "and the refused reservation credits nothing"
    );
    gpu.release_target(target).expect("its own target");
}

#[test]
fn a_refused_submission_retires_nothing() {
    let _one_at_a_time = shared();
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let first = upload(&mut gpu, &mut mesh, 27, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(8, 8).expect("a target");
    let buf = frame(&[(first.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    gpu.draw(&target, &buf, None, &[&first], ORIGIN)
        .expect("it draws");
    let second = upload(&mut gpu, &mut mesh, 28, 1, &[red_square([0.0, 0.0])]);
    gpu.release_built(second).expect("its own geometry");
    let doomed = gpu.target(8, 8).expect("a target");
    doomed.destroy_image();
    assert_eq!(
        gpu.draw(&doomed, &FrameBuf::new(), None, &[], ORIGIN),
        Err(GpuFailure::Refused),
        "a destroyed image is refused at submission"
    );
    assert!(!gpu.flagged(), "a refusal a scope caught is no loss");
    gpu.settle();
    gpu.release_built(first).expect("its own geometry");
    gpu.release_target(target).expect("its own target");
    gpu.release_target(doomed).expect("its own target");
    let probe = gpu.target(1, 1).expect("a target");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES + SQUARE_BYTES + TARGET_8_BYTES + PIXEL_BYTES,
        "the first draw retired; the second upload's writes and the refused image wait"
    );
    gpu.release_target(probe).expect("its own target");
}

/// Pixel `[x, y]` of an image `width` wide, read as BGRA and returned as RGBA.
fn pixel(pixels: &[[u8; 4]], width: u32, [x, y]: [u32; 2]) -> [u8; 4] {
    let [blue, green, red, alpha] = pixels[usize::try_from(y * width + x).expect("a pixel index")];
    [red, green, blue, alpha]
}

#[test]
fn a_reveal_or_dash_end_inside_a_pixel_covers_part_of_it() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let ink = paint(0xFF_80_40, 1.0);
    let along = vec![Vec2::new(0.0, 2.0), Vec2::new(16.0, 2.0)];
    let built = upload(
        &mut gpu,
        &mut mesh,
        11,
        1,
        &[
            stroke(along.clone(), ink),
            Prim::Line {
                points: along.into(),
                closed: false,
                color: ink,
                width: 2.0,
                dashed: true,
            },
        ],
    );
    let target = gpu.target(16, 8).expect("a target");
    // The reveal ends at x = 8.4; the dashes run from 0.4 to 6.4 with phase 9.6, flow 0.04.
    let revealed = Record {
        draw: 8.4 / 16.0,
        ..posed(0, 0, 1, [0.0, 0.0])
    };
    let dashed = Record {
        flow: 0.04,
        ..posed(0, 1, 1, [0.0, 4.0])
    };
    let buf = frame(&[(built.geometry(), &[revealed, dashed])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    let solid = [0xFF, 0x80, 0x40, 0xFF];
    for (row, [inked, cut, past]) in [(1, [7, 8, 9]), (5, [5, 6, 7])] {
        assert_eq!(
            pixel(&pixels, 16, [inked, row]),
            solid,
            "row {row} solid before"
        );
        let partial = pixel(&pixels, 16, [cut, row]);
        // Two samples of four, premultiplied: 127.5, 64, 32 and alpha 127.5, which the resolve
        // rounds either way.
        assert!(
            matches!(partial, [127 | 128, 64, 32, 127 | 128]),
            "row {row}: two samples of four left of the end: {partial:?}"
        );
        assert_eq!(
            pixel(&pixels, 16, [past, row]),
            CLEAR,
            "row {row} clear after"
        );
    }
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn skip_is_read_flat_from_each_triangle_s_first_vertex() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    // 500 wiggles of 0.04 merge into the middle point: 20 of the line's 40 source units, so
    // the first edge's triangles start at skip 0 and end at the middle's skip of 20.
    let mut zigzag = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)];
    zigzag.extend((1..=500).map(|at| Vec2::new(if at % 2 == 1 { 10.04 } else { 10.0 }, 0.0)));
    zigzag.push(Vec2::new(20.0, 0.0));
    let ink = paint(0xFF_80_40, 1.0);
    let built = upload(&mut gpu, &mut mesh, 12, 1, &[stroke(zigzag, ink)]);
    let target = gpu.target(32, 4).expect("a target");
    // Revealed to 8 of 40: on the first edge the source distance is x itself.
    let record = Record {
        draw: 0.2,
        ..posed(0, 0, 1, [4.0, 2.0])
    };
    let buf = frame(&[(built.geometry(), &[record])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 32, [9, 1]),
        [0xFF, 0x80, 0x40, 0xFF],
        "x = 5.5 is drawn"
    );
    assert_eq!(
        pixel(&pixels, 32, [13, 1]),
        CLEAR,
        "x = 9.5 is past the reveal"
    );
    assert_eq!(
        pixel(&pixels, 32, [19, 1]),
        CLEAR,
        "x = 15.5 is 35.5 along the source"
    );
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn final_alpha_multiplies_the_paint_the_override_and_the_record_once() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let body = square([0.0, 0.0], 4.0, paint(0x33_66_99, 0.5), None);
    let built = upload(&mut gpu, &mut mesh, 13, 2, &[body]);
    let target = gpu.target(8, 4).expect("a target");
    let own = Record {
        opacity: 0.8,
        ..posed(0, 0, 1, [0.0, 0.0])
    };
    let over = Record {
        opacity: 0.8,
        color: Some(Color {
            rgb: 0xFF_80_40,
            opacity: 0.5,
        }),
        ..posed(1, 0, 1, [4.0, 0.0])
    };
    let buf = frame(&[(built.geometry(), &[own, over])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    // 0.5 × 0.8 = 0.4 of 0x336699: 20.4, 40.8, 61.2 and alpha 102.
    assert_eq!(pixel(&pixels, 8, [1, 1]), [20, 41, 61, 102]);
    // 0.5 × 0.5 × 0.8 = 0.2 of 0xFF8040: 51, 25.6, 12.8 and alpha 51.
    assert_eq!(pixel(&pixels, 8, [5, 1]), [51, 26, 13, 51]);
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_full_or_empty_reveal_applies_to_revealed_pieces_only() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    // The border is half the side, so its inset collapses: every inner edge has weight 0.
    let body = square(
        [0.0, 0.0],
        4.0,
        paint(0x33_66_99, 1.0),
        Some((paint(0xFF_80_40, 1.0), 2.0)),
    );
    let built = upload(&mut gpu, &mut mesh, 14, 1, &[body]);
    let target = gpu.target(8, 4).expect("a target");
    let none = Record {
        draw: 0.0,
        ..posed(0, 0, 1, [0.0, 0.0])
    };
    let whole = posed(0, 0, 1, [4.0, 0.0]);
    let buf = frame(&[(built.geometry(), &[none, whole])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(
                pixel(&pixels, 8, [x, y]),
                [0x33, 0x66, 0x99, 0xFF],
                "fill, no border"
            );
            assert_eq!(
                pixel(&pixels, 8, [x + 4, y]),
                [0xFF, 0x80, 0x40, 0xFF],
                "border whole"
            );
        }
    }
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn two_geometries_built_through_one_mesh_draw_together_at_their_own_poses() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let red = square([0.0, 0.0], 2.0, paint(0xFF_00_00, 1.0), None);
    let blue = square([0.0, 0.0], 2.0, paint(0x00_00_FF, 1.0), None);
    let first = upload(&mut gpu, &mut mesh, 15, 1, &[red]);
    let second = upload(&mut gpu, &mut mesh, 16, 2, &[blue]);
    let target = gpu.target(16, 4).expect("a target");
    let buf = frame(&[
        (first.geometry(), &[posed(0, 0, 1, [0.0, 0.0])]),
        (second.geometry(), &[posed(1, 0, 1, [8.0, 0.0])]),
    ]);
    let drawn = gpu
        .draw(&target, &buf, None, &[&second, &first], ORIGIN)
        .expect("it draws");
    assert_eq!(
        drawn,
        Drawn {
            drawn: 2,
            empty: 0,
            unbuilt: 0
        }
    );
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 16, [1, 1]),
        [0xFF, 0, 0, 0xFF],
        "the first at its pose"
    );
    assert_eq!(
        pixel(&pixels, 16, [9, 1]),
        [0, 0, 0xFF, 0xFF],
        "the second at its pose"
    );
    assert_eq!(pixel(&pixels, 16, [5, 1]), CLEAR);
    let drawn = gpu
        .draw(&target, &buf, Some(1), &[&second], ORIGIN)
        .expect("it draws");
    assert_eq!(
        drawn,
        Drawn {
            drawn: 1,
            empty: 0,
            unbuilt: 0
        }
    );
    let pixels = gpu.read(&target);
    assert_eq!(pixel(&pixels, 16, [1, 1]), CLEAR, "only the chosen picture");
    assert_eq!(
        pixel(&pixels, 16, [9, 1]),
        [0, 0, 0xFF, 0xFF],
        "with its own record"
    );
    gpu.release_target(target).expect("its own target");
    gpu.release_built(first).expect("its own geometry");
    gpu.release_built(second).expect("its own geometry");
}

#[test]
fn a_draw_refuses_what_is_not_its_own_before_drawing_anything() {
    let mut gpu = shared();
    let mut other = software(BUDGET);
    let mut mesh = Mesh::new();
    let red = square([0.0, 0.0], 2.0, paint(0xFF_00_00, 1.0), None);
    let built = upload(&mut gpu, &mut mesh, 17, 1, std::slice::from_ref(&red));
    let elsewhere = upload(&mut other, &mut mesh, 17, 1, std::slice::from_ref(&red));
    let unshown = upload(&mut gpu, &mut mesh, 18, 1, &[red]);
    let target = gpu.target(4, 4).expect("a target");
    let foreign = other.target(4, 4).expect("a target");
    let buf = frame(&[(built.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let outside = frame(&[(built.geometry(), &[posed(1, 0, 1, [0.0, 0.0])])]);
    for (target, built, buf, picture) in [
        (&foreign, &built, &buf, None),
        (&target, &elsewhere, &buf, None),
        (&target, &unshown, &buf, None),
        (&target, &built, &buf, Some(1)),
        (&target, &built, &outside, None),
    ] {
        assert_eq!(
            gpu.draw(target, buf, picture, &[built], ORIGIN),
            Err(GpuFailure::Mismatch)
        );
    }
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 4, [1, 1]),
        [0xFF, 0, 0, 0xFF],
        "no refused draw cleared it"
    );
    let elsewhere = gpu
        .release_built(elsewhere)
        .expect_err("another device's geometry is handed back");
    let foreign = gpu
        .release_target(foreign)
        .expect_err("another device's target is handed back");
    other
        .release_built(elsewhere)
        .expect("its own device takes it back");
    other
        .release_target(foreign)
        .expect("its own device takes it back");
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
    gpu.release_built(unshown).expect("its own geometry");
}

#[test]
fn a_picture_without_its_geometry_differs_from_one_that_draws_nothing() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let ink = paint(0xFF_00_00, 1.0);
    let built = upload(
        &mut gpu,
        &mut mesh,
        19,
        1,
        &[label("a label"), square([0.0, 0.0], 2.0, ink, None)],
    );
    let target = gpu.target(4, 4).expect("a target");
    let missing = GeometryId(20);
    let text = posed(0, 0, 1, [0.0, 0.0]);
    let hidden = Record {
        opacity: 0.0,
        ..posed(0, 1, 1, [0.0, 0.0])
    };
    let buf = frame(&[(built.geometry(), &[text, hidden]), (missing, &[text])]);
    let drawn = gpu
        .draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    assert_eq!(
        drawn,
        Drawn {
            drawn: 0,
            empty: 1,
            unbuilt: 1
        }
    );
    assert_eq!(gpu.read(&target), vec![CLEAR; 16], "neither draws");
    let after = frame(&[(built.geometry(), &[text, posed(0, 1, 1, [2.0, 2.0])])]);
    let drawn = gpu
        .draw(&target, &after, None, &[&built], ORIGIN)
        .expect("it draws");
    assert_eq!(
        drawn,
        Drawn {
            drawn: 1,
            empty: 0,
            unbuilt: 0
        }
    );
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 4, [3, 3]),
        [0xFF, 0, 0, 0xFF],
        "the rect after the label"
    );
    assert_eq!(pixel(&pixels, 4, [1, 1]), CLEAR);
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_draw_at_reveal_zero_is_issued_and_inks_nothing() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let line = stroke(
        vec![Vec2::new(0.0, 2.0), Vec2::new(8.0, 2.0)],
        paint(0xFF_00_00, 1.0),
    );
    let built = upload(&mut gpu, &mut mesh, 29, 1, &[line]);
    let target = gpu.target(8, 4).expect("a target");
    let unrevealed = Record {
        draw: 0.0,
        ..posed(0, 0, 1, [0.0, 0.0])
    };
    let buf = frame(&[(built.geometry(), &[unrevealed])]);
    assert_eq!(
        gpu.draw(&target, &buf, None, &[&built], ORIGIN),
        Ok(Drawn {
            drawn: 1,
            empty: 0,
            unbuilt: 0
        }),
        "a draw is issued"
    );
    assert_eq!(gpu.read(&target), vec![CLEAR; 32], "and covers nothing");
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_closed_two_point_line_retraces_only_when_dashed_or_partly_revealed() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    // Out from x = 0 to 16 and back: a pixel at x is x along the way out and 32 - x back.
    let ink = paint(0xFF_80_20, 0.4);
    let there_and_back = |dashed| Prim::Line {
        points: vec![Vec2::new(0.0, 2.0), Vec2::new(16.0, 2.0)].into(),
        closed: true,
        color: ink,
        width: 2.0,
        dashed,
    };
    let built = upload(
        &mut gpu,
        &mut mesh,
        30,
        1,
        &[there_and_back(false), there_and_back(true)],
    );
    let target = gpu.target(16, 12).expect("a target");
    let whole = posed(0, 0, 1, [0.0, 0.0]);
    let partial = Record {
        draw: 0.75,
        ..posed(0, 0, 1, [0.0, 4.0])
    };
    let dashed = posed(0, 1, 1, [0.0, 8.0]);
    let buf = frame(&[(built.geometry(), &[whole, partial, dashed])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    // 0.4 of 0xFF8020 once: 102, 51.2, 12.8 and alpha 102. Twice, over the first layer's stored
    // bytes times 0.6, in float: 163.2, 51.2 + 30.6 = 81.8, 12.8 + 7.8 = 20.6; with the source
    // rounded to bytes first: 163.2, 51 + 30.6 = 81.6, 13 + 7.8 = 20.8. Either way 163, 82, 21.
    let once = [102, 51, 13, 102];
    let twice = [163, 82, 21, 163];
    for (at, expected, why) in [
        (
            [8, 1],
            once,
            "whole and undashed: the return leg is skipped",
        ),
        (
            [4, 5],
            once,
            "revealed to 24 of 32: x = 4.5 is 27.5 back, past it",
        ),
        ([12, 5], twice, "x = 12.5 is 19.5 back, inside the reveal"),
        ([3, 9], once, "dashed: 3.5 out is on, 28.5 back is in a gap"),
        (
            [10, 9],
            twice,
            "10.5 out and 21.5 back are both on: 0.5 and 1.5 into a period",
        ),
    ] {
        assert_eq!(pixel(&pixels, 16, at), expected, "{why}");
    }
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_border_s_partial_reveal_cuts_across_its_band_where_its_weight_says() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    // An 8 by 8 border 1 wide, revealed clockwise from the top right corner. The right band is
    // a quarter of the outline, y = 0 to 8 outside and 1 to 7 inside, so a reveal of 1/8 cuts
    // both edges at y = 4; compared through reveal alone, the cut would reach y = 5 inside.
    let ring = Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(8.0, 8.0),
        },
        radius: 0.0,
        fill: None,
        border: Some((paint(0xFF_80_40, 1.0), 1.0)),
    };
    let built = upload(&mut gpu, &mut mesh, 33, 1, &[ring]);
    let target = gpu.target(8, 8).expect("a target");
    let record = Record {
        draw: 0.125,
        ..posed(0, 0, 1, [0.0, 0.0])
    };
    let buf = frame(&[(built.geometry(), &[record])]);
    gpu.draw(&target, &buf, None, &[&built], ORIGIN)
        .expect("it draws");
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 8, [7, 3]),
        [0xFF, 0x80, 0x40, 0xFF],
        "y = 3 to 4 is revealed"
    );
    assert_eq!(
        pixel(&pixels, 8, [7, 4]),
        CLEAR,
        "y = 4 to 5 is past the cut on both edges"
    );
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn the_last_record_slot_draws_through_a_selected_picture() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let built = upload(&mut gpu, &mut mesh, 31, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(4, 4).expect("a target");
    let before = vec![filler(0, 0, 1); RECORDS - 1];
    let buf = frame(&[
        (GeometryId(32), &before),
        (built.geometry(), &[posed(0, 0, 1, [2.0, 2.0])]),
    ]);
    let drawn = gpu
        .draw(&target, &buf, Some(1), &[&built], ORIGIN)
        .expect("it draws");
    assert_eq!(
        drawn,
        Drawn {
            drawn: 1,
            empty: 0,
            unbuilt: 0
        }
    );
    let pixels = gpu.read(&target);
    assert_eq!(
        pixel(&pixels, 4, [3, 3]),
        [0xFF, 0, 0, 0xFF],
        "slot 2,999 at its pose"
    );
    assert_eq!(pixel(&pixels, 4, [1, 1]), CLEAR);
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_view_subtracts_its_origin_then_scales() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let built = upload(&mut gpu, &mut mesh, 34, 1, &[red_square([0.0, 0.0])]);
    let target = gpu.target(6, 6).expect("a target");
    let buf = frame(&[(built.geometry(), &[posed(0, 0, 1, [1.0, 1.0])])]);
    let view = View {
        origin: [1.0, 1.0],
        scale: 2.0,
    };
    gpu.draw(&target, &buf, None, &[&built], view)
        .expect("it draws");
    let pixels = gpu.read(&target);
    // The square spans world 1 to 3: (1 - 1) x 2 to (3 - 1) x 2 is pixels 0 to 4.
    assert_eq!(pixel(&pixels, 6, [0, 0]), [0xFF, 0, 0, 0xFF]);
    assert_eq!(pixel(&pixels, 6, [3, 3]), [0xFF, 0, 0, 0xFF]);
    assert_eq!(pixel(&pixels, 6, [4, 4]), CLEAR);
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}

#[test]
fn a_target_charges_both_its_textures_to_the_byte_and_nothing_when_refused() {
    let _one_at_a_time = shared();
    let exact = RECORDS_BYTES + TARGET_8_BYTES;
    assert_eq!(
        Gpu::software(RECORDS_BYTES - 1).err(),
        Some(GpuFailure::Budget),
        "the record buffer comes first"
    );
    let records_only = software(RECORDS_BYTES);
    assert_eq!(
        records_only.charged(),
        RECORDS_BYTES,
        "and fits to the byte"
    );
    drop(records_only);
    let mut short = software(exact - 1);
    assert_eq!(short.target(8, 8).err(), Some(GpuFailure::Budget));
    assert_eq!(short.charged(), RECORDS_BYTES);
    drop(short);
    let mut gpu = software(BUDGET);
    for (width, height) in [(0, 8), (8, 0), (8_193, 1), (1, 8_193)] {
        assert_eq!(gpu.target(width, height).err(), Some(GpuFailure::Size));
    }
    let wide = gpu.target(8_192, 1).expect("the device's limit fits");
    gpu.release_target(wide).expect("its own target");
    let tall = gpu.target(1, 8_192).expect("in height too");
    gpu.release_target(tall).expect("its own target");
    gpu.refuse_second();
    assert_eq!(
        gpu.target(8, 8).err(),
        Some(GpuFailure::Refused),
        "the second texture fails"
    );
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES,
        "the first is rolled back with it"
    );
    let mut gpu = software(exact);
    let target = gpu.target(8, 8).expect("it fits to the byte");
    assert_eq!(gpu.charged(), exact);
    gpu.release_target(target).expect("its own target");
    assert_eq!(
        gpu.charged(),
        RECORDS_BYTES,
        "never drawn: refunded at once"
    );
    let target = gpu.target(8, 8).expect("it fits again");
    gpu.draw(&target, &FrameBuf::new(), None, &[], ORIGIN)
        .expect("it clears");
    gpu.release_target(target).expect("its own target");
    assert_eq!(
        gpu.charged(),
        exact,
        "still charged while its submission may run"
    );
    gpu.settle();
    let target = gpu
        .target(8, 8)
        .expect("refunded once it retired: it fits again");
    assert_eq!(gpu.charged(), exact);
    gpu.release_target(target).expect("its own target");
}

#[test]
fn no_debug_or_error_output_carries_drawing_content() {
    let mut gpu = shared();
    let mut mesh = Mesh::new();
    let marked = square([1_234.5, 678.25], 3.0, paint(0xAB_CD_EF, 1.0), None);
    let built = upload(&mut gpu, &mut mesh, 22, 1, &[marked]);
    let target = gpu.target(4, 4).expect("a target");
    let view = View {
        origin: [1_234.5, 678.25],
        scale: 1.0,
    };
    let buf = frame(&[(built.geometry(), &[posed(0, 0, 1, [0.0, 0.0])])]);
    let drawn = gpu
        .draw(&target, &buf, None, &[&built], view)
        .expect("it draws");
    let printed = format!(
        "{gpu:?} {built:?} {built:#?} {target:?} {drawn:?} {view:?} {:?}",
        [
            GpuFailure::NoAdapter,
            GpuFailure::NoDevice,
            GpuFailure::Budget,
            GpuFailure::Size,
            GpuFailure::Refused,
            GpuFailure::Unfinished,
            GpuFailure::Mismatch,
            GpuFailure::Lost,
        ]
        .map(|failure| format!("{failure} {failure:?}"))
    );
    let lists = [
        format!("{:?}", 1_234.5_f32.to_le_bytes()),
        format!("{:?}", 678.25_f32.to_le_bytes()),
        format!("{:?}", [0xAB_u8, 0xCD, 0xEF]),
    ];
    for marker in ["1234.5", "1234", "678.25", "11259375", "abcdef", "ABCDEF"]
        .into_iter()
        .chain(lists.iter().map(String::as_str))
    {
        assert!(!printed.contains(marker), "no {marker} in {printed}");
    }
    gpu.release_target(target).expect("its own target");
    gpu.release_built(built).expect("its own geometry");
}
