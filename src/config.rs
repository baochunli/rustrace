use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::tui::theme::ThemeConfig;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ModifierPreference {
    #[default]
    Auto,
    Command,
    Control,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryModifier {
    Command,
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedConfig {
    pub modifier: ModifierPreference,
    pub theme: ThemeConfig,
    pub path: PathBuf,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModifierResolution {
    pub effective: PrimaryModifier,
    pub warning: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    modifier: ModifierPreference,
    theme: ThemeConfig,
}

pub fn config_file_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(PathBuf::from).map(|path| path.join(".config")))
        .map(|path| path.join("rustrace/config.toml"))
}

pub fn load_config() -> Option<LoadedConfig> {
    let path = config_file_path(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )?;
    Some(load_config_from(&path))
}

pub fn load_config_from(path: &Path) -> LoadedConfig {
    match fs::read_to_string(path) {
        Ok(contents) => match toml::from_str::<ConfigFile>(&contents) {
            Ok(config) => LoadedConfig {
                modifier: config.modifier,
                theme: config.theme,
                path: path.to_owned(),
                warning: None,
            },
            Err(error) => fallback_with_warning(path, format!("invalid configuration: {error}")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadedConfig {
            modifier: ModifierPreference::Auto,
            theme: ThemeConfig::default(),
            path: path.to_owned(),
            warning: None,
        },
        Err(error) => {
            fallback_with_warning(path, format!("configuration could not be read: {error}"))
        }
    }
}

fn fallback_with_warning(path: &Path, error: String) -> LoadedConfig {
    let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
    LoadedConfig {
        modifier: ModifierPreference::Auto,
        theme: ThemeConfig::default(),
        path: path.to_owned(),
        warning: Some(format!(
            "{} in {}; using modifier = \"auto\" and the default theme",
            error,
            path.display()
        )),
    }
}

pub fn resolve_primary_modifier(
    preference: ModifierPreference,
    is_macos: bool,
    keyboard_enhancement_supported: bool,
) -> ModifierResolution {
    match preference {
        ModifierPreference::Auto if is_macos && keyboard_enhancement_supported => {
            ModifierResolution {
                effective: PrimaryModifier::Command,
                warning: None,
            }
        }
        ModifierPreference::Command if keyboard_enhancement_supported => ModifierResolution {
            effective: PrimaryModifier::Command,
            warning: None,
        },
        ModifierPreference::Command => ModifierResolution {
            effective: PrimaryModifier::Control,
            warning: Some(
                "Command modifier unavailable in this terminal; using Control shortcuts".into(),
            ),
        },
        ModifierPreference::Auto | ModifierPreference::Control => ModifierResolution {
            effective: PrimaryModifier::Control,
            warning: None,
        },
    }
}
