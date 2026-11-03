# The GPUI Windows patch

Kumochi carries GPUI's Windows backend, `gpui-pre-windows` 0.3.10, in `vendor/gpui-pre-windows/`
with one change: an idle window no longer wakes on every vblank. `[patch.crates-io]` in
`Cargo.toml` points the crate at the vendored copy, and `cargo xtask check-vendor` (run by CI)
fails unless Cargo resolves the crate to that directory at the vendored version. Upstream tracking: zed-industries/zed#66221.
The core crate, `gpui-pre`, is not vendored.

## Why

Unpatched, the backend's vsync thread (`platform.rs` `begin_vsync_thread`) waits on
`DwmFlush` and then calls `RedrawWindow` on every GPUI window, every vblank, whether or not the
window has anything to draw. Each `WM_PAINT` reaches `draw_window` and GPUI's
`on_request_frame`, which finds nothing dirty and returns. On a 144 Hz display that's 144 wakes
a second per window, and a settled Kumochi (dock, card, overlay) spent about 2.5% of one core
doing nothing; a minimized window does it too.

## What the patch does

`vsync.rs` gains a per-window `FrameRequest`: a "wants a frame" flag and an input-sustain
deadline. The vsync thread invalidates only windows whose request is set, and sleeps on an event
when no window wants a frame, so a settled app makes no vsync wakes at all.

A window asks for a frame when:

- GPUI schedules one (`schedule_frame`, called by the core after every update that leaves a
  window dirty, which also retries a frame skipped because the window was already drawing);
- a forced render is pending (after a device-lost recovery, or one GPUI deferred because another
  draw was on the stack or the window was throttled): the window asks again every vblank until
  GPUI attempts a forced draw, so repeated deferrals can't strand it;
- GPUI receives any input for it, by any route (window messages, accelerators, DirectManipulation,
  drag and drop): the window then keeps drawing every vblank for one second (`INPUT_SUSTAIN`);
- DirectManipulation is active (touchpad pan and inertia).

The vsync thread also checks for a lost GPU device, and it now does so only when some window
wants a frame; a device lost while everything is idle is noticed on the next frame instead.

Two of these stand in for core behaviour that relies on being woken every vblank: GPUI retries
a re-entrant skip, and keeps presenting for a second after input that changed something. The
backend can't see either, so it errs on the side of drawing: it retries forced frames until GPUI
tries one, and sustains after every input, changed or not. That keeps the fix in the backend, so
`gpui-pre` stays a registry crate.
Each modified file says so in its first line, as Apache-2.0 4(b) asks; the license text is
`vendor/gpui-pre-windows/LICENSE-APACHE` and `THIRD-PARTY-NOTICES.md` lists the crate.

## Measured

Rez, 144 Hz, release builds of main 5205b0c ("before") and this branch ("after"), product shell
via `cargo xtask bench`, 20 s runs, one `measuring.lock` hold.

Shell process, calibrated cycles as a share of one core:

| Workload | Before | After |
|---|---|---|
| `idle`, run 1 | 2.37% | 0.058% |
| `idle`, run 2 | 2.73% | 0.057% |
| `idle --panel` | 2.50% | 0.057% |
| `stretch` (animating) | 9.59% | 9.63% |

During `stretch` the fixture counted 8643 ticks and 7140 shape changes before, 8641 and 7183
after, so the animation draws as often as it did. A spike app measured presents with PresentMon:
2876 vs 2875 presents at 144 Hz and 1436 vs 1437 at a 72 fps cap, with the same 6.94 ms and
13.89 ms median intervals.

Real-input checks (hover, drag, typing, touchpad pan and inertia) and a device-lost check are
done by hand at the desk (OSC-132); none of them is automated.

## Re-vendoring on a GPUI bump

1. Check whether upstream fixed #66221. If it did, delete `vendor/gpui-pre-windows`, its
   `[patch.crates-io]` line, its `THIRD-PARTY-NOTICES.md` entry, `xtask/src/vendor.rs` with
   its `check-vendor` task and CI step, and `exclude = ["vendor"]`; let Cargo update the lock,
   and stop.
2. Bump `gpui-pre`/`gpui-pre-windows` (and gpui-kit's rev) as usual; `check-vendor` now fails.
3. Replace `vendor/gpui-pre-windows` with the new release's `.crate` contents, unchanged, minus
   `.cargo-ok`, and commit that alone; `diff -r` it against the registry source first.
4. Re-apply the patch from the previous release's patch commit (`git log -- vendor/`), fixing
   conflicts by hand. Check that `begin_vsync_thread`, `draw_window`, `schedule_frame` and
   `handle_msg` still have the shapes the patch assumes, and that the core still calls
   `schedule_frame` after updates and still has its input-rate sustain.
5. `cargo build --locked`, `cargo xtask check-vendor`, `cargo test --workspace`.
6. Re-measure `cargo xtask bench idle` and `bench stretch` against the previous build, and
   repeat the OSC-132 hand checks.
