use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use rustrace::config::{
    ModifierPreference, PrimaryModifier, config_file_path, load_config_from,
    resolve_primary_modifier,
};
use rustrace::tui::theme::{PALETTE_FIELD_NAMES, ThemeColorSource, ThemeName, resolve_theme};

#[test]
fn config_path_prefers_xdg_and_falls_back_to_dot_config() {
    assert_eq!(
        config_file_path(Some("/tmp/xdg".as_ref()), Some("/tmp/home".as_ref())),
        Some("/tmp/xdg/rustrace/config.toml".into())
    );
    assert_eq!(
        config_file_path(None, Some("/tmp/home".as_ref())),
        Some("/tmp/home/.config/rustrace/config.toml".into())
    );
    assert_eq!(config_file_path(None, None), None);
}

#[test]
fn missing_config_uses_auto_without_a_warning() {
    let fixture = tempfile_dir("missing");
    let path = fixture.join("config.toml");
    let loaded = load_config_from(&path);

    assert_eq!(loaded.modifier, ModifierPreference::Auto);
    assert_eq!(loaded.theme.name, None);
    assert!(!loaded.theme.auto_switch);
    assert_eq!(loaded.warning, None);
}

#[test]
fn every_modifier_value_is_accepted() {
    for (value, expected) in [
        ("auto", ModifierPreference::Auto),
        ("command", ModifierPreference::Command),
        ("control", ModifierPreference::Control),
    ] {
        let fixture = tempfile_dir(value);
        let path = fixture.join("config.toml");
        fs::write(&path, format!("modifier = {value:?}\n")).unwrap();

        let loaded = load_config_from(&path);
        assert_eq!(loaded.modifier, expected, "{value}");
        assert_eq!(loaded.warning, None, "{value}");
    }
}

#[test]
fn invalid_value_and_unknown_key_name_the_file_and_fall_back_to_auto() {
    for (name, contents) in [
        ("value", "modifier = \"option\"\n"),
        ("unknown", "modifier = \"control\"\ncolour = \"blue\"\n"),
        ("syntax", "modifier = [\n"),
    ] {
        let fixture = tempfile_dir(name);
        let path = fixture.join("config.toml");
        fs::write(&path, contents).unwrap();

        let loaded = load_config_from(&path);
        assert_eq!(loaded.modifier, ModifierPreference::Auto, "{name}");
        let warning = loaded.warning.expect("bad files produce a startup toast");
        assert!(warning.contains(&path.display().to_string()), "{warning}");
        assert!(warning.contains("using modifier = \"auto\""), "{warning}");
        assert!(
            !warning.contains('\n'),
            "startup warning was not one line: {warning:?}"
        );
    }
}

#[test]
fn theme_table_parses_every_selector_and_defaults_to_manual_selection() {
    let fixture = tempfile_dir("theme-table");
    let path = fixture.join("config.toml");
    fs::write(
        &path,
        r#"modifier = "control"

[theme]
name = "terminal"
auto_switch = true
light_name = "catppuccin-latte"
dark_name = "catppuccin"
"#,
    )
    .unwrap();

    let loaded = load_config_from(&path);
    assert_eq!(loaded.modifier, ModifierPreference::Control);
    assert_eq!(loaded.theme.name, Some(ThemeName::Terminal));
    assert!(loaded.theme.auto_switch);
    assert_eq!(loaded.theme.light_name, ThemeName::CatppuccinLatte);
    assert_eq!(loaded.theme.dark_name, ThemeName::Catppuccin);
    assert_eq!(loaded.warning, None);
}

#[test]
fn every_palette_field_accepts_a_hex_override_and_reports_custom_source() {
    assert_eq!(
        PALETTE_FIELD_NAMES.len(),
        23,
        "Palette field inventory drifted"
    );
    for field in PALETTE_FIELD_NAMES {
        let fixture = tempfile_dir(field);
        let path = fixture.join("config.toml");
        fs::write(
            &path,
            format!("[theme]\nname = \"catppuccin\"\n\n[theme.custom]\n{field} = \"#123456\"\n"),
        )
        .unwrap();

        let loaded = load_config_from(&path);
        assert_eq!(loaded.warning, None, "{field}: {:?}", loaded.warning);
        let effective = resolve_theme(&loaded.theme, Some("truecolor"), None);
        assert_eq!(
            effective.palette.color(field),
            Some(Color::Rgb(0x12, 0x34, 0x56)),
            "{field}"
        );
        assert_eq!(
            effective.source(field),
            Some(ThemeColorSource::Custom),
            "{field}"
        );
    }
}

#[test]
fn unset_tint_and_tab_fields_keep_the_selected_builtins_derived_values() {
    let fixture = tempfile_dir("derived-tints");
    let path = fixture.join("config.toml");
    fs::write(
        &path,
        r##"[theme]
name = "catppuccin-latte"

[theme.custom]
panel_bg = "#010203"
red = "#040506"
yellow = "#070809"
accent = "#0a0b0c"
"##,
    )
    .unwrap();

    let effective = resolve_theme(&load_config_from(&path).theme, Some("truecolor"), None);
    assert_eq!(effective.palette.error_bg, Color::Rgb(233, 195, 207));
    assert_eq!(effective.palette.warning_bg, Color::Rgb(235, 221, 201));
    assert_eq!(effective.palette.tab_active_bg, Color::Rgb(30, 102, 245));
    assert_eq!(effective.palette.tab_active_fg, Color::Rgb(239, 241, 245));
    for field in ["error_bg", "warning_bg", "tab_active_bg", "tab_active_fg"] {
        assert_eq!(
            effective.source(field),
            Some(ThemeColorSource::BuiltIn(ThemeName::CatppuccinLatte)),
            "{field}"
        );
    }
}

#[test]
fn invalid_theme_names_and_every_invalid_override_warn_once_and_fall_back() {
    for (name, contents) in [
        ("name", "[theme]\nname = \"mocha\"\n".to_owned()),
        ("light-name", "[theme]\nlight_name = \"latte\"\n".to_owned()),
        ("dark-name", "[theme]\ndark_name = \"dark\"\n".to_owned()),
    ]
    .into_iter()
    .chain(PALETTE_FIELD_NAMES.iter().map(|field| {
        (
            *field,
            format!("[theme.custom]\n{field} = \"not-a-hex-colour\"\n"),
        )
    })) {
        let fixture = tempfile_dir(name);
        let path = fixture.join("config.toml");
        fs::write(&path, contents).unwrap();

        let loaded = load_config_from(&path);
        assert_eq!(loaded.modifier, ModifierPreference::Auto, "{name}");
        assert_eq!(loaded.theme.name, None, "{name}");
        let warning = loaded.warning.expect("invalid theme setting warns");
        assert!(
            warning.contains(&path.display().to_string()),
            "{name}: {warning}"
        );
        assert_eq!(warning.lines().count(), 1, "{name}: {warning:?}");
    }
}

#[test]
fn malformed_unicode_theme_colour_warns_instead_of_panicking() {
    let fixture = tempfile_dir("unicode-theme-colour");
    let path = fixture.join("config.toml");
    fs::write(&path, "[theme.custom]\naccent = \"#aébcd\"\n").unwrap();

    let loaded = load_config_from(&path);
    assert_eq!(loaded.theme.name, None);
    assert_eq!(
        loaded
            .warning
            .as_deref()
            .map(str::lines)
            .map(Iterator::count),
        Some(1)
    );
}

#[test]
fn effective_modifier_obeys_preference_platform_and_terminal_capability() {
    for (preference, macos, supported, effective, warning) in [
        (
            ModifierPreference::Auto,
            true,
            true,
            PrimaryModifier::Command,
            false,
        ),
        (
            ModifierPreference::Auto,
            true,
            false,
            PrimaryModifier::Control,
            false,
        ),
        (
            ModifierPreference::Auto,
            false,
            true,
            PrimaryModifier::Control,
            false,
        ),
        (
            ModifierPreference::Command,
            false,
            true,
            PrimaryModifier::Command,
            false,
        ),
        (
            ModifierPreference::Command,
            true,
            false,
            PrimaryModifier::Control,
            true,
        ),
        (
            ModifierPreference::Control,
            true,
            true,
            PrimaryModifier::Control,
            false,
        ),
    ] {
        let resolved = resolve_primary_modifier(preference, macos, supported);
        assert_eq!(resolved.effective, effective);
        assert_eq!(resolved.warning.is_some(), warning);
        if warning {
            assert!(resolved.warning.unwrap().contains("Control"));
        }
    }
}

struct TempDir(PathBuf);

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tempfile_dir(label: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "rustrace-t10-12-config-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    TempDir(path)
}
