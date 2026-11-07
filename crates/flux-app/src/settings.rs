//! Shared, versioned user settings.
//!
//! Crossfade configuration is intentionally a small JSON document in the
//! platform configuration directory. Missing or empty files, and older
//! versions without a recognized field, use defaults; malformed files,
//! invalid version fields, and files from a newer schema are preserved and
//! protected from automatic overwrite. Unknown fields are retained when a
//! known setting is saved, and publication uses the shared atomic writer.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use flux_library::write_atomic;
use serde_json::{Map, Value};

/// Current settings schema version written by this build.
pub const SETTINGS_SCHEMA_VERSION: u64 = 1;

/// Sensible crossfade duration used when enabling without a prior value.
pub const CROSSFADE_DEFAULT_MS: u64 = 3_000;

/// Maximum crossfade duration accepted by the engine.
pub const CROSSFADE_MAX_MS: u64 = flux_audio::engine::MAX_CROSSFADE_MS;

/// User-facing crossfade configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossfadeSettings {
    /// Whether natural transitions should overlap.
    pub enabled: bool,
    /// Selected overlap duration in milliseconds.
    pub duration_ms: u64,
}

impl Default for CrossfadeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: CROSSFADE_DEFAULT_MS,
        }
    }
}

impl CrossfadeSettings {
    /// Clamps persisted/requested values into the supported range.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.duration_ms = self.duration_ms.min(CROSSFADE_MAX_MS);
        if self.enabled && self.duration_ms == 0 {
            self.duration_ms = CROSSFADE_DEFAULT_MS;
        }
        self
    }

    /// The duration to apply to the audio engine (zero disables it).
    #[must_use]
    pub fn effective_ms(self) -> u64 {
        let settings = self.normalized();
        if settings.enabled {
            settings.duration_ms
        } else {
            0
        }
    }
}

/// Loaded store plus any non-fatal diagnostic.
#[derive(Debug)]
pub struct LoadedSettings {
    /// Store bound to the file path.
    pub store: SettingsStore,
    /// Crossfade values recovered from the document.
    pub crossfade: CrossfadeSettings,
    /// Persisted explicit output-device ID, or `None` for system default.
    pub audio_output_device: Option<String>,
    /// A malformed/unreadable file diagnostic. The original file is left
    /// untouched and `store.write_protected()` is true.
    pub warning: Option<String>,
}

/// Versioned JSON settings store at a caller-supplied path.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsStore {
    path: PathBuf,
    document: Value,
    write_protected: bool,
}

impl SettingsStore {
    /// Default per-user settings path:
    /// `<config_dir>/flux/settings.json`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        flux_platform::app_dirs("flux").map(|dirs| dirs.config_dir().join("settings.json"))
    }

    /// Loads settings without ever panicking or writing.
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> LoadedSettings {
        let path = path.into();
        let defaults = CrossfadeSettings::default();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::fresh(path, defaults, None);
            }
            Err(error) => {
                let warning = format!(
                    "Could not read settings at {}: {error}. Defaults are in use and \
                     the file will not be overwritten.",
                    path.display()
                );
                return Self::protected(path, defaults, warning);
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Self::fresh(path, defaults, None);
        }
        let document: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                let warning = format!(
                    "Could not parse settings at {}: {error}. Defaults are in use and \
                     the file is preserved.",
                    path.display()
                );
                return Self::protected(path, defaults, warning);
            }
        };
        let Some(root) = document.as_object() else {
            let warning = format!(
                "Settings at {} must be a JSON object; defaults are in use and the \
                 file is preserved.",
                path.display()
            );
            return Self::protected(path, defaults, warning);
        };
        if let Some(version_value) = root.get("version") {
            let Some(version) = version_value.as_u64() else {
                let warning = format!(
                    "Settings at {} have an invalid schema version field; defaults are \
                     in use and the file will not be overwritten.",
                    path.display()
                );
                return Self::protected(path, defaults, warning);
            };
            if version > SETTINGS_SCHEMA_VERSION {
                let warning = format!(
                    "Settings at {} use newer schema version {version} (this build supports \
                     {SETTINGS_SCHEMA_VERSION}); defaults are in use and the file will not \
                     be overwritten.",
                    path.display()
                );
                return Self::protected(path, defaults, warning);
            }
        }
        let crossfade = recover_crossfade(root.get("crossfade"));
        let audio_output_device = recover_audio_output_device(root.get("audio_output"));
        LoadedSettings {
            store: Self {
                path,
                document,
                write_protected: false,
            },
            crossfade,
            audio_output_device,
            warning: None,
        }
    }

    fn fresh(
        path: PathBuf,
        crossfade: CrossfadeSettings,
        warning: Option<String>,
    ) -> LoadedSettings {
        LoadedSettings {
            store: Self {
                path,
                document: Value::Object(Map::new()),
                write_protected: false,
            },
            crossfade,
            audio_output_device: None,
            warning,
        }
    }

    fn protected(path: PathBuf, crossfade: CrossfadeSettings, warning: String) -> LoadedSettings {
        LoadedSettings {
            store: Self {
                path,
                document: Value::Object(Map::new()),
                write_protected: true,
            },
            crossfade,
            audio_output_device: None,
            warning: Some(warning),
        }
    }

    /// The settings file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an unreadable/malformed file must not be overwritten.
    #[must_use]
    pub fn write_protected(&self) -> bool {
        self.write_protected
    }

    /// Atomically writes known settings while preserving unrelated JSON
    /// fields. A failed write leaves the previous file intact.
    pub fn save(&mut self, crossfade: CrossfadeSettings) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let crossfade = crossfade.normalized();
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let crossfade_object = root
            .entry("crossfade")
            .or_insert_with(|| Value::Object(Map::new()));
        if !crossfade_object.is_object() {
            *crossfade_object = Value::Object(Map::new());
        }
        let crossfade_object = crossfade_object.as_object_mut().expect("object inserted");
        crossfade_object.insert("enabled".to_owned(), Value::Bool(crossfade.enabled));
        crossfade_object.insert("duration_ms".to_owned(), Value::from(crossfade.duration_ms));
        let document = Value::Object(root);
        let mut bytes = serde_json::to_vec_pretty(&document).map_err(SettingsError::Serialize)?;
        bytes.push(b'\n');

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(SettingsError::Io)?;
        }
        write_atomic(&self.path, &bytes).map_err(SettingsError::Io)?;
        self.document = document;
        Ok(())
    }

    /// Atomically updates only the explicit audio-output preference while
    /// preserving crossfade values and unknown fields. `None` selects the
    /// system default and removes the stored device ID.
    pub fn save_audio_output_device(
        &mut self,
        device_id: Option<&str>,
    ) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let output = root
            .entry("audio_output")
            .or_insert_with(|| Value::Object(Map::new()));
        if !output.is_object() {
            *output = Value::Object(Map::new());
        }
        let output = output.as_object_mut().expect("object inserted");
        match device_id {
            Some(device_id) => {
                output.insert("device_id".to_owned(), Value::from(device_id));
            }
            None => {
                output.remove("device_id");
            }
        }
        let document = Value::Object(root);
        let mut bytes = serde_json::to_vec_pretty(&document).map_err(SettingsError::Serialize)?;
        bytes.push(b'\n');
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(SettingsError::Io)?;
        }
        write_atomic(&self.path, &bytes).map_err(SettingsError::Io)?;
        self.document = document;
        Ok(())
    }
}

fn recover_audio_output_device(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get("device_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .map(str::to_owned)
}

fn recover_crossfade(value: Option<&Value>) -> CrossfadeSettings {
    let mut settings = CrossfadeSettings::default();
    let Some(object) = value.and_then(Value::as_object) else {
        return settings;
    };
    if let Some(enabled) = object.get("enabled").and_then(Value::as_bool) {
        settings.enabled = enabled;
    }
    if let Some(duration) = object.get("duration_ms").and_then(Value::as_u64) {
        settings.duration_ms = duration.min(CROSSFADE_MAX_MS);
    }
    settings.normalized()
}

/// Settings persistence failure.
#[derive(Debug)]
pub enum SettingsError {
    /// The loaded file was malformed/unreadable and is preserved.
    Protected(PathBuf),
    /// Filesystem failure; the previous file is left in place.
    Io(std::io::Error),
    /// JSON serialization failure.
    Serialize(serde_json::Error),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protected(path) => write!(
                f,
                "settings at {} are write-protected because the existing file is malformed or unreadable",
                path.display()
            ),
            Self::Io(error) => write!(f, "could not save settings: {error}"),
            Self::Serialize(error) => write!(f, "could not serialize settings: {error}"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protected(_) => None,
            Self::Io(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::Value;

    use super::{
        CROSSFADE_DEFAULT_MS, CROSSFADE_MAX_MS, CrossfadeSettings, SettingsError, SettingsStore,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-settings-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn defaults() -> CrossfadeSettings {
        CrossfadeSettings {
            enabled: false,
            duration_ms: CROSSFADE_DEFAULT_MS,
        }
    }

    #[test]
    fn missing_and_empty_settings_use_defaults_and_are_writable() {
        let dir = TempDir::new("defaults");
        let missing = dir.path().join("settings.json");
        let loaded = SettingsStore::load(&missing);
        assert_eq!(loaded.crossfade, defaults());
        assert!(loaded.warning.is_none());
        assert!(!loaded.store.write_protected());
        assert_eq!(loaded.store.path(), missing);
        assert!(!missing.exists(), "loading never creates a file");

        fs::write(&missing, b"  \n\t").expect("empty file");
        let loaded = SettingsStore::load(&missing);
        assert_eq!(loaded.crossfade, defaults());
        assert!(loaded.warning.is_none());
        assert!(!loaded.store.write_protected());
    }

    #[test]
    fn save_round_trips_and_preserves_unknown_fields() {
        let dir = TempDir::new("round-trip");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{
                "version": 1,
                "theme": {"accent": "pink"},
                "future": {"keep": true},
                "crossfade": {"enabled": true, "duration_ms": 4200, "future_curve": "linear"}
            }"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(
            loaded.crossfade,
            CrossfadeSettings {
                enabled: true,
                duration_ms: 4200,
            }
        );
        let mut store = loaded.store;
        store
            .save(CrossfadeSettings {
                enabled: false,
                duration_ms: 2500,
            })
            .expect("save");
        let document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(document["theme"]["accent"], "pink");
        assert_eq!(document["future"]["keep"], true);
        assert_eq!(document["crossfade"]["future_curve"], "linear");
        assert_eq!(document["crossfade"]["enabled"], false);
        assert_eq!(document["crossfade"]["duration_ms"], 2500);
        assert_eq!(document["version"], 1);
        assert!(!dir.path().join("settings.json.tmp").exists());

        let reloaded = SettingsStore::load(&path);
        assert_eq!(
            reloaded.crossfade,
            CrossfadeSettings {
                enabled: false,
                duration_ms: 2500,
            }
        );
    }

    #[test]
    fn invalid_values_are_clamped_or_defaulted_without_panicking() {
        let dir = TempDir::new("invalid");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":999999}}"#,
        )
        .expect("settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(
            loaded.crossfade,
            CrossfadeSettings {
                enabled: true,
                duration_ms: CROSSFADE_MAX_MS,
            }
        );

        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":0}}"#,
        )
        .expect("settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(
            loaded.crossfade,
            CrossfadeSettings {
                enabled: true,
                duration_ms: CROSSFADE_DEFAULT_MS,
            }
        );

        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":"yes","duration_ms":"fast"}}"#,
        )
        .expect("settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.crossfade, defaults());
        assert!(!loaded.store.write_protected());
    }

    #[test]
    fn missing_or_older_fields_use_defaults() {
        let dir = TempDir::new("older");
        let path = dir.path().join("settings.json");
        for body in [b"{}".as_slice(), br#"{"version":0}"#.as_slice()] {
            fs::write(&path, body).expect("settings");
            let loaded = SettingsStore::load(&path);
            assert_eq!(loaded.crossfade, defaults());
            assert!(loaded.warning.is_none());
        }
    }

    #[test]
    fn audio_output_selection_round_trips_without_resetting_crossfade() {
        let dir = TempDir::new("audio-output");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{
                "version": 1,
                "crossfade": {"enabled": true, "duration_ms": 4200},
                "audio_output": {"device_id": "alsa:hw:0,0", "label": "desk"}
            }"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.audio_output_device.as_deref(), Some("alsa:hw:0,0"));
        assert_eq!(loaded.crossfade.duration_ms, 4200);
        let mut store = loaded.store;
        store
            .save_audio_output_device(Some("alsa:hw:1,0"))
            .expect("save selected output");
        let document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(document["audio_output"]["device_id"], "alsa:hw:1,0");
        assert_eq!(document["audio_output"]["label"], "desk");
        assert_eq!(document["crossfade"]["enabled"], true);
        assert_eq!(document["crossfade"]["duration_ms"], 4200);

        store
            .save_audio_output_device(None)
            .expect("save system default");
        let document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert!(document["audio_output"].get("device_id").is_none());
        assert_eq!(document["audio_output"]["label"], "desk");
        assert_eq!(
            SettingsStore::load(&path).audio_output_device,
            None,
            "system default is the absence of an explicit selection"
        );
    }

    #[test]
    fn newer_settings_schema_is_preserved_and_write_protected() {
        let dir = TempDir::new("newer-schema");
        let path = dir.path().join("settings.json");
        let original =
            br#"{"version":2,"crossfade":{"enabled":true,"duration_ms":1000},"future":{"x":1}}"#;
        fs::write(&path, original).expect("settings");
        let mut loaded = SettingsStore::load(&path);
        assert_eq!(loaded.crossfade, defaults());
        assert!(loaded.store.write_protected());
        assert!(
            loaded
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("newer schema version 2")),
            "{:?}",
            loaded.warning
        );
        assert!(matches!(
            loaded.store.save(CrossfadeSettings {
                enabled: true,
                duration_ms: 2_000
            }),
            Err(SettingsError::Protected(_))
        ));
        assert_eq!(fs::read(&path).expect("preserved"), original.as_slice());
    }

    #[test]
    fn non_numeric_settings_version_is_preserved_and_write_protected() {
        let dir = TempDir::new("invalid-version");
        let path = dir.path().join("settings.json");
        let original = br#"{"version":"2","crossfade":{"enabled":true}}"#;
        fs::write(&path, original).expect("settings");
        let loaded = SettingsStore::load(&path);
        assert!(loaded.store.write_protected());
        assert!(
            loaded
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("invalid schema version")),
            "{:?}",
            loaded.warning
        );
        assert_eq!(fs::read(&path).expect("preserved"), original.as_slice());
    }

    #[test]
    fn malformed_settings_are_preserved_and_write_protected() {
        let dir = TempDir::new("malformed");
        let path = dir.path().join("settings.json");
        let original = b"{ not valid json";
        fs::write(&path, original).expect("malformed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.crossfade, defaults());
        assert!(loaded.warning.is_some());
        assert!(loaded.store.write_protected());
        let mut store = loaded.store;
        assert!(matches!(
            store.save(defaults()),
            Err(SettingsError::Protected(_))
        ));
        assert_eq!(fs::read(&path).expect("preserved"), original);
        assert!(!dir.path().join("settings.json.tmp").exists());
    }

    #[test]
    fn failed_save_does_not_poison_later_successful_saves() {
        let dir = TempDir::new("save-retry-preserves-document");
        let path = dir.path().join("settings.json");
        let original = br#"{"version":1,"audio_output":{"device_id":"alsa:test"},"theme":{"accent":"pink"},"future":{"kept":true}}"#;
        fs::write(&path, original).expect("seed settings");
        let mut store = SettingsStore::load(&path).store;
        fs::remove_file(&path).expect("remove seed");
        fs::create_dir_all(&path).expect("blocking directory");
        assert!(
            store
                .save(CrossfadeSettings {
                    enabled: true,
                    duration_ms: 2_000,
                })
                .is_err()
        );
        fs::remove_dir(&path).expect("remove blocker");
        store
            .save(CrossfadeSettings {
                enabled: true,
                duration_ms: 2_500,
            })
            .expect("retry saves");
        let saved: Value =
            serde_json::from_slice(&fs::read(&path).expect("read saved settings")).expect("json");
        assert_eq!(saved["theme"]["accent"], "pink");
        assert_eq!(saved["future"]["kept"], true);
        assert_eq!(saved["audio_output"]["device_id"], "alsa:test");
        assert_eq!(saved["crossfade"]["duration_ms"], 2_500);
    }

    #[test]
    fn failed_save_leaves_previous_path_intact_and_cleans_temp() {
        let dir = TempDir::new("save-failure");
        let path = dir.path().join("settings.json");
        let loaded = SettingsStore::load(&path);
        let mut store = loaded.store;
        // A directory at the target makes the final rename fail.
        fs::create_dir_all(&path).expect("blocking directory");
        let result = store.save(CrossfadeSettings {
            enabled: true,
            duration_ms: 3000,
        });
        assert!(matches!(result, Err(SettingsError::Io(_))));
        assert!(path.is_dir(), "previous target remains");
        assert!(!dir.path().join("settings.json.tmp").exists());
        assert!(
            fs::read_dir(dir.path())
                .expect("read temp dir")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.tmp.")),
            "no shared-writer staging file remains"
        );
    }
}
