// One pass: each record's range of its geometry, posed in the vertex stage and painted per
// sample in the fragment stage, so a reveal's or a dash's end is antialiased like an edge.

override ON: f32;
override PERIOD: f32;

const DASHED: u32 = 1u;
const REVEALED: u32 = 2u;
const RETRACE: u32 = 4u;

// pose: a, b, c, d to clip space; place: e, f, opacity, draw; dash: phase, override, 0, 0;
// over: the override's red, green, blue and opacity.
struct Record {
    pose: vec4<f32>,
    place: vec4<f32>,
    dash: vec4<f32>,
    over: vec4<f32>,
}

struct Piece {
    color: vec4<f32>,
    length: f32,
    flags: u32,
    pad: vec2<u32>,
}

@group(0) @binding(0) var<storage, read> records: array<Record>;
@group(0) @binding(1) var<storage, read> pieces: array<Piece>;

struct Corner {
    @location(0) at: vec2<f32>,
    @location(1) along: f32,
    @location(2) skip: f32,
    @location(3) reveal: f32,
    @location(4) weight: f32,
    @location(5) piece: u32,
}

struct Sample {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(linear, sample) along: f32,
    @location(1) @interpolate(linear, sample) reveal: f32,
    @location(2) @interpolate(linear, sample) weight: f32,
    @location(3) @interpolate(flat, first) skip: f32,
    @location(4) @interpolate(flat, first) piece: u32,
    @location(5) @interpolate(flat, first) record: u32,
}

@vertex
fn vertex(corner: Corner, @builtin(instance_index) record: u32) -> Sample {
    let r = records[record];
    let at = vec2(
        r.pose.x * corner.at.x + r.pose.z * corner.at.y + r.place.x,
        r.pose.y * corner.at.x + r.pose.w * corner.at.y + r.place.y,
    );
    return Sample(
        vec4(at, 0.0, 1.0),
        corner.along,
        corner.reveal,
        corner.weight,
        corner.skip,
        corner.piece,
        record,
    );
}

@fragment
fn fragment(s: Sample) -> @location(0) vec4<f32> {
    let r = records[s.record];
    let piece = pieces[s.piece];
    let draw = r.place.w;
    let distance = s.along + s.skip;
    let dashed = (piece.flags & DASHED) != 0u;
    // A full reveal draws whole and an empty one nothing, without comparing.
    if (piece.flags & REVEALED) != 0u {
        if draw <= 0.0 {
            discard;
        }
        if draw < 1.0 {
            let stroke = piece.length > 0.0;
            if select(s.reveal > draw * s.weight, distance > draw * piece.length, stroke) {
                discard;
            }
        }
    }
    if dashed {
        let at = distance + r.dash.x;
        if at - PERIOD * floor(at / PERIOD) >= ON {
            discard;
        }
    }
    if (piece.flags & RETRACE) != 0u && draw >= 1.0 && !dashed {
        discard;
    }
    var color = piece.color;
    if r.dash.y > 0.0 {
        color = vec4(r.over.xyz, r.over.w * piece.color.w);
    }
    let alpha = clamp(color.w * r.place.z, 0.0, 1.0);
    return vec4(color.xyz * alpha, alpha);
}
