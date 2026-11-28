//! Writing a frame's records is per-frame work: once warm, it allocates and frees nothing. What
//! lyon's tessellators keep between builds is measured against their allowance.

#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared test allocator includes additional instrumentation"
)]
mod allocation;
#[path = "../src/tests/fixture.rs"]
#[expect(
    dead_code,
    reason = "the shared fixtures also build views this test doesn't draw"
)]
mod fixture;

use kumochi_protocol::message::SceneId;
use kumochi_render::{
    BuildInput, Cursor, FrameBuf, Mesh, RenderCache, Stager, limits::TESSELLATOR_BYTES, record,
};
use kumochi_scene::GlobalBudget;
use kumochi_shell_core::presenter::{SceneCache, Stage};

#[test]
fn writing_four_views_records_allocates_and_frees_nothing() {
    let mut budget = GlobalBudget::default();
    let stager = Stager::new();
    let views: Vec<_> = [500, 500, 250, 250]
        .into_iter()
        .enumerate()
        .map(|(n, count)| fixture::view(&format!("v{n}"), &fixture::squares(count), &mut budget))
        .collect();
    let mut caches: Vec<_> = views.iter().map(|view| stager.stage(view)).collect();
    let frames: Vec<_> = views.iter().map(fixture::sample).collect();
    let mut buf = FrameBuf::new();
    let mut write = |buf: &mut FrameBuf| {
        buf.clear();
        for (scene, (frame, cache)) in (0..).zip(frames.iter().zip(&mut caches)) {
            assert_eq!(record(buf, SceneId(scene), frame, &mut **cache), Ok(true));
        }
    };
    write(&mut buf);
    let ((), activity) = allocation::activity(|| {
        for _ in 0..16 {
            write(&mut buf);
        }
    });
    assert_eq!(activity, [0, 0], "no allocation and no free");
    assert_eq!(buf.pictures().len(), 4, "every view was written");
}

#[test]
fn what_lyon_keeps_after_builds_at_the_fill_cap_stays_within_its_allowance() {
    let fills = fixture::view("fills", &[fixture::fills()], &mut GlobalBudget::default());
    let limits = fixture::view(
        "limits",
        &fixture::at_limits(),
        &mut GlobalBudget::default(),
    );
    let stager = Stager::new();
    let caches = [stager.stage(&fills), stager.stage(&limits)];
    let mut input = BuildInput::new();
    let mut mesh = Mesh::new();
    let ((), _, [allocated, freed]) = allocation::usage(|| {
        for _ in 0..4 {
            for cache in &caches {
                let cache = render(&**cache);
                input.wipe();
                assert_eq!(input.add(cache, Cursor::START), Ok(None));
                mesh.begin(cache.id());
                mesh.extend(&input).expect("it builds");
                mesh.finish().expect("in one batch");
            }
        }
    });
    let kept = allocated - freed;
    assert!(
        kept <= TESSELLATOR_BYTES,
        "lyon kept {kept} bytes past its allowance of {TESSELLATOR_BYTES}"
    );
}

fn render(cache: &dyn SceneCache) -> &RenderCache {
    let cache: &dyn std::any::Any = cache;
    cache.downcast_ref().expect("a render cache")
}
