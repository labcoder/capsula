//! Product palette overrides retained across global appearance changes.

use std::rc::Rc;

use gpui_kit::App;
use gpui_kit::component::Theme;

/// Installs the shell palette after `gpui_kit::init`, preserving the current mode.
pub(crate) fn init(cx: &mut App) {
    Theme::update(cx, |theme| {
        let light = &mut Rc::make_mut(&mut theme.light_theme).colors;
        light.danger = Some("#B91C1C".into());
        light.button_danger = Some("#B91C1C".into());
        light.button_danger_foreground = Some("#FFFFFF".into());
        light.button_danger_hover = Some("#991B1B".into());
        light.button_danger_active = Some("#7F1D1D".into());
        light.link = Some("#1D4ED8".into());
        light.link_hover = Some("#1E40AF".into());
        light.link_active = Some("#1E3A8A".into());

        let dark = &mut Rc::make_mut(&mut theme.dark_theme).colors;
        dark.link = Some("#93C5FD".into());
        dark.link_hover = Some("#BFDBFE".into());
        dark.link_active = Some("#DBEAFE".into());
    });
    Theme::change(Theme::global(cx).mode, None, cx);
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::theme::ThemeMode;
    use gpui_kit::{TestAppContext, rgb};

    use super::*;

    #[gpui::test]
    fn palette_survives_global_mode_changes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            let dark = Theme::global(cx).colors;
            init(cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
            for _ in 0..2 {
                Theme::change(ThemeMode::Light, None, cx);
                let theme = Theme::global(cx);
                assert_eq!(theme.danger, rgb(0xB9_1C_1C).into());
                assert_eq!(theme.button_danger, rgb(0xB9_1C_1C).into());
                assert_eq!(theme.button_danger_foreground, rgb(0xFF_FF_FF).into());
                assert_eq!(theme.button_danger_hover, rgb(0x99_1B_1B).into());
                assert_eq!(theme.button_danger_active, rgb(0x7F_1D_1D).into());
                assert_eq!(theme.link, rgb(0x1D_4E_D8).into());
                assert_eq!(
                    theme.tokens.button_danger.background,
                    gpui_kit::Background::from(rgb(0xB9_1C_1C))
                );
                Theme::change(ThemeMode::Dark, None, cx);
                let theme = Theme::global(cx);
                assert_eq!(theme.danger, dark.danger);
                assert_eq!(theme.button_danger, dark.button_danger);
                assert_eq!(
                    theme.button_danger_foreground,
                    dark.button_danger_foreground
                );
                assert_eq!(theme.button_danger_hover, dark.button_danger_hover);
                assert_eq!(theme.button_danger_active, dark.button_danger_active);
                assert_eq!(theme.link, rgb(0x93_C5_FD).into());
            }
        });
    }
}
