//! A view's commit-time solids, and their canonical prepared meshes.
//!
//! Admission composes [`ViewInput`] once per changed view; preparation turns it into a
//! [`ViewGeometry`] that shares each unchanged solid's mesh with the revision before it.

use super::{
    Transform, Triangle,
    camera::{Camera, Light},
    mesh::{MeshScratch, build_local},
    size::{MeshError, triangles},
};
use crate::{
    Handle, Op, WireId,
    geom::Vec3,
    layout::heap,
    record::{Color, Rgb, Solid, Token, View3d},
    theme::Theme,
};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, atomic::AtomicBool},
};

/// One view after a commit: its new inputs, or `None` when the step removed it.
#[derive(Clone, PartialEq)]
pub struct ViewUpdate {
    /// The view's model id.
    pub id: WireId,
    /// Every solid of the view after the step, or `None` for a removed view.
    pub input: Option<Arc<ViewInput>>,
}
impl fmt::Debug for ViewUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewUpdate")
            .field("removed", &self.input.is_none())
            .finish_non_exhaustive()
    }
}

/// Every solid of one view after a step, with rest transforms composed down attach chains.
#[derive(PartialEq)]
pub struct ViewInput {
    /// The view's identity.
    pub handle: Handle,
    /// The admitted `view3d`, shared with the scene's object.
    pub source: Arc<Op>,
    /// Attach order: a parent precedes its children.
    pub solids: Box<[SolidInput]>,
}
impl fmt::Debug for ViewInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewInput")
            .field("solids", &self.solids.len())
            .finish_non_exhaustive()
    }
}

/// One solid as committed: its shared record, latest authored colour and placement.
#[derive(PartialEq)]
pub struct SolidInput {
    /// Model id.
    pub id: WireId,
    /// Identity.
    pub handle: Handle,
    /// The admitted `solid`, cloned once at its put and shared from then on.
    pub source: Arc<Op>,
    /// The latest authored colour: a `set`, else the record's.
    pub color: Option<Color>,
    /// Index of the attachment parent in [`ViewInput::solids`].
    pub parent: Option<u16>,
    /// The solid's own position, rotation and scale.
    pub local: Transform,
    /// The complete composed rest transform, where world checks run.
    pub rest: Transform,
}
impl fmt::Debug for SolidInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SolidInput")
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}
impl SolidInput {
    fn record(&self) -> &Solid {
        match &*self.source {
            Op::Solid(solid) => solid,
            _ => unreachable!("solid inputs hold solid records"),
        }
    }
}
impl ViewInput {
    fn record(&self) -> &View3d {
        match &*self.source {
            Op::View3d(view) => view,
            _ => unreachable!("view inputs hold view records"),
        }
    }
    /// Bytes this input owns beyond the shared sources: the box, the solid array and ids.
    pub(crate) fn owned_bytes(&self) -> usize {
        heap::ARC_HEADER
            + size_of::<Self>()
            + size_of_val(&*self.solids)
            + self
                .solids
                .iter()
                .map(|solid| solid.id.as_str().len())
                .sum::<usize>()
    }
}

/// One source face: three anchors giving its plane, and its run of triangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceRange {
    /// Vertex indices of the face's first triangle, which every triangle of the face shares.
    pub anchors: [u32; 3],
    /// First triangle.
    pub start: u32,
    /// Triangle count.
    pub len: u32,
}

/// A solid's canonical mesh at its authored dimensions, before scale or placement.
pub struct SolidMesh {
    /// Distinct local vertices.
    pub vertices: Box<[Vec3]>,
    /// Outward-wound triangles indexing `vertices`.
    pub triangles: Box<[[u32; 3]]>,
    /// Source faces in emission order.
    pub faces: Box<[FaceRange]>,
}
impl fmt::Debug for SolidMesh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SolidMesh")
            .field("triangles", &self.triangles.len())
            .finish_non_exhaustive()
    }
}
impl SolidMesh {
    fn bytes(&self) -> usize {
        heap::ARC_HEADER
            + size_of::<Self>()
            + size_of_val(&*self.vertices)
            + size_of_val(&*self.triangles)
            + size_of_val(&*self.faces)
    }
}

/// A solid's prepared mesh, or why it couldn't be built at rest.
#[derive(Debug, Clone)]
pub enum Mesh {
    /// Built and shareable across revisions.
    Built(Arc<SolidMesh>),
    /// Kept while the source is unchanged; its view draws nothing.
    Failed(MeshError),
}

/// One prepared solid.
pub struct PreparedSolid {
    /// Model id.
    pub id: WireId,
    /// Identity.
    pub handle: Handle,
    /// The committed record, shared with the scene.
    pub source: Arc<Op>,
    /// Index of the attachment parent in [`ViewGeometry::solids`].
    pub parent: Option<u16>,
    /// Own position, rotation and scale; poses compose these parent-first.
    pub local: Transform,
    /// Composed rest transform.
    pub rest: Transform,
    /// Canonical mesh or its failure.
    pub mesh: Mesh,
    /// Theme-resolved colour.
    pub rgb: [u8; 3],
    /// Authored opacity, 1 when omitted.
    pub opacity: f32,
}
impl fmt::Debug for PreparedSolid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedSolid")
            .field("mesh", &self.mesh)
            .finish_non_exhaustive()
    }
}

/// One view's prepared solids, published whole: a failed solid empties the view.
pub struct ViewGeometry {
    /// The view's identity.
    pub handle: Handle,
    /// The committed `view3d`.
    pub source: Arc<Op>,
    /// The authored camera over the built solids' rest bounds; `None` when nothing is built.
    pub camera: Option<Camera>,
    /// The authored light.
    pub light: Light,
    /// Attach order: a parent precedes its children.
    pub solids: Box<[PreparedSolid]>,
    /// Some solid failed, so the view draws nothing.
    pub failed: bool,
}
impl fmt::Debug for ViewGeometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewGeometry")
            .field("solids", &self.solids.len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl ViewGeometry {
    /// Bytes this view retains on its own: the box, its solid array, ids, every mesh and
    /// every source once with its `Arc` header. A rest owner charges this in full.
    pub fn full_bytes(&self) -> usize {
        self.marginal_bytes(None)
    }
    /// Like [`Self::full_bytes`], leaving out meshes and sources `before` already holds.
    pub fn marginal_bytes(&self, before: Option<&Self>) -> usize {
        let held = |solid: &PreparedSolid| {
            before.and_then(|old| old.solids.iter().find(|old| old.id == solid.id))
        };
        let mut bytes = heap::ARC_HEADER
            + size_of::<Self>()
            + size_of_val(&*self.solids)
            + before.map_or_else(|| heap::source_bytes(&self.source), |_| 0);
        for solid in &*self.solids {
            let old = held(solid);
            bytes += solid.id.as_str().len();
            if !old.is_some_and(|old| Arc::ptr_eq(&old.source, &solid.source)) {
                bytes += heap::source_bytes(&solid.source);
            }
            if let Mesh::Built(mesh) = &solid.mesh
                && !old
                    .is_some_and(|old| matches!(&old.mesh, Mesh::Built(m) if Arc::ptr_eq(m, mesh)))
            {
                bytes += mesh.bytes();
            }
        }
        bytes
    }

    /// The same view without the solids a cut dropped, sharing every mesh.
    #[must_use]
    pub(crate) fn retain(&self, keep: impl Fn(Handle) -> bool) -> Self {
        let mut index = Vec::with_capacity(self.solids.len());
        let mut solids = Vec::with_capacity(self.solids.len());
        for solid in &*self.solids {
            let parent = solid
                .parent
                .and_then(|p| index.get(usize::from(p)).copied().flatten());
            let kept = keep(solid.handle) && (solid.parent.is_none() || parent.is_some());
            index.push(
                kept.then(|| u16::try_from(solids.len()).expect("a view holds at most 160 solids")),
            );
            if kept {
                solids.push(PreparedSolid {
                    id: solid.id.clone(),
                    handle: solid.handle,
                    source: Arc::clone(&solid.source),
                    parent,
                    local: solid.local,
                    rest: solid.rest,
                    mesh: solid.mesh.clone(),
                    rgb: solid.rgb,
                    opacity: solid.opacity,
                });
            }
        }
        let failed = solids
            .iter()
            .any(|solid| matches!(solid.mesh, Mesh::Failed(_)));
        Self {
            handle: self.handle,
            source: Arc::clone(&self.source),
            camera: camera(self.record(), &solids),
            light: self.light,
            solids: solids.into_boxed_slice(),
            failed,
        }
    }

    fn record(&self) -> &View3d {
        match &*self.source {
            Op::View3d(view) => view,
            _ => unreachable!("prepared views hold view records"),
        }
    }
}

/// Builds every solid whose record changed since `before` and shares the rest.
///
/// A solid that fails at rest keeps that failure while its record is unchanged, and the view
/// is published empty. Only cancellation and an exhausted output fail the call.
/// # Errors
/// Returns `Cancelled` or `Full`; nothing is published.
pub fn tessellate_view(
    input: &ViewInput,
    before: Option<&ViewGeometry>,
    theme: &Theme,
    cancel: &AtomicBool,
    scratch: &mut MeshScratch,
) -> Result<ViewGeometry, MeshError> {
    let mut out = Vec::new();
    let mut groups = Vec::new();
    let mut solids = Vec::with_capacity(input.solids.len());
    for (part, solid) in input.solids.iter().enumerate() {
        let reused = before
            .and_then(|old| old.solids.iter().find(|old| old.id == solid.id))
            .filter(|old| Arc::ptr_eq(&old.source, &solid.source))
            .map(|old| old.mesh.clone());
        let mesh = match reused {
            Some(mesh) => mesh,
            None => build(solid, part, &mut out, &mut groups, cancel, scratch)?,
        };
        let record = solid.record();
        solids.push(PreparedSolid {
            id: solid.id.clone(),
            handle: solid.handle,
            source: Arc::clone(&solid.source),
            parent: solid.parent,
            local: solid.local,
            rest: solid.rest,
            mesh,
            rgb: rgb(theme.resolve(&solid.color.unwrap_or(Color::Token(Token::Accent)))),
            opacity: record.opacity.map_or(1.0, crate::record::Unit::get),
        });
    }
    let failed = solids
        .iter()
        .any(|solid| matches!(solid.mesh, Mesh::Failed(_)));
    Ok(ViewGeometry {
        handle: input.handle,
        source: Arc::clone(&input.source),
        camera: camera(input.record(), &solids),
        light: Light::authored(input.record()),
        solids: solids.into_boxed_slice(),
        failed,
    })
}

fn build(
    solid: &SolidInput,
    part: usize,
    out: &mut Vec<Triangle>,
    groups: &mut Vec<u8>,
    cancel: &AtomicBool,
    scratch: &mut MeshScratch,
) -> Result<Mesh, MeshError> {
    out.clear();
    groups.clear();
    let built = triangles(solid.record()).and_then(|count| {
        out.reserve(usize::try_from(count).map_err(|_| MeshError::Full)?);
        let part = u16::try_from(part).map_err(|_| MeshError::Full)?;
        build_local(
            solid.record(),
            &solid.rest,
            part,
            out,
            groups,
            cancel,
            scratch,
        )
    });
    match built {
        Ok(_) => Ok(Mesh::Built(Arc::new(index(out, groups)))),
        Err(error @ (MeshError::Cancelled | MeshError::Full)) => Err(error),
        Err(error) => Ok(Mesh::Failed(error)),
    }
}

/// Merges exactly equal vertices and gives each source face its triangle run.
fn index(out: &[Triangle], groups: &[u8]) -> SolidMesh {
    let mut seen = HashMap::with_capacity(out.len());
    let mut vertices = Vec::new();
    let triangles: Vec<[u32; 3]> = out
        .iter()
        .map(|triangle| {
            triangle.v.map(|v| {
                *seen
                    .entry([v.x, v.y, v.z].map(f32::to_bits))
                    .or_insert_with(|| {
                        vertices.push(v);
                        u32::try_from(vertices.len() - 1).expect("a solid's vertices fit u32")
                    })
            })
        })
        .collect();
    let mut start = 0_u32;
    let faces = groups
        .iter()
        .map(|&len| {
            let face = FaceRange {
                anchors: triangles[usize::try_from(start).expect("u32 fits usize")],
                start,
                len: u32::from(len),
            };
            start += u32::from(len);
            face
        })
        .collect();
    SolidMesh {
        vertices: vertices.into_boxed_slice(),
        triangles: triangles.into_boxed_slice(),
        faces,
    }
}

fn camera(view: &View3d, solids: &[PreparedSolid]) -> Option<Camera> {
    let mut bounds = [[f64::INFINITY; 3], [f64::NEG_INFINITY; 3]];
    for solid in solids {
        let Mesh::Built(mesh) = &solid.mesh else {
            return None;
        };
        for &v in &*mesh.vertices {
            let p = solid.rest.point(v).ok()?;
            for (i, c) in [p.x, p.y, p.z].into_iter().enumerate() {
                bounds[0][i] = bounds[0][i].min(f64::from(c));
                bounds[1][i] = bounds[1][i].max(f64::from(c));
            }
        }
    }
    Camera::authored(view, bounds).ok()
}

fn rgb(color: Rgb) -> [u8; 3] {
    let [_, r, g, b] = color.get().to_be_bytes();
    [r, g, b]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Admission, GlobalBudget, Record, SceneState, parse_record};

    #[test]
    fn cancellation_is_never_kept_as_a_failed_solid() {
        let Ok(Record::Begin(header)) =
            parse_record(br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":""}}"#)
        else {
            panic!("begin fixture");
        };
        let Ok(Record::Step(step)) = parse_record(
            br#"{"step":{"id":"a","put":[{"view3d":{"id":"v"}},{"solid":{"in":"v","id":"s","shape":"box","size":[2,2,2]}}]}}"#,
        ) else {
            panic!("step fixture");
        };
        let (mut scene, mut budget) = (
            SceneState::begin(header).expect("scene"),
            GlobalBudget::default(),
        );
        let Admission::New(candidate) = scene.admit(&step, &budget).expect("admitted") else {
            panic!("new step");
        };
        let committed = scene.commit(candidate, &mut budget).expect("fresh");
        let input = committed.preparation.views[0]
            .input
            .clone()
            .expect("live view");
        let mut scratch = MeshScratch::new();
        let theme = Theme::default();
        assert_eq!(
            tessellate_view(&input, None, &theme, &AtomicBool::new(true), &mut scratch).err(),
            Some(MeshError::Cancelled)
        );
        let geometry = tessellate_view(&input, None, &theme, &AtomicBool::new(false), &mut scratch)
            .expect("built after a cancelled attempt");
        assert!(!geometry.failed);
    }
}
