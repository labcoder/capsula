//! 3D motion: a view's track owners aligned to its solids, its spin, and the pose sampler.
use super::{Live, Value};
use crate::{
    Op,
    record::Track,
    three::{Transform, camera::Camera, view::ViewGeometry},
};
use std::{fmt, sync::Arc};

/// How long an authored spin turns the camera after its view is first put.
const SPIN_MS: u64 = 15_000;

/// A 3D track's index in a solid's owner pair.
pub(super) fn slot(track: Track) -> Option<usize> {
    match track {
        Track::Pos3 => Some(0),
        Track::Rot3 => Some(1),
        _ => None,
    }
}

/// A view's automatic orbit, from the activation that first put the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Spin {
    degrees_per_s: f64,
    start: u64,
    held: Option<u64>,
}
impl Spin {
    /// When the turn stops: its 15 s end, a hold or the scene deadline, whichever comes first.
    pub(super) fn stop(&self, deadline: u64) -> u64 {
        self.start
            .saturating_add(SPIN_MS)
            .min(deadline)
            .min(self.held.unwrap_or(u64::MAX))
    }
    pub(super) fn running(&self, now: u64, deadline: u64) -> bool {
        self.start <= now && now < self.stop(deadline)
    }
    /// Yaw added to the authored camera at `now`, unwrapped; the camera wraps it.
    pub(super) fn degrees(&self, now: u64, deadline: u64) -> f64 {
        let elapsed = now.min(self.stop(deadline)).saturating_sub(self.start);
        let elapsed = u32::try_from(elapsed).expect("a spin turns for at most 15 s");
        self.degrees_per_s * f64::from(elapsed) / 1000.0
    }
    pub(super) fn hold(&mut self, now: u64) {
        self.held.get_or_insert(now);
    }
}

/// The scene's view as activated: its geometry, each solid's `pos3`/`rot3` owner (an index into
/// the view's tracks) in `geometry.solids` order, its spin, and the yaw a Stop carried into it.
pub(super) struct ViewMotion {
    pub(super) geometry: Arc<ViewGeometry>,
    pub(super) owners: Box<[[Option<usize>; 2]]>,
    pub(super) spin: Option<Spin>,
    pub(super) yaw: f64,
}
impl ViewMotion {
    /// A view with no owners. A view keeps the spin of the view it continues, and a newly put
    /// view starts its authored spin at `now`; `None` installs it still.
    pub(super) fn new(
        geometry: &Arc<ViewGeometry>,
        previous: Option<&Self>,
        now: Option<u64>,
    ) -> Self {
        let same = previous.filter(|previous| previous.geometry.handle == geometry.handle);
        let spin = match (same, now) {
            (Some(previous), _) => previous.spin,
            (_, Some(now)) => match &*geometry.source {
                Op::View3d(view) => view.spin.map(|spin| Spin {
                    degrees_per_s: f64::from(spin.get()),
                    start: now,
                    held: None,
                }),
                _ => None,
            },
            _ => None,
        };
        Self {
            geometry: Arc::clone(geometry),
            owners: vec![[None; 2]; geometry.solids.len()].into_boxed_slice(),
            spin,
            yaw: same.map_or(0.0, |previous| previous.yaw),
        }
    }
    /// Requested bytes of the owner array; the geometry belongs to the layout.
    pub(super) fn bytes(&self) -> usize {
        size_of_val(&*self.owners)
    }

    /// Poses every solid at `now` into `out` without allocating. Each property takes its owner's
    /// sample, first-key hold included, until the owner ends or the deadline passes; otherwise its
    /// settled offset, so an unowned solid sits at its rest bit for bit.
    pub(super) fn sample(&self, tracks: &[Live], now: u64, deadline: u64, out: &mut ViewFrame) {
        out.poses.clear();
        for (solid, owners) in self.geometry.solids.iter().zip(&*self.owners) {
            let owned = |slot: usize| {
                owners[slot]
                    .map(|owner| &tracks[owner])
                    .filter(|live| now < deadline && now < live.end())
                    .map(|live| live.sample(now))
            };
            let local = match (owned(0), owned(1)) {
                (None, None) => solid.local,
                (pos3, rot3) => {
                    let offset = |value: Option<Value>, settled: [f32; 3]| match value {
                        Some(Value::V3(value)) => value,
                        _ => settled,
                    };
                    solid.posed(offset(pos3, solid.pos3), offset(rot3, solid.rot3))
                }
            };
            let pose = solid
                .parent
                .map_or(local, |parent| local.then(&out.poses[usize::from(parent)]));
            out.poses.push(pose);
        }
        out.camera = self
            .geometry
            .camera
            .map(|camera| camera.yawed(self.degrees(now, deadline)));
    }

    /// The yaw added to the authored camera at `now`: the carried yaw plus the spin's turn.
    pub(super) fn degrees(&self, now: u64, deadline: u64) -> f64 {
        self.yaw + self.spin.map_or(0.0, |spin| spin.degrees(now, deadline))
    }
}

/// One view as sampled: its shared geometry, the spun camera and each solid's composed pose.
pub struct ViewFrame {
    pub(super) geometry: Arc<ViewGeometry>,
    pub(super) camera: Option<Camera>,
    pub(super) poses: Vec<Transform>,
}
impl ViewFrame {
    /// The prepared solids, shared with the layout.
    pub fn geometry(&self) -> &ViewGeometry {
        &self.geometry
    }
    /// The authored camera turned by the spin; `None` when nothing is built.
    pub fn camera(&self) -> Option<Camera> {
        self.camera
    }
    /// Each solid's local-to-world pose, in `geometry().solids` order.
    pub fn poses(&self) -> &[Transform] {
        &self.poses
    }
}
impl fmt::Debug for ViewFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewFrame")
            .field("poses", &self.poses.len())
            .finish_non_exhaustive()
    }
}
