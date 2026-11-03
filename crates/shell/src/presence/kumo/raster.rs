//! Shades one frame of the body: samples the bake through the frame's squash and lift, lights
//! it, adds the inner glow and rim, then draws the eyes on top. Output is straight-alpha BGRA,
//! which is what GPUI's image pipeline blends, with clear pixels coloured like their covered
//! neighbours so GPUI's filtering of a scaled image leaves no dark fringe.
#![expect(
    clippy::many_single_char_names,
    reason = "shading math keeps its usual one-letter names"
)]

use super::frame::{Eye, FrameState};
use super::look::Look;
use super::math::{Rgb, Vec2, Vec3, hex, mix, smoothstep, v2, v3};
use super::shape::{Framing, GBuffer, Silhouette, fbm};

const WHITE: Rgb = hex(0xFF_FF_FF);
const BLUSH: Rgb = hex(0xFF_9D_B6);
const ACCENT_POS: Vec2 = v2(0.56, -0.24);
/// The renderer always frames the body with room for its shadow.
const FRAMING: Framing = Framing::CARD;

/// The output buffer didn't match the renderer's size.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the image buffer holds {got} bytes, not the {want} its size needs")]
pub(crate) struct SizeMismatch {
    got: usize,
    want: usize,
}

/// A baked body lit with one look, ready to shade frames at one square pixel size. The geometry
/// and the static lighting are baked; a frame adds only what moves.
#[derive(Clone, Debug)]
pub(crate) struct Renderer {
    look: Look,
    size: u16,
    lit: Vec<Lit>,
    floor: f32,
}

impl Renderer {
    /// Bakes the body at `size` pixels. This is the expensive step (see [`GBuffer::bake`]): run
    /// it when the size, scale or look changes, never in paint.
    pub(crate) fn new(shape: &Silhouette, size: u16, look: Look) -> Self {
        let bake = GBuffer::bake(shape, size, FRAMING);
        let mut lit = Vec::with_capacity(bake.texels.len());
        for j in 0..size {
            for i in 0..size {
                let t = bake.texels[index(size, i, j)];
                if t.coverage <= 0.0 {
                    lit.push(Lit::default());
                    continue;
                }
                let q = FRAMING.to_cloud(size, f32::from(i) + 0.5, f32::from(j) + 0.5);
                lit.push(Lit {
                    coverage: t.coverage,
                    ..surface(&look, t.normal, t.thickness, t.occlusion, q, shape.floor)
                });
            }
        }
        Self {
            look,
            size,
            lit,
            floor: shape.floor,
        }
    }

    /// The bytes one frame needs: four per pixel.
    pub(crate) fn image_len(&self) -> usize {
        usize::from(self.size) * usize::from(self.size) * 4
    }

    /// Writes one frame as straight-alpha BGRA into `out`, which must hold exactly
    /// [`Self::image_len`] bytes. Allocates nothing.
    pub(crate) fn render(&self, f: &FrameState, out: &mut [u8]) -> Result<(), SizeMismatch> {
        if out.len() != self.image_len() {
            return Err(SizeMismatch {
                got: out.len(),
                want: self.image_len(),
            });
        }
        let size = self.size;
        let upp = FRAMING.units_per_pixel(size);
        for j in 0..size {
            for i in 0..size {
                let p = FRAMING.to_cloud(size, f32::from(i) + 0.5, f32::from(j) + 0.5);
                let px = &mut out[index(size, i, j) * 4..][..4];
                px.copy_from_slice(&self.pixel(f, p, upp));
            }
        }
        bleed_clear(out, size);
        Ok(())
    }

    /// One BGRA pixel at cloud point `p`.
    fn pixel(&self, f: &FrameState, p: Vec2, upp: f32) -> [u8; 4] {
        let look = &self.look;
        // Into body space: undo the squash about the bottom centre, then the lift.
        let q = v2(
            p.x / f.body_scale.x,
            self.floor + (p.y - self.floor - f.body_lift) / f.body_scale.y,
        );
        let mut t = self.sample(FRAMING.to_pixel(self.size, q));
        if t.coverage > 0.0 && t.coverage < 1.0 {
            // The soft band drifts slowly, so the edge reads as vapour rather than vinyl.
            let n = fbm(v3(q.x * 7.0, q.y * 7.0, f.time * 0.15));
            t.coverage = (t.coverage * (1.0 + 0.35 * n * (1.0 - t.coverage))).clamp(0.0, 1.0);
        }
        let body = if t.coverage > 0.0 {
            self.body_color(f, &t, q, upp)
        } else {
            Rgb::default()
        };
        let mut shadow = 0.0;
        let d = v2(
            (p.x - 0.02) / (0.70 * f.body_scale.x),
            (p.y - (self.floor - 0.085)) / 0.07,
        );
        if look.shadow && d.y.abs() < 2.5 {
            shadow = look.material.shadow
                * (-d.dot(d) * 1.6).exp()
                * (1.0 - f.body_lift * 8.0).clamp(0.3, 1.0);
        }
        let a = t.coverage + shadow * (1.0 - t.coverage);
        if a <= 1e-4 {
            return [0; 4];
        }
        let c = look.theme.shadow.lerp(body, t.coverage / a);
        [to_u8(c.b), to_u8(c.g), to_u8(c.r), to_u8(a)]
    }

    /// The lit body at body-space point `q` with the mood's light, blush and eyes on top.
    fn body_color(&self, f: &FrameState, t: &Lit, q: Vec2, upp: f32) -> Rgb {
        let theme = self.look.theme;
        let g = (glow(q, f.glow_pos, f.glow_r) * f.glow_k * t.inside).min(1.0);
        let mut c = t.base.lerp(f.glow, g * 0.55).screen(f.glow, g * 0.35);
        let rim = f.glow.lerp(theme.accent, t.accent_mix).lerp(WHITE, 0.25);
        c = c.lerp(rim, (t.rim * f.rim_k * 0.6).min(0.85));
        for e in &f.eyes {
            let rel = q - e.center;
            if f.blush > 0.01 && rel.y < 0.0 && rel.y > -0.2 {
                let blush = f.blush * glow(q, e.center + v2(e.side * 0.075, -0.088), 0.06) * 0.55;
                c = c.lerp(BLUSH, blush);
            }
            if rel.x.abs() > 0.2 || rel.y.abs() > 0.2 {
                continue;
            }
            let (ink, shine) = eye_coverage(e, rel, upp);
            c = c.lerp(theme.eye, ink).lerp(WHITE, shine);
        }
        c
    }

    /// Bilinear sample of the baked texels at a fractional pixel; outside is empty.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "floored sample coordinates near a small image fit i32"
    )]
    fn sample(&self, p: Vec2) -> Lit {
        let (x, y) = (p.x - 0.5, p.y - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let at = |xi: i32, yi: i32| match (u16::try_from(xi), u16::try_from(yi)) {
            (Ok(xi), Ok(yi)) if xi < self.size && yi < self.size => {
                self.lit[index(self.size, xi, yi)]
            }
            _ => Lit::default(),
        };
        let [a, b, c, d] = [
            at(x0, y0),
            at(x0 + 1, y0),
            at(x0, y0 + 1),
            at(x0 + 1, y0 + 1),
        ];
        if [a, b, c, d].iter().all(|l| l.coverage <= 0.0) {
            return Lit::default();
        }
        a.lerp(b, fx).lerp(c.lerp(d, fx), fy)
    }
}

/// Gives each clear pixel of a straight-alpha BGRA square the colour of a covered 4-neighbour,
/// keeping its alpha at 0. GPUI filters straight-alpha colour when it scales an image, so a
/// black clear pixel next to a covered one would darken the edge.
pub(crate) fn bleed_clear(pixels: &mut [u8], size: u16) {
    let covered = |pixels: &[u8], i: u16, j: u16| pixels[index(size, i, j) * 4 + 3] > 0;
    for j in 0..size {
        for i in 0..size {
            if covered(pixels, i, j) {
                continue;
            }
            let neighbours = [
                i.checked_sub(1).map(|i| (i, j)),
                (i + 1 < size).then(|| (i + 1, j)),
                j.checked_sub(1).map(|j| (i, j)),
                (j + 1 < size).then(|| (i, j + 1)),
            ];
            let Some((ni, nj)) = neighbours
                .into_iter()
                .flatten()
                .find(|&(ni, nj)| covered(pixels, ni, nj))
            else {
                continue;
            };
            let from = index(size, ni, nj) * 4;
            let to = index(size, i, j) * 4;
            pixels.copy_within(from..from + 3, to);
        }
    }
}

/// The frame-independent part of a surface point: lit colour with the static accent light in,
/// how much inner light it lets through, its rim weight, and where its rim sits between the
/// mood's colour and the accent.
#[derive(Clone, Copy, Debug, Default)]
struct Lit {
    coverage: f32,
    base: Rgb,
    inside: f32,
    rim: f32,
    accent_mix: f32,
}

impl Lit {
    fn lerp(self, o: Self, t: f32) -> Self {
        Self {
            coverage: mix(self.coverage, o.coverage, t),
            base: self.base.lerp(o.base, t),
            inside: mix(self.inside, o.inside, t),
            rim: mix(self.rim, o.rim, t),
            accent_mix: mix(self.accent_mix, o.accent_mix, t),
        }
    }
}

/// The material and colour layers at one surface point: `n` is its normal and `q` its
/// position in body space.
fn surface(look: &Look, n: Vec3, thickness: f32, occlusion: f32, q: Vec2, floor: f32) -> Lit {
    let theme = look.theme;
    let m = look.material;
    let key = m.key_dir.normalized();
    let half = v3(key.x, key.y, key.z + 1.0).normalized();
    let albedo = theme
        .bottom
        .lerp(theme.top, smoothstep(floor, 0.68, q.y).powf(0.8));
    let diffuse = ((n.dot(key) + m.wrap) / (1.0 + m.wrap)).clamp(0.0, 1.0);
    let sky = 0.55 + 0.45 * (0.5 + 0.5 * n.y);
    let light = m.ambient * sky * mix(0.72, 1.0, occlusion) + m.key * diffuse;
    // Shadows go to a saturated shade colour rather than towards grey, the way lit vapour does.
    let mut c = theme
        .shade
        .lerp(albedo, smoothstep(0.25, 1.02, light))
        .screen(WHITE, (light - 1.0).max(0.0) * 1.5);
    c = c.screen(WHITE, n.dot(half).max(0.0).powf(m.sheen_power) * m.sheen);
    // Creases between lobes sink into the shade colour, the way packed vapour self-shadows.
    c = c.lerp(theme.shade, (1.0 - occlusion) * 0.55);
    // Thin vapour at the silhouette scatters the key light through it, brightest on the lit side.
    let along = (n.x * key.x + n.y * key.y) / (key.x * key.x + key.y * key.y).sqrt().max(1e-3);
    let lit_side = 0.5 + 0.5 * along;
    let silver =
        (1.0 - n.z.max(0.0)).powf(1.6) * (1.0 - thickness).powf(1.5) * lit_side * m.scatter;
    c = c.screen(theme.top.lerp(WHITE, 0.6), silver.min(1.0) * 0.85);
    // Thicker parts, and parts turned away from the key light, let more inner light through.
    let inside = (0.45 + 0.55 * thickness) * (1.0 - 0.3 * diffuse);
    let accent = glow(q, ACCENT_POS, 0.46) * 0.8 * inside;
    c = c
        .lerp(theme.accent, accent * 0.45)
        .screen(theme.accent, accent * 0.3);
    Lit {
        coverage: 1.0,
        base: c,
        inside,
        rim: (1.0 - n.z.max(0.0)).powf(2.2) * m.rim,
        accent_mix: smoothstep(-0.2, 0.8, q.x),
    }
}

fn glow(q: Vec2, at: Vec2, r: f32) -> f32 {
    let d = q - at;
    (-d.dot(d) / (r * r)).exp()
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=255.5 first"
)]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn index(size: u16, i: u16, j: u16) -> usize {
    usize::from(j) * usize::from(size) + usize::from(i)
}

/// The ink and highlight coverage of one eye at `rel`, cloud units from its centre.
fn eye_coverage(e: &Eye, rel: Vec2, upp: f32) -> (f32, f32) {
    let w_min = 0.8 * upp;
    // Mirrored so +x points outward for both eyes.
    let p = v2(rel.x * e.side, rel.y);
    let w = e.half_w.max(w_min);
    let h = e.half_h.max(w_min * 0.6);
    let r = w.min(h);
    let corner = v2(p.x.abs() - w + r, p.y.abs() - h + r);
    let pill = corner.max(0.0).len() + corner.x.max(corner.y).min(0.0) - r;
    let mut d = pill;
    if e.smile > 0.0 {
        let radius = w * 1.7;
        let thick = w * 0.62;
        let v = p - v2(0.0, -radius * 0.78);
        let aperture = 0.95_f32;
        let angle = v.x.atan2(v.y);
        let arc = if angle.abs() <= aperture {
            (v.len() - radius).abs() - thick
        } else {
            let end = v2(
                radius * aperture.sin() * angle.signum(),
                radius * aperture.cos(),
            );
            (v - end).len() - thick
        };
        d = mix(pill, arc, e.smile);
    }
    if e.lid > 0.0 || e.droop > 0.0 {
        let slope = e.droop * 0.9;
        let top = h * (1.0 - 1.6 * e.lid) - slope * p.x;
        d = d.max((p.y - top) / (1.0 + slope * slope).sqrt());
    }
    let ink = (0.5 - d / upp).clamp(0.0, 1.0);
    let mut shine = 0.0;
    if e.sparkle > 0.0 {
        let highlight = v2(p.x + w * 0.25, p.y - h * 0.42).len() - w * 0.32;
        shine = (0.5 - highlight / upp).clamp(0.0, 1.0) * ink * e.sparkle * 0.9;
    }
    (ink, shine)
}

#[cfg(test)]
mod tests {
    use super::{FRAMING, FrameState, Look, Renderer, SizeMismatch, bleed_clear, index};
    use crate::presence::kumo::shape::Silhouette;

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a point inside a small test image"
    )]
    fn pixel(v: f32) -> u16 {
        v as u16
    }

    fn rendered(size: u16) -> Vec<u8> {
        let renderer = Renderer::new(&Silhouette::emoji(), size, Look::default());
        let mut out = vec![0; renderer.image_len()];
        renderer
            .render(&FrameState::IDLE, &mut out)
            .expect("the buffer is the renderer's size");
        out
    }

    #[test]
    fn a_wrong_buffer_is_an_error_not_a_panic() {
        let renderer = Renderer::new(&Silhouette::emoji(), 8, Look::default());
        let mut short = vec![0; 8 * 8 * 4 - 1];
        assert_eq!(
            renderer.render(&FrameState::IDLE, &mut short),
            Err(SizeMismatch {
                got: 255,
                want: 256
            })
        );
        assert_eq!(
            renderer.render(&FrameState::IDLE, &mut []),
            Err(SizeMismatch { got: 0, want: 256 })
        );
    }

    #[test]
    fn the_still_pose_renders_the_same_bytes_twice() {
        assert_eq!(rendered(32), rendered(32));
    }

    #[test]
    fn corners_are_clear_and_the_middle_is_opaque() {
        let out = rendered(48);
        for (i, j) in [(0, 0), (47, 0)] {
            assert_eq!(&out[index(48, i, j) * 4..][..4], &[0; 4], "corner {i},{j}");
        }
        assert_eq!(out[index(48, 24, 24) * 4 + 3], 255, "the middle is opaque");
    }

    #[test]
    fn the_eyes_are_inked_in_the_eye_colour() {
        let size = 84;
        let out = rendered(size);
        let eye = FrameState::IDLE.eyes[0].center;
        let at = FRAMING.to_pixel(size, eye);
        let px = &out[index(size, pixel(at.x), pixel(at.y)) * 4..][..4];
        // The theme's eye ink is 0x353A5E: BGR 5E 3A 35.
        assert_eq!(px[3], 255, "the eye sits on opaque body");
        for (got, want) in px[..3].iter().zip([0x5E_u8, 0x3A, 0x35]) {
            assert!(got.abs_diff(want) <= 8, "eye pixel {px:?}");
        }
    }

    #[test]
    fn clear_pixels_take_a_covered_neighbours_colour() {
        let mut px = vec![0; 3 * 3 * 4];
        px[index(3, 1, 1) * 4..][..4].copy_from_slice(&[10, 20, 30, 255]);
        bleed_clear(&mut px, 3);
        for (i, j) in [(0, 1), (2, 1), (1, 0), (1, 2)] {
            assert_eq!(&px[index(3, i, j) * 4..][..4], &[10, 20, 30, 0], "{i},{j}");
        }
        for (i, j) in [(0, 0), (2, 2)] {
            assert_eq!(&px[index(3, i, j) * 4..][..4], &[0; 4], "diagonal {i},{j}");
        }
    }

    #[test]
    fn a_rendered_edge_has_no_black_clear_pixel_beside_the_body() {
        let size = 48;
        let out = rendered(size);
        for j in 0..size {
            for i in 1..size {
                let (left, here) = (index(size, i - 1, j) * 4, index(size, i, j) * 4);
                if out[here + 3] == 0 && out[left + 3] > 0 {
                    assert_ne!(&out[here..here + 3], &[0, 0, 0], "a dark fringe at {i},{j}");
                }
            }
        }
    }
}
