//! Per-user editor view preferences; never assignment provenance.
//!
//! The file lives beside `update-state.json` and uses the same owner-only,
//! atomically replaced JSON. It is deliberately a separate file: older
//! Rustrace releases parse `update-state.json` strictly, so a new field there
//! would make them discard the update preference and cache.
//!
//! Compatibility contract: fields are only ever added. A reader takes the
//! fields it knows and ignores the rest; a writer keeps fields it does not
//! know. A missing, unreadable or invalid file, or a missing or non-boolean
//! field, means the default: line numbers on. A file that exists but cannot
//! be read as a JSON object is never overwritten: saving reports an error and
//! leaves it, and whatever it holds, alone.

use std::{
    env,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};

const FILE_NAME: &str = "editor-preferences.json";
const LINE_NUMBERS: &str = "line_numbers";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorPreferences {
    pub line_numbers: bool,
}

impl Default for EditorPreferences {
    fn default() -> Self {
        Self { line_numbers: true }
    }
}

pub fn preferences_file_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::update::state_directory(xdg, home).map(|path| path.join(FILE_NAME))
}

pub fn preferences_path() -> Option<PathBuf> {
    preferences_file_path(
        env::var_os("XDG_STATE_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

impl EditorPreferences {
    /// Never fails: anything unusable falls back to the defaults.
    pub fn load(path: &Path) -> Self {
        let fields = read_fields(path);
        Self {
            line_numbers: fields
                .get(LINE_NUMBERS)
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }
}

/// The preferences for this user, or the defaults when no state directory is known.
pub fn current() -> EditorPreferences {
    preferences_path()
        .map(|path| EditorPreferences::load(&path))
        .unwrap_or_default()
}

/// Persists the line-number choice for the next launch.
pub fn set_line_numbers(enabled: bool) -> io::Result<()> {
    let path = preferences_path()
        .ok_or_else(|| io::Error::other("Rustrace state directory unavailable"))?;
    set_line_numbers_at(&path, enabled)
}

/// Persists the line-number choice at an explicit path, keeping unknown fields.
pub fn set_line_numbers_at(path: &Path, enabled: bool) -> io::Result<()> {
    let mut fields = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(error),
        Ok(_) => match crate::update::read_cached_json::<Value>(path) {
            Some(Value::Object(fields)) => fields,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a readable preferences file", path.display()),
                ));
            }
        },
    };
    fields
        .entry("schema_version")
        .or_insert_with(|| Value::from(1));
    fields.insert(LINE_NUMBERS.to_owned(), Value::Bool(enabled));
    crate::update::save_json(&Value::Object(fields), path)
}

fn read_fields(path: &Path) -> Map<String, Value> {
    match crate::update::read_cached_json::<Value>(path) {
        Some(Value::Object(fields)) => fields,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "rustrace-preferences-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn path_sits_beside_the_update_state() {
        assert_eq!(
            preferences_file_path(Some(OsStr::new("/x/state")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/x/state/rustrace/editor-preferences.json"))
        );
        assert_eq!(
            preferences_file_path(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from(
                "/home/u/.local/state/rustrace/editor-preferences.json"
            ))
        );
        assert_eq!(
            preferences_file_path(None, Some(OsStr::new("/home/u")))
                .unwrap()
                .parent(),
            crate::update::state_file_path(None, Some(OsStr::new("/home/u")))
                .unwrap()
                .parent()
        );
    }

    #[test]
    fn a_new_install_starts_with_line_numbers_on() {
        let root = scratch();
        let path = root.join("rustrace").join(FILE_NAME);
        assert!(!path.exists());
        assert!(EditorPreferences::load(&path).line_numbers);
        assert!(EditorPreferences::default().line_numbers);
    }

    #[test]
    fn toggling_is_saved_and_read_back_owner_only() {
        let root = scratch();
        let path = root.join("rustrace").join(FILE_NAME);
        set_line_numbers_at(&path, false).unwrap();
        assert!(!EditorPreferences::load(&path).line_numbers);
        let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({"schema_version": 1, "line_numbers": false})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        set_line_numbers_at(&path, true).unwrap();
        assert!(EditorPreferences::load(&path).line_numbers);
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_or_unusable_field_means_on() {
        let root = scratch();
        let path = root.join(FILE_NAME);
        fs::create_dir_all(&root).unwrap();
        for contents in [
            r#"{"schema_version":1}"#,
            r#"{}"#,
            r#"{"schema_version":1,"line_numbers":"off"}"#,
            r#"{"schema_version":1,"line_numbers":null}"#,
            r#"[false]"#,
            "not json",
            "",
        ] {
            fs::write(&path, contents).unwrap();
            assert!(EditorPreferences::load(&path).line_numbers, "{contents:?}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newer_fields_are_ignored_on_read_and_kept_on_write() {
        let root = scratch();
        let path = root.join(FILE_NAME);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{"schema_version":1,"line_numbers":false,"word_wrap":true,"future":{"a":[1]}}"#,
        )
        .unwrap();
        assert!(!EditorPreferences::load(&path).line_numbers);

        set_line_numbers_at(&path, true).unwrap();
        let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "schema_version": 1,
                "line_numbers": true,
                "word_wrap": true,
                "future": {"a": [1]},
            })
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreadable_file_is_left_alone_and_the_choice_is_not_kept() {
        let root = scratch();
        let path = root.join(FILE_NAME);
        fs::create_dir_all(&root).unwrap();
        let oversize = format!(
            r#"{{"line_numbers":false,"padding":"{}"}}"#,
            "x".repeat(70_000)
        );
        for contents in ["not json", "[true]", oversize.as_str()] {
            fs::write(&path, contents).unwrap();
            assert!(set_line_numbers_at(&path, true).is_err(), "{contents:.20}");
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            assert!(EditorPreferences::load(&path).line_numbers);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let contents = r#"{"line_numbers":true,"other":1}"#;
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            // Root can read a mode-000 file; the rule only matters when the read fails.
            if fs::read(&path).is_err() {
                assert!(set_line_numbers_at(&path, false).is_err());
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_update_state_is_untouched_by_the_line_number_choice() {
        let root = scratch();
        let state = root.join("rustrace/update-state.json");
        let preferences = root.join("rustrace").join(FILE_NAME);
        let update = crate::update::UpdateState {
            checks_enabled: false,
            ..crate::update::UpdateState::default()
        };
        update.save(&state).unwrap();
        let before = fs::read(&state).unwrap();
        set_line_numbers_at(&preferences, false).unwrap();
        assert_eq!(fs::read(&state).unwrap(), before);
        assert_eq!(crate::update::UpdateState::load(&state), update);
        fs::remove_dir_all(root).unwrap();
    }
}
