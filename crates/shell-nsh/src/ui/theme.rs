//! Kumochi's theme: the theme file, the tokens it adds to `NativeShell`'s own and the look a person
//! picks. `NativeShell`'s own tokens (`background`, `primary`, `radius`, ...) need no name here:
//! code reads them as fields of `nsh_base::theme()`.

use nsh_base::{Theme, ThemeError};
use nsh_core::element::Tokens;
use nsh_core::platform::Appearance;

/// The theme file, in shadcn's `cssVars` form: Dusk is its `dark` set and Day its `light` set.
const THEME: &str = include_str!("../../theme.json");

/// The theme file's tokens over `NativeShell`'s defaults; an error when the file isn't JSON.
pub(crate) fn tokens() -> Result<Tokens, ThemeError> {
    // What it reports leaving out is empty: the file is compiled in and a test checks that.
    Theme::from_json_lenient(THEME).map(|(theme, _left_out)| theme.into_tokens())
}

/// Declares `$name`, one variant per token, with `ALL` and each one's name in the theme file.
macro_rules! token_names {
    ($(#[$doc:meta])* $name:ident { $($variant:ident = $token:literal,)* }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum $name {
            $($variant,)*
        }

        impl $name {
            /// Every one.
            pub(crate) const ALL: &[Self] = &[$(Self::$variant),*];

            /// Its name in the theme file.
            pub(crate) const fn token(self) -> &'static str {
                match self {
                    $(Self::$variant => $token,)*
                }
            }
        }
    };
}

token_names! {
    /// A colour the theme adds to `NativeShell`'s, named for its role.
    Tone {
        SubtleForeground = "subtle-foreground",
        Bubble = "bubble",
        BubbleForeground = "bubble-foreground",
        Attention = "attention",
        AttentionForeground = "attention-foreground",
        Info = "info",
        Success = "success",
        Glass = "glass",
        Mark = "mark",
        MarkEdge = "mark-edge",
        MarkForeground = "mark-foreground",
        MarkMarker = "mark-marker",
        MarkDim = "mark-dim",
        KumoTop = "kumo-top",
        KumoMid = "kumo-mid",
        KumoFloor = "kumo-floor",
        KumoEye = "kumo-eye",
        LightListen = "light-listen",
        LightSpeak = "light-speak",
        LightAttention = "light-attention",
        LightError = "light-error",
        PointerLine = "pointer-line",
        PointerLineEdge = "pointer-line-edge",
    }
}

token_names! {
    /// A length the theme adds to `NativeShell`'s `radius`, in points, named for its role.
    Size {
        RadiusSm = "radius-sm",
        RadiusCard = "radius-card",
        RadiusPopover = "radius-popover",
        RadiusBox = "radius-box",
        RadiusBubbleTail = "radius-bubble-tail",
        RadiusPanel = "radius-panel",
        RadiusWindow = "radius-window",
        BarWidth = "size-bar-width",
        BarHeight = "size-bar-height",
        BarInset = "size-bar-inset",
        PeekWidth = "size-peek-width",
        PeekHeight = "size-peek-height",
        KumoPeek = "size-kumo-peek",
        KumoPerch = "size-kumo-perch",
        KumoCarried = "size-kumo-carried",
        PanelWidth = "size-panel-width",
        PanelHeight = "size-panel-height",
        PanelInset = "size-panel-inset",
        WindowWidth = "size-window-width",
        WindowHeight = "size-window-height",
        WindowMinWidth = "size-window-min-width",
        WindowMinHeight = "size-window-min-height",
        Header = "size-header",
        SidebarSettings = "size-sidebar-settings",
        SidebarSettingsWindow = "size-sidebar-settings-window",
        SidebarTasks = "size-sidebar-tasks",
        OnboardingWidth = "size-onboarding-width",
        GapCard = "gap-card",
        GapGroupHeader = "gap-group-header",
        GapGroup = "gap-group",
        PadPanel = "pad-panel",
        PadWindow = "pad-window",
        TextDisplay = "text-display",
        LeadingDisplay = "leading-display",
        TextHeading = "text-heading",
        LeadingHeading = "leading-heading",
        TextTitle = "text-title",
        LeadingTitle = "leading-title",
        TextBody = "text-body",
        LeadingBody = "leading-body",
        TextCaption = "text-caption",
        LeadingCaption = "leading-caption",
        TextKeycap = "text-keycap",
        LeadingKeycap = "leading-keycap",
        TextCode = "text-code",
        LeadingCode = "leading-code",
    }
}

/// The look a person picks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Look {
    /// Dusk or Day, as the system is dark or light.
    #[default]
    MatchSystem,
    /// The theme file's dark set.
    Dusk,
    /// The theme file's light set.
    Day,
}

impl Look {
    /// The appearance `NativeShell` shows it in.
    pub(crate) const fn appearance(self) -> Appearance {
        match self {
            Self::MatchSystem => Appearance::System,
            Self::Dusk => Appearance::Dark,
            Self::Day => Appearance::Light,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use nsh_core::element::{AdaptiveColor, FONT_MONO, FONT_SANS};
    use nsh_core::{Color, JsonValue};

    use super::*;

    /// `color` as 8-bit RGBA, Day then Dusk: the pin keeps f32 HSLA and packs it to bytes.
    fn rgba(color: AdaptiveColor) -> (u32, u32) {
        (Color::from(color.light).0, Color::from(color.dark).0)
    }

    /// The names in section `section` of the theme file, as written, each with text for a value:
    /// the reader takes a null as unset without saying so, and Day's value then shows in Dusk.
    fn names(section: &str) -> BTreeSet<String> {
        let file: JsonValue = THEME.parse().expect("the theme file is JSON");
        let fields = file["cssVars"][section]
            .as_object()
            .expect("each section is an object");
        let unwritten: Vec<&String> = fields
            .iter()
            .filter(|(_, value)| !value.is_string())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(unwritten, Vec::<&String>::new(), "{section}: not text");
        fields.keys().cloned().collect()
    }

    #[test]
    fn the_theme_file_reads_with_nothing_left_out() {
        let (_, left_out) = Theme::from_json_lenient(THEME).expect("the theme file is JSON");
        assert_eq!(left_out, Vec::<String>::new());
        for section in ["theme", "dark", "light"] {
            assert!(!names(section).is_empty(), "{section} is empty");
        }
    }

    #[test]
    fn the_pin_reads_each_form_as_the_spec_means_it() {
        let tokens = tokens().expect("the theme file is JSON");
        let color = |name: &str| rgba(tokens.color(name).expect(name));
        assert_eq!(color("attention"), (0x9B42_1DFF, 0xFFC2_A6FF), "a hex");
        // 86% of 255 is 219.3, which the pin's byte rounds to 219 (0xDB); 80% is 204 (0xCC).
        assert_eq!(
            color("glass"),
            (0xFAF9_FFDB, 0x1212_18CC),
            "a colour with alpha"
        );
        assert_eq!(color("foreground"), (0x1B1A_2BFF, 0xF5F3_FFFF), "a hex");
        assert_eq!(
            tokens.color("bubble-foreground"),
            tokens.color("foreground"),
            "a var"
        );
        assert_eq!(
            color("pointer-line"),
            (0xFF14_93FF, 0xFF14_93FF),
            "both looks"
        );
        assert_eq!(tokens.number("radius-panel"), Some(22.0), "a length");
        assert_eq!(tokens.number("text-body"), Some(14.0), "a length");
        assert_eq!(tokens.number("leading-body"), Some(20.0), "a length");
        assert_eq!(
            tokens.number("radius"),
            Some(9.0),
            "NativeShell's own radius"
        );
    }

    #[test]
    fn the_code_and_the_file_name_the_same_tokens() {
        let pin = Theme::default();
        let own: BTreeSet<&str> = pin
            .tokens()
            .colors()
            .map(|(name, _)| name)
            .chain(pin.tokens().numbers().map(|(name, _)| name))
            .chain([FONT_SANS, FONT_MONO])
            .collect();
        let ours: BTreeSet<&str> = Tone::ALL
            .iter()
            .map(|tone| tone.token())
            .chain(Size::ALL.iter().map(|size| size.token()))
            .collect();
        assert_eq!(ours.len(), Tone::ALL.len() + Size::ALL.len(), "named once");
        assert!(
            own.is_disjoint(&ours),
            "NativeShell's own tokens get no name"
        );
        let tokens = tokens().expect("the theme file is JSON");
        let unset: Vec<&str> = Tone::ALL
            .iter()
            .map(|tone| tone.token())
            .filter(|name| tokens.color(name).is_none())
            .chain(
                Size::ALL
                    .iter()
                    .map(|size| size.token())
                    .filter(|name| tokens.number(name).is_none()),
            )
            .collect();
        assert_eq!(unset, Vec::<&str>::new(), "a Tone or a Size the file lacks");
        let unknown: Vec<String> = ["theme", "dark", "light"]
            .into_iter()
            .flat_map(names)
            .filter(|name| !own.contains(name.as_str()) && !ours.contains(name.as_str()))
            .collect();
        assert_eq!(unknown, Vec::<String>::new(), "a name the code lacks");
    }

    #[test]
    fn dusk_and_day_define_the_same_names() {
        assert_eq!(names("dark"), names("light"));
    }

    #[test]
    fn each_look_shows_in_its_appearance() {
        assert_eq!(Look::default(), Look::MatchSystem);
        assert_eq!(Look::MatchSystem.appearance(), Appearance::System);
        assert_eq!(Look::Dusk.appearance(), Appearance::Dark);
        assert_eq!(Look::Day.appearance(), Appearance::Light);
    }
}
