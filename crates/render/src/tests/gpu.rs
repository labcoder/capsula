//! The GPU side's device and uploads on Direct3D 12's software adapter: charges worked by hand
//! from each fixture's mesh, and range tables that outlive the mesh's next build.

use std::{thread, time::Duration};

use kumochi_scene::{
    display::{Paint, Prim},
    geom::{Rect, Vec2},
    record::Rgb,
};

use super::{BuildInput, Mesh, build, cache, filler};
use crate::{Built, GeometryId, Gpu, GpuFailure, limits::UPLOADED};

/// Room for every test's uploads.
const BUDGET: u64 = 64 * 1024 * 1024;
/// A filled square's upload: lyon fills it with 4 vertices of 28 bytes and 2 triangles of
/// 4-byte indices, and it paints 1 piece of 32 bytes.
const SQUARE_BYTES: u64 = 4 * 28 + 6 * 4 + 32;

fn software(budget: u64) -> Gpu {
    let gpu = Gpu::software(budget)
        .expect("GPU tests need Direct3D 12's software adapter, which this machine lacks");
    assert!(gpu.on_cpu(), "the software adapter is a CPU adapter");
    gpu
}

fn square(at: [f32; 2], rgb: u32) -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(at[0], at[1]),
            size: Vec2::new(2.0, 2.0),
        },
        radius: 0.0,
        fill: Some(Paint {
            rgb: Rgb::try_from(rgb).expect("fixture colour"),
            opacity: 1.0,
        }),
        border: None,
    }
}

/// Builds geometry `id` of `objects` objects holding `prims` through `mesh`.
fn built(mesh: &mut Mesh, id: u64, objects: usize, prims: &[Prim]) {
    build(&mut BuildInput::new(), mesh, &cache(id, objects, prims)).expect("it builds");
}

fn upload(gpu: &mut Gpu, mesh: &mut Mesh, id: u64, objects: usize, prims: &[Prim]) -> Built {
    built(mesh, id, objects, prims);
    gpu.upload(mesh).expect("it uploads")
}

#[test]
fn an_upload_charges_its_buffers_to_the_byte_and_nothing_when_refused() {
    let mut mesh = Mesh::new();
    let mut gpu = software(SQUARE_BYTES - 1);
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Unfinished),
        "nothing built"
    );
    built(&mut mesh, 1, 1, &[square([0.0, 0.0], 0xFF_00_00)]);
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Budget),
        "one byte short"
    );
    assert_eq!(gpu.charged(), 0, "a refusal charges nothing");
    assert_eq!(gpu.free_tables(), UPLOADED, "and holds no slot");
    let mut gpu = software(SQUARE_BYTES);
    let one = gpu.upload(&mesh).expect("it fits to the byte");
    assert_eq!(gpu.charged(), SQUARE_BYTES);
    mesh.begin(GeometryId(2));
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Unfinished),
        "a build is open"
    );
    gpu.release_built(one).expect("its own geometry");
    assert_eq!(gpu.charged(), 0, "refunded at once");
    assert_eq!(gpu.free_tables(), UPLOADED);
}

#[test]
fn uploads_fit_their_slots_and_one_more_waits_for_a_release() {
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let mut held: Vec<Built> = (0..UPLOADED)
        .map(|_| upload(&mut gpu, &mut mesh, 3, 1, &[square([0.0, 0.0], 0xFF_00_00)]))
        .collect();
    let charged = gpu.charged();
    assert_eq!(charged, 5 * SQUARE_BYTES, "five slots, each a square");
    assert_eq!(
        gpu.upload(&mesh).err(),
        Some(GpuFailure::Budget),
        "every slot is held"
    );
    assert_eq!(gpu.charged(), charged, "the refusal charges nothing");
    let mut other = software(BUDGET);
    let elsewhere = other.upload(&mesh).expect("another device's slot");
    assert_eq!(gpu.release_built(elsewhere), Err(GpuFailure::Mismatch));
    assert_eq!(gpu.free_tables(), 0, "a foreign release frees nothing here");
    let first = held.pop().expect("an upload");
    gpu.release_built(first).expect("its own geometry");
    held.push(gpu.upload(&mesh).expect("the released slot takes the next"));
    for built in held {
        gpu.release_built(built).expect("its own geometry");
    }
}

#[test]
fn an_upload_keeps_its_range_tables_through_the_mesh_s_next_build() {
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let red = square([0.0, 0.0], 0xFF_00_00);
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
fn a_lost_device_refuses_every_later_call() {
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let one = upload(&mut gpu, &mut mesh, 6, 1, &[square([0.0, 0.0], 0xFF_00_00)]);
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
        assert_eq!(gpu.upload(&mesh).err(), Some(GpuFailure::Lost));
    }
    gpu.release_built(one).expect("its own geometry");
}

#[test]
fn no_debug_or_error_output_carries_drawing_content() {
    let mut mesh = Mesh::new();
    let mut gpu = software(BUDGET);
    let marked = square([1_234.5, 678.25], 0xAB_CD_EF);
    let one = upload(&mut gpu, &mut mesh, 7, 1, &[marked]);
    let printed = format!(
        "{gpu:?} {one:?} {one:#?} {:?}",
        [
            GpuFailure::NoAdapter,
            GpuFailure::NoDevice,
            GpuFailure::Budget,
            GpuFailure::Refused,
            GpuFailure::Unfinished,
            GpuFailure::Mismatch,
            GpuFailure::Lost,
        ]
        .map(|failure| failure.to_string())
    );
    for marker in ["1234.5", "1234", "678.25", "11259375", "abcdef", "ABCDEF"] {
        assert!(!printed.contains(marker), "no {marker} in {printed}");
    }
    gpu.release_built(one).expect("its own geometry");
}
