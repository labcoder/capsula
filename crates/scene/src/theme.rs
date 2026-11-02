//! Palette resolution shared by preparation and the renderer.
use crate::record::{Color, Rgb, Token};

/// The host-selected scene palette and default text size.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Main foreground.
    pub fg: Rgb,
    /// Secondary foreground.
    pub muted: Rgb,
    /// Background.
    pub surface: Rgb,
    /// Raised background.
    pub surface_raised: Rgb,
    /// Default line color.
    pub line: Rgb,
    /// Host accent.
    pub accent: Rgb,
    /// Callout backdrop.
    pub halo: Rgb,
    /// Default font size in logical pixels.
    pub font_size: f32,
}
fn rgb(value: u32) -> Rgb {
    value.try_into().expect("palette entries are 24-bit RGB")
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: rgb(0x00f3_f1f8),
            muted: rgb(0x00a9_a4b8),
            surface: rgb(0x001d_1b26),
            surface_raised: rgb(0x002a_2735),
            line: rgb(0x008d_88a0),
            accent: rgb(0x007a_a2ff),
            halo: rgb(0x000d_0c12),
            font_size: 15.0,
        }
    }
}
impl Theme {
    /// Resolves a typed color without parsing strings or allocating.
    pub fn resolve(&self, color: &Color) -> Rgb {
        let token = match color {
            Color::Hex(value) => return *value,
            Color::Token(token) => token,
        };
        match token {
            Token::Fg => self.fg,
            Token::White => rgb(0x00f3_f1f8),
            Token::Muted => self.muted,
            Token::Accent => self.accent,
            Token::Surface => self.surface_raised,
            Token::Blue => rgb(0x007a_a2ff),
            Token::Green => rgb(0x005f_d39a),
            Token::Red => rgb(0x00ff_6b7d),
            Token::Orange => rgb(0x00ff_9a5c),
            Token::Amber => rgb(0x00f5_c15c),
            Token::Purple => rgb(0x00b2_8cff),
            Token::Teal => rgb(0x004f_d1d9),
            Token::Pink => rgb(0x00f3_8bc8),
            Token::Gray => rgb(0x008d_88a0),
            Token::Black => rgb(0x0015_141b),
        }
    }
    /// Deterministic categorical color, cycling after the eight series colors.
    pub fn series(&self, index: usize) -> Rgb {
        const COLORS: [u32; 8] = [
            0x007a_a2ff,
            0x005f_d39a,
            0x00f5_c15c,
            0x00ff_6b7d,
            0x00b2_8cff,
            0x004f_d1d9,
            0x00ff_9a5c,
            0x00f3_8bc8,
        ];
        rgb(COLORS[index % COLORS.len()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_colors_keep_the_spike_palette_and_host_overrides() {
        let theme = Theme {
            accent: rgb(0x0012_3456),
            ..Theme::default()
        };
        assert_eq!(
            theme.resolve(&Color::Token(Token::Accent)).get(),
            0x0012_3456
        );
        assert_eq!(theme.resolve(&Color::Token(Token::Blue)).get(), 0x007a_a2ff);
        assert_eq!(
            theme.resolve(&Color::Hex(rgb(0x00ab_cdef))).get(),
            0x00ab_cdef
        );
        assert_eq!(theme.series(9).get(), 0x005f_d39a);
    }
}
