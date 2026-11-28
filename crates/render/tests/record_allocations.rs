//! Writing a frame's records is per-frame work: once warm, it allocates and frees nothing.

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
use kumochi_render::{FrameBuf, Stager, record};
use kumochi_scene::GlobalBudget;
use kumochi_shell_core::presenter::Stage;

#[test]
fn writing_four_views_records_allocates_and_frees_nothing() {
    let mut budget = GlobalBudget::default();
    let stager = Stager::new();
    let views: Vec<_> = [600, 600, 150, 150]
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
