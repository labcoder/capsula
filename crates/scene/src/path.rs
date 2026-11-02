//! Bounded SVG-subset preflight shared with path preparation.
use crate::{
    Budget, ErrorCode,
    limits::{MAX_COORD, MAX_PATH_POINTS, MAX_PATH_SEGMENTS},
};
use std::sync::Arc;

/// Logical-pixel flatness used for deterministic curve subdivision.
pub(crate) const TOLERANCE: f64 = 0.25;
pub(crate) const PROCEDURAL_CURVE_PIECES: usize = 16;
pub(crate) const ROUND_RECT_SEGMENTS: usize = 29;
pub(crate) const ELLIPSE_SEGMENTS: usize = 48;
pub(crate) const CYLINDER_CAP_SEGMENTS: usize = 32;
pub(crate) const PIE_ARC_SEGMENTS: usize = 120;
pub(crate) const CHART_AXES_SEGMENTS: usize = 64;

#[derive(Debug, Clone, Default)]
pub(crate) struct PathWork {
    pub lines: usize,
    pub curvature: Arc<[f32]>,
    pub extent: f32,
}
impl PathWork {
    pub(crate) fn segments(&self, scale: f32) -> Result<usize, ErrorCode> {
        let mut count = self.lines;
        for curvature in &*self.curvature {
            count += usize::from(subdivisions(*curvature, scale)?);
            if count > MAX_PATH_SEGMENTS {
                return Err(ErrorCode::OverBudget(Budget::PathSegments));
            }
        }
        Ok(count)
    }
}

pub(crate) fn subdivisions(curvature: f32, scale: f32) -> Result<u16, ErrorCode> {
    let deviation = f64::from(curvature) * f64::from(scale);
    let mut pieces = 1_u16;
    while deviation > TOLERANCE * f64::from(pieces).powi(2) {
        if usize::from(pieces) >= MAX_PATH_SEGMENTS {
            return Err(ErrorCode::OverBudget(Budget::PathSegments));
        }
        pieces *= 2;
    }
    Ok(pieces)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Segment {
    Move([f32; 2]),
    Line([f32; 2]),
    Quad([[f32; 2]; 2]),
    Cubic([[f32; 2]; 3]),
    Close,
}

pub(crate) fn walk(
    source: &str,
    mut visit: impl FnMut([f32; 2], Segment) -> Result<(), ErrorCode>,
) -> Result<PathWork, ErrorCode> {
    let mut tokens = Tokens {
        source: source.as_bytes(),
        position: 0,
    };
    let mut pen = [0.0; 2];
    let mut start = pen;
    let mut previous = None;
    let mut commands = 0;
    let mut lines = 0;
    let mut curvature = Vec::new();
    let mut extent = 0.0_f32;
    while let Some(byte) = tokens.peek() {
        let command = if byte.is_ascii_alphabetic() {
            tokens.position += 1;
            byte
        } else {
            match previous {
                Some(b'M') => b'L',
                Some(b'm') => b'l',
                Some(b'Z' | b'z') | None => return Err(ErrorCode::Malformed),
                Some(command) => command,
            }
        };
        if previous.is_none() && !matches!(command, b'M' | b'm') {
            return Err(ErrorCode::Malformed);
        }
        commands += 1;
        if commands > MAX_PATH_POINTS {
            return Err(ErrorCode::OverBudget(Budget::PathPoints));
        }
        let segment = tokens.segment(command, pen)?;
        let original = pen;
        let mut include = |p: [f32; 2]| -> Result<(), ErrorCode> {
            if p.iter().any(|v| !v.is_finite() || v.abs() > MAX_COORD) {
                return Err(ErrorCode::OutOfRange);
            }
            extent = extent.max(p[0].abs()).max(p[1].abs());
            Ok(())
        };
        match segment {
            Segment::Move(p) => {
                include(p)?;
                start = p;
                pen = p;
            }
            Segment::Line(p) => {
                include(p)?;
                pen = p;
                lines += 1;
            }
            Segment::Quad([c, p]) => {
                include(c)?;
                include(p)?;
                curvature.push(second_difference(pen, c, p) * 0.25);
                pen = p;
            }
            Segment::Cubic([c0, c1, p]) => {
                include(c0)?;
                include(c1)?;
                include(p)?;
                curvature
                    .push(second_difference(pen, c0, c1).max(second_difference(c0, c1, p)) * 0.75);
                pen = p;
            }
            Segment::Close => {
                pen = start;
                lines += 1;
            }
        }
        visit(original, segment)?;
        previous = Some(command);
    }
    if commands == 0 {
        return Err(ErrorCode::Malformed);
    }
    Ok(PathWork {
        lines,
        curvature: curvature.into(),
        extent,
    })
}

pub(crate) fn second_difference(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (a[0] - 2.0 * b[0] + c[0]).hypot(a[1] - 2.0 * b[1] + c[1])
}

struct Tokens<'a> {
    source: &'a [u8],
    position: usize,
}
impl Tokens<'_> {
    fn segment(&mut self, command: u8, pen: [f32; 2]) -> Result<Segment, ErrorCode> {
        let base = if command.is_ascii_lowercase() {
            pen
        } else {
            [0.0; 2]
        };
        let point = |tokens: &mut Tokens<'_>| -> Result<[f32; 2], ErrorCode> {
            let p = [base[0] + tokens.number()?, base[1] + tokens.number()?];
            if p.iter().any(|v| !v.is_finite() || v.abs() > MAX_COORD) {
                return Err(ErrorCode::OutOfRange);
            }
            Ok(p)
        };
        Ok(match command.to_ascii_uppercase() {
            b'M' => Segment::Move(point(self)?),
            b'L' => Segment::Line(point(self)?),
            b'H' => Segment::Line([base[0] + self.number()?, pen[1]]),
            b'V' => Segment::Line([pen[0], base[1] + self.number()?]),
            b'Q' => Segment::Quad([point(self)?, point(self)?]),
            b'C' => Segment::Cubic([point(self)?, point(self)?, point(self)?]),
            b'Z' => Segment::Close,
            _ => return Err(ErrorCode::Unsupported),
        })
    }

    fn peek(&mut self) -> Option<u8> {
        while self
            .source
            .get(self.position)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b',')
        {
            self.position += 1;
        }
        self.source.get(self.position).copied()
    }
    fn number(&mut self) -> Result<f32, ErrorCode> {
        self.peek();
        let start = self.position;
        let mut decimal = false;
        let mut exponent = false;
        while let Some(&byte) = self.source.get(self.position) {
            let sign =
                self.position == start || matches!(self.source[self.position - 1], b'e' | b'E');
            match byte {
                b'.' if !decimal && !exponent => decimal = true,
                b'e' | b'E' if !exponent => exponent = true,
                b'+' | b'-' if sign => {}
                b'0'..=b'9' => {}
                _ => break,
            }
            self.position += 1;
        }
        let value = std::str::from_utf8(&self.source[start..self.position])
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .ok_or(ErrorCode::Malformed)?;
        if !value.is_finite() || value.abs() > MAX_COORD {
            return Err(ErrorCode::OutOfRange);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spike_subset_with_relative_and_implicit_commands() {
        let mut segments = Vec::new();
        let work = walk("M0 0 L10 0 10 10 Z m20 0 h5 v5 q5 5 10 0", |_, segment| {
            segments.push(segment);
            Ok(())
        })
        .expect("valid subset");
        assert_eq!(segments.len(), 8);
        assert_eq!(
            segments.last(),
            Some(&Segment::Quad([[30.0, 10.0], [35.0, 5.0]]))
        );
        assert_eq!(work.segments(1.0).expect("bounded"), 9);
        let mut compact = Vec::new();
        walk("M.5.5 L1.5-2.5", |_, segment| {
            compact.push(segment);
            Ok(())
        })
        .expect("compact SVG numbers");
        assert_eq!(
            compact,
            vec![Segment::Move([0.5, 0.5]), Segment::Line([1.5, -2.5])]
        );
    }

    #[test]
    fn command_and_coordinate_errors_are_typed_and_terminal() {
        for (source, error) in [
            ("M0 0 A5 5 0 0 1 10 10", ErrorCode::Unsupported),
            ("M0 0 L1e40 0", ErrorCode::OutOfRange),
            ("M20000 0 l1 0", ErrorCode::OutOfRange),
            ("M0 0 Lx", ErrorCode::Malformed),
            ("L1 0", ErrorCode::Malformed),
            ("", ErrorCode::Malformed),
        ] {
            assert_eq!(
                walk(source, |_, _| Ok(())).expect_err("invalid subset"),
                error
            );
        }
    }

    #[test]
    fn curve_bounds_scale_without_building_flattened_vertices() {
        let quadratic = walk("M0 0 Q0 4 4 4", |_, _| Ok(())).expect("quadratic");
        assert_eq!(quadratic.segments(1.0).expect("bounded"), 4);
        assert_eq!(quadratic.segments(4.0).expect("bounded"), 8);
        let cubic = walk("M0 0 C0 4 4 4 4 0", |_, _| Ok(())).expect("cubic");
        assert_eq!(cubic.segments(1.0).expect("bounded"), 8);
        assert_eq!(cubic.segments(4.0).expect("bounded"), 16);
    }
}
