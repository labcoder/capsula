//! Drawing a frame's pictures into a target: the records written into the record buffer, then
//! one indexed draw per record into a 4x multisampled image, resolved into the target.

use std::{fmt, sync::atomic::Ordering};

use super::{Built, FORMAT, Gpu, GpuFailure, SAMPLES, Target, bytes, channels, scoped};
use crate::limits::DASH;
use crate::stage::{FrameBuf, Picture, Record};

/// Words per encoded record: four rows of four.
const RECORD_WORDS: usize = 16;
/// Bytes per encoded record.
pub(super) const RECORD_BYTES: usize = RECORD_WORDS * 4;
/// The vertex stride: `VERTEX_WORDS` of four bytes.
const VERTEX_STRIDE: u64 = 28;

/// Where a frame's world lands in its target: a target pixel is `(world − origin) × scale`.
#[derive(Clone, Copy, PartialEq)]
pub struct View {
    /// The world point at the target's top left.
    pub origin: [f32; 2],
    /// Target pixels per world unit.
    pub scale: f32,
}

/// Where the drawing is looked at from is the drawing's: nothing.
impl fmt::Debug for View {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("View(..)")
    }
}

/// What a draw issued for the pictures it was given: counts of commands, not of ink. It is
/// provisional, as a [`Built`] is: its commands were submitted with no error and no loss seen,
/// not confirmed on the GPU, which may still be drawing when the answer arrives. wgpu reports
/// nothing when a device is lost under a submission, so such a loss shows only once it is
/// latched, when every later call answers `Lost`. Nothing is presented.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drawn {
    /// Pictures for which at least one indexed draw was issued: a record with a range, a
    /// positive finite opacity and a finite reveal. That proves no visible ink: a stroke-only
    /// picture at a reveal of 0 issues a draw and covers nothing.
    pub drawn: u32,
    /// Pictures whose geometry was given built but that issued no draw: text only, empty
    /// ranges, or opacity 0 or not finite.
    pub empty: u32,
    /// Pictures whose geometry wasn't given built.
    pub unbuilt: u32,
}

impl Gpu {
    /// Draws picture `picture` of `frame`, or every picture when none, into `target`: it is
    /// cleared, then each record draws its range in its pose through `view`, from the one of
    /// `built` of its picture's geometry. Records are read in place, by their place in the
    /// frame.
    ///
    /// # Errors
    /// [`GpuFailure::Mismatch`], before any GPU command, for a target or geometry of another
    /// device, a geometry none of the pictures shows, a picture the frame doesn't list or a
    /// record outside its geometry; [`GpuFailure::Refused`] when the GPU refuses the records'
    /// write or the submission; [`GpuFailure::Lost`] when the device is lost, before work, when
    /// the records' write is refused with no error to say why, or once it is submitted. Only a
    /// submission that passes is counted to retire what it reads.
    pub fn draw(
        &mut self,
        target: &Target,
        frame: &FrameBuf,
        picture: Option<usize>,
        built: &[&Built],
        view: View,
    ) -> Result<Drawn, GpuFailure> {
        self.live()?;
        let pictures = match picture {
            Some(at) => frame.pictures().get(at..=at).ok_or(GpuFailure::Mismatch)?,
            None => frame.pictures(),
        };
        let of = |picture: &Picture| {
            built
                .iter()
                .find(|built| built.geometry == picture.geometry)
        };
        let foreign = target.device != self.id
            || built.iter().any(|built| {
                built.device != self.id || !pictures.iter().any(|p| p.geometry == built.geometry)
            });
        let outside = pictures.iter().any(|picture| {
            of(picture).is_some_and(|built| {
                frame
                    .records(picture)
                    .iter()
                    .any(|record| built.range(record).is_none())
            })
        });
        if foreign || outside {
            return Err(GpuFailure::Mismatch);
        }
        let (first, records) = frame.span(pictures);
        self.stage_records(first, records, view, target.size())?;
        let done = self.submitted + 1;
        let mut drawn = Drawn::default();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.samples,
                    depth_slice: None,
                    resolve_target: Some(&target.view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            for picture in pictures {
                let Some(built) = of(picture) else {
                    drawn.unbuilt += 1;
                    continue;
                };
                built.held.last.set(done);
                pass.set_bind_group(0, &built.bind, &[]);
                pass.set_vertex_buffer(0, built.vertices.slice(..));
                pass.set_index_buffer(built.indices.slice(..), wgpu::IndexFormat::Uint32);
                let mut any = false;
                for (at, record) in (picture.first..).zip(frame.records(picture)) {
                    let range = built.range(record).unwrap_or_default();
                    let shown = record.opacity.is_finite()
                        && record.opacity > 0.0
                        && record.draw.is_finite();
                    if shown && !range.is_empty() {
                        pass.draw_indexed(range, 0, at..at + 1);
                        any = true;
                    }
                }
                if any {
                    drawn.drawn += 1;
                } else {
                    drawn.empty += 1;
                }
            }
        }
        target.held.last.set(done);
        self.submit(Some(encoder))?;
        Ok(drawn)
    }

    /// Stages `records` for their slots from `first` on, as an upload stages its buffers.
    fn stage_records(
        &self,
        first: u32,
        records: &[Record],
        view: View,
        image: [u32; 2],
    ) -> Result<(), GpuFailure> {
        if records.is_empty() {
            return Ok(());
        }
        let size = bytes(records.len(), RECORD_BYTES)?;
        let offset = u64::from(first) * bytes(1, RECORD_BYTES)?;
        let staged = scoped(&self.device, || {
            let mut staging = self.queue.write_buffer_with(
                &self.records,
                offset,
                wgpu::BufferSize::new(size)?,
            )?;
            let (slots, _) = staging.slice(..).into_chunks::<4>();
            slots.write_iter(
                records
                    .iter()
                    .flat_map(|record| record_words(record, view, image))
                    .map(u32::to_le_bytes),
            );
            Some(())
        });
        match staged {
            Ok(Some(())) => Ok(()),
            Ok(None) => {
                // As in an upload: a write refused with both scopes clean is a loss.
                self.lost.store(true, Ordering::Release);
                Err(GpuFailure::Lost)
            }
            Err(failure) => Err(self.live().err().unwrap_or(failure)),
        }
    }
}

/// A record as the shader reads it: its pose taken to clip space through `view` and the
/// target's size, the origin subtracted once and then scaled; opacity, reveal and dash phase;
/// the colour override and whether there is one.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "image sides stay within the device's texture limit, far under 2^24"
)]
fn record_words(record: &Record, view: View, [width, height]: [u32; 2]) -> [u32; RECORD_WORDS] {
    let [xx, xy, yx, yy, ex, ey] = record.transform;
    let [sx, sy] = [width as f32, height as f32].map(|side| 2.0 * view.scale / side);
    let [ox, oy] = view.origin;
    let period = DASH[0] + DASH[1];
    let phase = (1.0 - record.flow.rem_euclid(1.0)) * period;
    let (over, rgb, opacity) = record
        .color
        .map_or((0.0, 0, 1.0), |color| (1.0, color.rgb, color.opacity));
    let [red, green, blue] = channels(rgb);
    [
        sx * xx,
        -sy * xy,
        sx * yx,
        -sy * yy,
        sx * (ex - ox) - 1.0,
        1.0 - sy * (ey - oy),
        record.opacity,
        record.draw,
        phase,
        over,
        0.0,
        0.0,
        red,
        green,
        blue,
        opacity,
    ]
    .map(f32::to_bits)
}

/// The record buffer, its bind group layout and the pipeline every draw uses.
pub(super) fn pipeline(
    device: &wgpu::Device,
    records_bytes: u64,
) -> (wgpu::Buffer, wgpu::BindGroupLayout, wgpu::RenderPipeline) {
    let records = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: records_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let storage = |binding, visibility| wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            storage(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
            storage(1, wgpu::ShaderStages::FRAGMENT),
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let constants = [
        ("ON", f64::from(DASH[0])),
        ("PERIOD", f64::from(DASH[0] + DASH[1])),
    ];
    let options = || wgpu::PipelineCompilationOptions {
        constants: &constants,
        zero_initialize_workgroup_memory: false,
    };
    let attributes = wgpu::vertex_attr_array![
        0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Float32, 5 => Uint32
    ];
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            compilation_options: options(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: VERTEX_STRIDE,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &attributes,
            }],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: SAMPLES,
            ..Default::default()
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fragment"),
            compilation_options: options(),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    (records, layout, pipeline)
}

/// A geometry's bind group: the shared records and its own pieces.
pub(super) fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    records: &wgpu::Buffer,
    pieces: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: records.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: pieces.as_entire_binding(),
            },
        ],
    })
}
