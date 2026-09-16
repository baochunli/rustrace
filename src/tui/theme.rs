use std::fmt;

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

pub const PALETTE_FIELD_NAMES: &[&str] = &[
    "accent",
    "panel_bg",
    "sidebar_bg",
    "active_row_bg",
    "selection_bg",
    "surface0",
    "surface1",
    "surface_dim",
    "overlay0",
    "overlay1",
    "text",
    "subtext0",
    "mauve",
    "green",
    "yellow",
    "red",
    "blue",
    "teal",
    "peach",
    "error_bg",
    "warning_bg",
    "tab_active_bg",
    "tab_active_fg",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    Catppuccin,
    CatppuccinLatte,
    Terminal,
}

impl ThemeName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Catppuccin => "catppuccin",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::Terminal => "terminal",
        }
    }

    pub(crate) fn for_colorterm(value: Option<&str>) -> Self {
        if value.is_some_and(|value| {
            value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
        }) {
            Self::Catppuccin
        } else {
            Self::Terminal
        }
    }
}

impl fmt::Display for ThemeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HexColor(Color);

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_hex_color(&value)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("expected a #RRGGBB hex colour"))
    }
}

fn parse_hex_color(value: &str) -> Option<Color> {
    let bytes = value.as_bytes();
    if bytes.len() != 7
        || bytes.first() != Some(&b'#')
        || !bytes[1..].iter().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let component = |start| u8::from_str_radix(&value[start..start + 2], 16).ok();
    Some(Color::Rgb(component(1)?, component(3)?, component(5)?))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeOverrides {
    accent: Option<HexColor>,
    panel_bg: Option<HexColor>,
    sidebar_bg: Option<HexColor>,
    active_row_bg: Option<HexColor>,
    selection_bg: Option<HexColor>,
    surface0: Option<HexColor>,
    surface1: Option<HexColor>,
    surface_dim: Option<HexColor>,
    overlay0: Option<HexColor>,
    overlay1: Option<HexColor>,
    text: Option<HexColor>,
    subtext0: Option<HexColor>,
    mauve: Option<HexColor>,
    green: Option<HexColor>,
    yellow: Option<HexColor>,
    red: Option<HexColor>,
    blue: Option<HexColor>,
    teal: Option<HexColor>,
    peach: Option<HexColor>,
    error_bg: Option<HexColor>,
    warning_bg: Option<HexColor>,
    tab_active_bg: Option<HexColor>,
    tab_active_fg: Option<HexColor>,
}

impl ThemeOverrides {
    fn color(&self, field: &str) -> Option<Color> {
        match field {
            "accent" => self.accent,
            "panel_bg" => self.panel_bg,
            "sidebar_bg" => self.sidebar_bg,
            "active_row_bg" => self.active_row_bg,
            "selection_bg" => self.selection_bg,
            "surface0" => self.surface0,
            "surface1" => self.surface1,
            "surface_dim" => self.surface_dim,
            "overlay0" => self.overlay0,
            "overlay1" => self.overlay1,
            "text" => self.text,
            "subtext0" => self.subtext0,
            "mauve" => self.mauve,
            "green" => self.green,
            "yellow" => self.yellow,
            "red" => self.red,
            "blue" => self.blue,
            "teal" => self.teal,
            "peach" => self.peach,
            "error_bg" => self.error_bg,
            "warning_bg" => self.warning_bg,
            "tab_active_bg" => self.tab_active_bg,
            "tab_active_fg" => self.tab_active_fg,
            _ => return None,
        }
        .map(|color| color.0)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    pub name: Option<ThemeName>,
    pub auto_switch: bool,
    pub light_name: ThemeName,
    pub dark_name: ThemeName,
    pub custom: ThemeOverrides,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: None,
            auto_switch: false,
            light_name: ThemeName::CatppuccinLatte,
            dark_name: ThemeName::Catppuccin,
            custom: ThemeOverrides::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeColorSource {
    BuiltIn(ThemeName),
    Custom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveTheme {
    pub name: ThemeName,
    pub palette: Palette,
    custom: ThemeOverrides,
}

impl EffectiveTheme {
    pub fn source(&self, field: &str) -> Option<ThemeColorSource> {
        self.palette.color(field)?;
        Some(if self.custom.color(field).is_some() {
            ThemeColorSource::Custom
        } else {
            ThemeColorSource::BuiltIn(self.name)
        })
    }
}

pub fn resolve_theme(
    config: &ThemeConfig,
    colorterm: Option<&str>,
    background: Option<[u8; 3]>,
) -> EffectiveTheme {
    let manual_name = config
        .name
        .unwrap_or_else(|| ThemeName::for_colorterm(colorterm));
    let name = if config.auto_switch {
        background.map_or(manual_name, |[red, green, blue]| {
            let luminance = u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114;
            if luminance >= 128_000 {
                config.light_name
            } else {
                config.dark_name
            }
        })
    } else {
        manual_name
    };
    EffectiveTheme {
        name,
        palette: Palette::from_name(name).with_overrides(&config.custom),
        custom: config.custom.clone(),
    }
}

/// The complete colour vocabulary used by the student and replay shells.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    pub accent: Color,
    pub panel_bg: Color,
    pub sidebar_bg: Color,
    pub active_row_bg: Color,
    pub selection_bg: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub surface_dim: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    pub text: Color,
    pub subtext0: Color,
    pub mauve: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub blue: Color,
    pub teal: Color,
    pub peach: Color,
    pub error_bg: Color,
    pub warning_bg: Color,
    pub tab_active_bg: Color,
    pub tab_active_fg: Color,
}

impl Palette {
    /// Catppuccin Mocha, matching the reference shell.
    pub const fn catppuccin() -> Self {
        Self {
            accent: Color::Rgb(137, 180, 250),
            panel_bg: Color::Rgb(24, 24, 37),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(30, 30, 46),
            selection_bg: Color::Rgb(49, 50, 68),
            surface0: Color::Rgb(49, 50, 68),
            surface1: Color::Rgb(69, 71, 90),
            surface_dim: Color::Rgb(30, 30, 46),
            overlay0: Color::Rgb(108, 112, 134),
            overlay1: Color::Rgb(127, 132, 156),
            text: Color::Rgb(205, 214, 244),
            subtext0: Color::Rgb(166, 173, 200),
            mauve: Color::Rgb(203, 166, 247),
            green: Color::Rgb(166, 227, 161),
            yellow: Color::Rgb(249, 226, 175),
            red: Color::Rgb(243, 139, 168),
            blue: Color::Rgb(137, 180, 250),
            teal: Color::Rgb(148, 226, 213),
            peach: Color::Rgb(250, 179, 135),
            error_bg: Color::Rgb(67, 47, 63),
            warning_bg: Color::Rgb(69, 64, 64),
            tab_active_bg: Color::Rgb(137, 180, 250),
            tab_active_fg: Color::Rgb(24, 24, 37),
        }
    }

    /// Catppuccin Latte, with dark text and low-ratio tints for light terminals.
    pub const fn catppuccin_latte() -> Self {
        Self {
            accent: Color::Rgb(30, 102, 245),
            panel_bg: Color::Rgb(239, 241, 245),
            sidebar_bg: Color::Reset,
            active_row_bg: Color::Rgb(230, 233, 239),
            selection_bg: Color::Rgb(189, 208, 245),
            surface0: Color::Rgb(204, 208, 218),
            surface1: Color::Rgb(188, 192, 204),
            surface_dim: Color::Rgb(230, 233, 239),
            overlay0: Color::Rgb(156, 160, 176),
            overlay1: Color::Rgb(140, 143, 161),
            text: Color::Rgb(76, 79, 105),
            subtext0: Color::Rgb(108, 111, 133),
            mauve: Color::Rgb(136, 57, 239),
            green: Color::Rgb(64, 160, 43),
            yellow: Color::Rgb(223, 142, 29),
            red: Color::Rgb(210, 15, 57),
            blue: Color::Rgb(30, 102, 245),
            teal: Color::Rgb(23, 146, 153),
            peach: Color::Rgb(254, 100, 11),
            error_bg: Color::Rgb(233, 195, 207),
            warning_bg: Color::Rgb(235, 221, 201),
            tab_active_bg: Color::Rgb(30, 102, 245),
            tab_active_fg: Color::Rgb(239, 241, 245),
        }
    }

    /// A host-palette-preserving theme requiring only ANSI colours.
    pub const fn terminal() -> Self {
        Self {
            accent: Color::Blue,
            panel_bg: Color::Reset,
            sidebar_bg: Color::Reset,
            active_row_bg: Color::DarkGray,
            selection_bg: Color::Reset,
            surface0: Color::Reset,
            surface1: Color::DarkGray,
            surface_dim: Color::DarkGray,
            overlay0: Color::Gray,
            overlay1: Color::White,
            text: Color::Reset,
            subtext0: Color::Gray,
            mauve: Color::Magenta,
            green: Color::Green,
            yellow: Color::Yellow,
            red: Color::LightRed,
            blue: Color::Blue,
            teal: Color::Cyan,
            peach: Color::Yellow,
            error_bg: Color::Red,
            warning_bg: Color::Yellow,
            tab_active_bg: Color::Blue,
            tab_active_fg: Color::DarkGray,
        }
    }

    pub const fn from_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Catppuccin => Self::catppuccin(),
            ThemeName::CatppuccinLatte => Self::catppuccin_latte(),
            ThemeName::Terminal => Self::terminal(),
        }
    }

    pub fn selected() -> Self {
        Self::for_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }

    pub(crate) fn for_colorterm(value: Option<&str>) -> Self {
        Self::from_name(ThemeName::for_colorterm(value))
    }

    fn with_overrides(mut self, custom: &ThemeOverrides) -> Self {
        for field in PALETTE_FIELD_NAMES {
            if let Some(color) = custom.color(field) {
                self.set_color(field, color);
            }
        }
        self
    }

    fn set_color(&mut self, field: &str, color: Color) {
        match field {
            "accent" => self.accent = color,
            "panel_bg" => self.panel_bg = color,
            "sidebar_bg" => self.sidebar_bg = color,
            "active_row_bg" => self.active_row_bg = color,
            "selection_bg" => self.selection_bg = color,
            "surface0" => self.surface0 = color,
            "surface1" => self.surface1 = color,
            "surface_dim" => self.surface_dim = color,
            "overlay0" => self.overlay0 = color,
            "overlay1" => self.overlay1 = color,
            "text" => self.text = color,
            "subtext0" => self.subtext0 = color,
            "mauve" => self.mauve = color,
            "green" => self.green = color,
            "yellow" => self.yellow = color,
            "red" => self.red = color,
            "blue" => self.blue = color,
            "teal" => self.teal = color,
            "peach" => self.peach = color,
            "error_bg" => self.error_bg = color,
            "warning_bg" => self.warning_bg = color,
            "tab_active_bg" => self.tab_active_bg = color,
            "tab_active_fg" => self.tab_active_fg = color,
            _ => unreachable!("palette field inventory is closed"),
        }
    }

    pub fn color(&self, field: &str) -> Option<Color> {
        Some(match field {
            "accent" => self.accent,
            "panel_bg" => self.panel_bg,
            "sidebar_bg" => self.sidebar_bg,
            "active_row_bg" => self.active_row_bg,
            "selection_bg" => self.selection_bg,
            "surface0" => self.surface0,
            "surface1" => self.surface1,
            "surface_dim" => self.surface_dim,
            "overlay0" => self.overlay0,
            "overlay1" => self.overlay1,
            "text" => self.text,
            "subtext0" => self.subtext0,
            "mauve" => self.mauve,
            "green" => self.green,
            "yellow" => self.yellow,
            "red" => self.red,
            "blue" => self.blue,
            "teal" => self.teal,
            "peach" => self.peach,
            "error_bg" => self.error_bg,
            "warning_bg" => self.warning_bg,
            "tab_active_bg" => self.tab_active_bg,
            "tab_active_fg" => self.tab_active_fg,
            _ => return None,
        })
    }

    pub const fn panel_contrast_fg(&self) -> Color {
        match self.panel_bg {
            Color::Reset => self.surface_dim,
            color => color,
        }
    }

    pub const fn diagnostic_error_bg(&self) -> Color {
        self.error_bg
    }

    pub const fn diagnostic_warning_bg(&self) -> Color {
        self.warning_bg
    }
}

pub fn format_color(color: Color) -> String {
    match color {
        Color::Reset => "reset".to_owned(),
        Color::Black => "black".to_owned(),
        Color::Red => "red".to_owned(),
        Color::Green => "green".to_owned(),
        Color::Yellow => "yellow".to_owned(),
        Color::Blue => "blue".to_owned(),
        Color::Magenta => "magenta".to_owned(),
        Color::Cyan => "cyan".to_owned(),
        Color::Gray => "gray".to_owned(),
        Color::DarkGray => "dark-gray".to_owned(),
        Color::LightRed => "light-red".to_owned(),
        Color::LightGreen => "light-green".to_owned(),
        Color::LightYellow => "light-yellow".to_owned(),
        Color::LightBlue => "light-blue".to_owned(),
        Color::LightMagenta => "light-magenta".to_owned(),
        Color::LightCyan => "light-cyan".to_owned(),
        Color::White => "white".to_owned(),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        Color::Indexed(index) => format!("indexed-{index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_selection_requires_an_explicit_true_colour_colorterm() {
        for value in [Some("truecolor"), Some("TRUECOLOR"), Some("24bit")] {
            assert_eq!(Palette::for_colorterm(value), Palette::catppuccin());
        }
        for value in [None, Some(""), Some("256color"), Some("true-color")] {
            assert_eq!(Palette::for_colorterm(value), Palette::terminal());
        }
    }

    #[test]
    fn terminal_panel_contrast_uses_a_visible_ansi_colour_under_reset() {
        let palette = Palette::terminal();
        assert_eq!(palette.panel_bg, Color::Reset);
        assert_eq!(palette.panel_contrast_fg(), Color::DarkGray);
    }

    #[test]
    fn diagnostic_backgrounds_are_low_ratio_blends_or_dark_ansi_colours() {
        let catppuccin = Palette::catppuccin();
        assert_eq!(catppuccin.diagnostic_error_bg(), Color::Rgb(67, 47, 63));
        assert_eq!(catppuccin.diagnostic_warning_bg(), Color::Rgb(69, 64, 64));

        let terminal = Palette::terminal();
        assert_eq!(terminal.diagnostic_error_bg(), Color::Red);
        assert_eq!(terminal.diagnostic_warning_bg(), Color::Yellow);
    }
}
