use kumochi_scene::{
    Handle,
    geom::Vec3,
    three::{
        Transform,
        camera::{Camera, Light},
        frame::Solid,
        pick::{FrameState, Mapping},
        view::FaceRange,
    },
};

pub(super) const SQUARE: [Vec3; 4] = [
    Vec3::new(-1.0, -1.0, 0.0),
    Vec3::new(1.0, -1.0, 0.0),
    Vec3::new(1.0, 1.0, 0.0),
    Vec3::new(-1.0, 1.0, 0.0),
];
pub(super) fn camera() -> Camera {
    Camera::new(0.0, 0.0, 2.0, 90.0, [0.0; 3]).expect("valid fixture")
}

pub(super) fn light() -> Light {
    Light::new([0.0, 0.0, -1.0], 0.0).expect("valid fixture")
}

pub(super) fn identity() -> Transform {
    Transform::local([0.0; 3], [0.0; 3], 1.0)
}

/// A source face as these fixtures write it: its anchors and its own triangles.
pub(super) struct Face<'a> {
    pub(super) anchors: [u32; 3],
    pub(super) triangles: &'a [[u32; 3]],
}
/// Packs fixture faces into one solid's shared triangle array and face ranges, as a prepared mesh
/// stores them; the few fixture bytes are leaked so the solid stays `Copy`.
pub(super) fn solid(vertices: &[Vec3], faces: &[Face<'_>]) -> Solid<'static> {
    let mut triangles = Vec::new();
    let mut ranges = Vec::new();
    for face in faces {
        ranges.push(FaceRange {
            anchors: face.anchors,
            start: u32::try_from(triangles.len()).expect("fixture triangle start"),
            len: u32::try_from(face.triangles.len()).expect("fixture face size"),
        });
        triangles.extend_from_slice(face.triangles);
    }
    let vertices: Vec<[f64; 3]> = vertices
        .iter()
        .map(|v| [v.x, v.y, v.z].map(f64::from))
        .collect();
    Solid {
        vertices: Vec::leak(vertices),
        triangles: Vec::leak(triangles),
        faces: Vec::leak(ranges),
        pose: identity(),
        rgb: [200, 100, 50],
        opacity: 1.0,
    }
}

pub(super) fn handle(slot: usize) -> Handle {
    Handle::new(3, slot).expect("fixture handle")
}

pub(super) fn state(mapping: Mapping, parts: Vec<Option<Handle>>) -> FrameState {
    FrameState {
        revision: 7,
        camera: camera(),
        pose: vec![identity(); parts.len()],
        mapping,
        parts,
    }
}

pub(super) fn mapping(size: [u32; 2]) -> Mapping {
    Mapping::new([10.0, -20.0], [231.0, 117.0], 1.5, size).expect("fixture mapping")
}

pub(super) fn pointer(mapping: Mapping, texel: [f64; 2]) -> [f64; 2] {
    let [origin, extent] = mapping.destination();
    std::array::from_fn(|i| origin[i] + texel[i] * extent[i] / f64::from(mapping.texels()[i]))
}
