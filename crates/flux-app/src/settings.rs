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

/// Default interface text scale percentage.
pub const TEXT_SCALE_DEFAULT: u16 = 100;

/// Smallest supported interface text scale.
pub const TEXT_SCALE_MIN: u16 = 80;

/// Largest supported interface text scale.
pub const TEXT_SCALE_MAX: u16 = 140;

/// Recommended interface text scale presets.
pub const TEXT_SCALE_LEVELS: [u16; 3] = [100, 110, 120];

/// Persisted list/grid presentation for a library surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Information-dense rows.
    #[default]
    List,
    /// Artwork-forward cards.
    Grid,
}

impl ViewMode {
    /// Every mode in presentation order.
    pub const ALL: [Self; 2] = [Self::List, Self::Grid];

    /// Stable settings identifier.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Grid => "grid",
        }
    }

    /// User-facing label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::List => "List",
            Self::Grid => "Grid",
        }
    }

    /// Recovers a mode from a persisted identifier.
    #[must_use]
    pub fn from_id(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.id().eq_ignore_ascii_case(value.trim()))
    }
}

/// Library surface whose view mode is persisted independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewSurface {
    /// The Track table/grid.
    Tracks,
    /// The Artist profile grid/list.
    Artists,
    /// The Album discography grid/list.
    Albums,
}

impl ViewSurface {
    /// Settings key for this surface.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Artists => "artists",
            Self::Albums => "albums",
        }
    }
}

/// Persisted view modes for the three library surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewModePreferences {
    /// Track presentation.
    pub tracks: ViewMode,
    /// Artist presentation.
    pub artists: ViewMode,
    /// Album presentation.
    pub albums: ViewMode,
}

impl Default for ViewModePreferences {
    fn default() -> Self {
        Self {
            tracks: ViewMode::List,
            artists: ViewMode::Grid,
            albums: ViewMode::Grid,
        }
    }
}

impl ViewModePreferences {
    /// Reads one surface preference.
    #[must_use]
    pub const fn get(self, surface: ViewSurface) -> ViewMode {
        match surface {
            ViewSurface::Tracks => self.tracks,
            ViewSurface::Artists => self.artists,
            ViewSurface::Albums => self.albums,
        }
    }

    /// Replaces one surface preference.
    pub fn set(&mut self, surface: ViewSurface, mode: ViewMode) {
        match surface {
            ViewSurface::Tracks => self.tracks = mode,
            ViewSurface::Artists => self.artists = mode,
            ViewSurface::Albums => self.albums = mode,
        }
    }
}

/// Sensible crossfade duration used when enabling without a prior value.
pub const CROSSFADE_DEFAULT_MS: u64 = 3_000;

/// Maximum crossfade duration accepted by the engine.
pub const CROSSFADE_MAX_MS: u64 = flux_audio::engine::MAX_CROSSFADE_MS;

/// Curated accent presets. All presets are chosen for readable contrast on
/// Flux's dark neutral-violet surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accent {
    /// Flux Pink, the established identity default.
    #[default]
    Pink,
    /// Soft violet.
    Violet,
    /// Clear blue.
    Blue,
    /// Calm teal.
    Teal,
    /// Warm amber.
    Amber,
}

impl Accent {
    /// Every preset in presentation order.
    pub const ALL: [Self; 5] = [
        Self::Pink,
        Self::Violet,
        Self::Blue,
        Self::Teal,
        Self::Amber,
    ];

    /// Stable settings identifier.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Pink => "pink",
            Self::Violet => "violet",
            Self::Blue => "blue",
            Self::Teal => "teal",
            Self::Amber => "amber",
        }
    }

    /// User-facing preset name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pink => "Flux Pink",
            Self::Violet => "Violet",
            Self::Blue => "Blue",
            Self::Teal => "Teal",
            Self::Amber => "Amber",
        }
    }

    /// Accent RGB used for solid text, icons and primary controls.
    #[must_use]
    pub const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Pink => (0xff, 0x66, 0xcc),
            Self::Violet => (0xa7, 0x8b, 0xfa),
            Self::Blue => (0x60, 0xa5, 0xfa),
            Self::Teal => (0x5e, 0xe0, 0xc8),
            Self::Amber => (0xf6, 0xc1, 0x5f),
        }
    }

    /// Recovers a preset from a persisted identifier.
    #[must_use]
    pub fn from_id(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|accent| accent.id().eq_ignore_ascii_case(value.trim()))
    }
}

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
    /// Persisted accent preset.
    pub accent: Accent,
    /// Persisted interface text-scale percentage.
    pub text_scale: u16,
    /// Whether non-essential visual motion should be suppressed.
    pub reduced_motion: bool,
    /// Persisted library view modes.
    pub view_modes: ViewModePreferences,
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
        let accent = recover_accent(root.get("theme"));
        let text_scale = recover_text_scale(root.get("interface"));
        let reduced_motion = recover_reduced_motion(root.get("interface"));
        let view_modes = recover_view_modes(root.get("interface"));
        LoadedSettings {
            store: Self {
                path,
                document,
                write_protected: false,
            },
            crossfade,
            audio_output_device,
            accent,
            text_scale,
            reduced_motion,
            view_modes,
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
            accent: Accent::default(),
            text_scale: TEXT_SCALE_DEFAULT,
            reduced_motion: false,
            view_modes: ViewModePreferences::default(),
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
            accent: Accent::default(),
            text_scale: TEXT_SCALE_DEFAULT,
            reduced_motion: false,
            view_modes: ViewModePreferences::default(),
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

    /// Atomically updates only the reduced-motion preference.
    pub fn save_reduced_motion(&mut self, reduced: bool) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let interface = root
            .entry("interface")
            .or_insert_with(|| Value::Object(Map::new()));
        if !interface.is_object() {
            *interface = Value::Object(Map::new());
        }
        interface
            .as_object_mut()
            .expect("object inserted")
            .insert("reduced_motion".to_owned(), Value::Bool(reduced));
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

    /// Atomically updates only the persisted library view modes.
    pub fn save_view_modes(&mut self, modes: ViewModePreferences) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let interface = root
            .entry("interface")
            .or_insert_with(|| Value::Object(Map::new()));
        if !interface.is_object() {
            *interface = Value::Object(Map::new());
        }
        let interface = interface.as_object_mut().expect("object inserted");
        let view_modes = interface
            .entry("view_modes")
            .or_insert_with(|| Value::Object(Map::new()));
        if !view_modes.is_object() {
            *view_modes = Value::Object(Map::new());
        }
        let view_modes = view_modes.as_object_mut().expect("object inserted");
        for (surface, mode) in [
            (ViewSurface::Tracks, modes.tracks),
            (ViewSurface::Artists, modes.artists),
            (ViewSurface::Albums, modes.albums),
        ] {
            view_modes.insert(surface.id().to_owned(), Value::from(mode.id()));
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

    /// Atomically updates only the interface text-scale percentage while
    /// preserving every other setting and unknown fields.
    pub fn save_text_scale(&mut self, percent: u16) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let percent = normalize_text_scale(percent);
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let interface = root
            .entry("interface")
            .or_insert_with(|| Value::Object(Map::new()));
        if !interface.is_object() {
            *interface = Value::Object(Map::new());
        }
        interface
            .as_object_mut()
            .expect("object inserted")
            .insert("text_scale".to_owned(), Value::from(percent));
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

    /// Atomically updates only the accent preset while preserving crossfade,
    /// audio output and unknown fields.
    pub fn save_accent(&mut self, accent: Accent) -> Result<(), SettingsError> {
        if self.write_protected {
            return Err(SettingsError::Protected(self.path.clone()));
        }
        let mut root = match self.document.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        root.insert("version".to_owned(), Value::from(SETTINGS_SCHEMA_VERSION));
        let theme = root
            .entry("theme")
            .or_insert_with(|| Value::Object(Map::new()));
        if !theme.is_object() {
            *theme = Value::Object(Map::new());
        }
        theme
            .as_object_mut()
            .expect("object inserted")
            .insert("accent".to_owned(), Value::from(accent.id()));
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

fn normalize_text_scale(percent: u16) -> u16 {
    percent.clamp(TEXT_SCALE_MIN, TEXT_SCALE_MAX)
}

fn recover_reduced_motion(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get("reduced_motion"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn recover_view_modes(value: Option<&Value>) -> ViewModePreferences {
    let mut modes = ViewModePreferences::default();
    let Some(view_modes) = value
        .and_then(Value::as_object)
        .and_then(|object| object.get("view_modes"))
        .and_then(Value::as_object)
    else {
        return modes;
    };
    for (surface, slot) in [
        (ViewSurface::Tracks, &mut modes.tracks),
        (ViewSurface::Artists, &mut modes.artists),
        (ViewSurface::Albums, &mut modes.albums),
    ] {
        if let Some(mode) = view_modes
            .get(surface.id())
            .and_then(Value::as_str)
            .and_then(ViewMode::from_id)
        {
            *slot = mode;
        }
    }
    modes
}

fn recover_text_scale(value: Option<&Value>) -> u16 {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get("text_scale"))
        .and_then(Value::as_u64)
        .map(|value| normalize_text_scale(u16::try_from(value).unwrap_or(TEXT_SCALE_DEFAULT)))
        .unwrap_or(TEXT_SCALE_DEFAULT)
}

fn recover_accent(value: Option<&Value>) -> Accent {
    value
        .and_then(Value::as_object)
        .and_then(|object| object.get("accent"))
        .and_then(Value::as_str)
        .and_then(Accent::from_id)
        .unwrap_or_default()
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
        Accent, CROSSFADE_DEFAULT_MS, CROSSFADE_MAX_MS, CrossfadeSettings, SettingsError,
        SettingsStore, ViewMode, ViewModePreferences, ViewSurface,
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
            #[cfg(target_os = "macos")]
            let path = fs::canonicalize(&path).expect("canonical temp dir");
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
    fn reduced_motion_round_trips_and_preserves_other_settings() {
        let dir = TempDir::new("reduced-motion");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":4200},"theme":{"accent":"teal"},"interface":{"text_scale":110,"reduced_motion":true},"future":{"kept":true}}"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert!(loaded.reduced_motion);
        assert_eq!(loaded.text_scale, 110);
        let mut store = loaded.store;
        store.save_text_scale(120).expect("save scale");
        let reloaded = SettingsStore::load(&path);
        assert!(
            reloaded.reduced_motion,
            "other interface settings preserve it"
        );
        store
            .save_reduced_motion(false)
            .expect("save reduced motion");
        let reloaded = SettingsStore::load(&path);
        assert!(!reloaded.reduced_motion);
        assert_eq!(reloaded.accent, Accent::Teal);
        assert_eq!(reloaded.text_scale, 120);
        assert_eq!(reloaded.crossfade.duration_ms, 4200);
    }

    #[test]
    fn text_scale_normalizes_and_preserves_unrelated_settings() {
        let dir = TempDir::new("text-scale");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":4200},"theme":{"accent":"violet"},"interface":{"text_scale":116},"future":{"kept":true}}"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(
            loaded.text_scale, 116,
            "custom scale values are preserved within the supported range"
        );
        assert_eq!(loaded.accent, Accent::Violet);
        let mut store = loaded.store;
        store.save_text_scale(110).expect("save text scale");
        let saved: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(saved["interface"]["text_scale"], 110);
        assert_eq!(saved["crossfade"]["duration_ms"], 4200);
        assert_eq!(saved["theme"]["accent"], "violet");
        assert_eq!(saved["future"]["kept"], true);
    }

    #[test]
    fn accent_preset_round_trips_and_preserves_unrelated_settings() {
        let dir = TempDir::new("accent");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":4200},"audio_output":{"device_id":"alsa:test"},"future":{"kept":true}}"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.accent, Accent::Pink);
        let mut store = loaded.store;
        store.save_accent(Accent::Violet).expect("save accent");
        let saved: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(saved["theme"]["accent"], "violet");
        assert_eq!(saved["crossfade"]["duration_ms"], 4200);
        assert_eq!(saved["audio_output"]["device_id"], "alsa:test");
        assert_eq!(saved["future"]["kept"], true);
        let reloaded = SettingsStore::load(&path);
        assert_eq!(reloaded.accent, Accent::Violet);
        assert_eq!(reloaded.crossfade.duration_ms, 4200);
        assert_eq!(reloaded.audio_output_device.as_deref(), Some("alsa:test"));
    }

    #[test]
    fn view_modes_round_trip_and_preserve_every_other_setting() {
        let dir = TempDir::new("view-modes");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":4200},"theme":{"accent":"teal"},"interface":{"text_scale":110,"reduced_motion":true,"view_modes":{"tracks":"grid"}},"future":{"kept":true}}"#,
        )
        .expect("seed settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.view_modes.tracks, ViewMode::Grid);
        assert_eq!(
            loaded.view_modes.artists,
            ViewMode::Grid,
            "missing surfaces use their defaults"
        );
        assert_eq!(loaded.view_modes.albums, ViewMode::Grid);
        let mut store = loaded.store;
        let mut modes = loaded.view_modes;
        modes.set(ViewSurface::Tracks, ViewMode::List);
        modes.set(ViewSurface::Artists, ViewMode::List);
        modes.set(ViewSurface::Albums, ViewMode::List);
        store.save_view_modes(modes).expect("save view modes");
        let saved: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings")).expect("json");
        assert_eq!(saved["interface"]["view_modes"]["tracks"], "list");
        assert_eq!(saved["interface"]["view_modes"]["artists"], "list");
        assert_eq!(saved["interface"]["view_modes"]["albums"], "list");
        assert_eq!(saved["interface"]["text_scale"], 110);
        assert_eq!(saved["interface"]["reduced_motion"], true);
        assert_eq!(saved["crossfade"]["duration_ms"], 4200);
        assert_eq!(saved["theme"]["accent"], "teal");
        assert_eq!(saved["future"]["kept"], true);
        let reloaded = SettingsStore::load(&path);
        assert_eq!(reloaded.view_modes, modes);
        assert_eq!(
            reloaded.view_modes,
            ViewModePreferences {
                tracks: ViewMode::List,
                artists: ViewMode::List,
                albums: ViewMode::List,
            }
        );
        assert_eq!(reloaded.text_scale, 110);
        assert!(reloaded.reduced_motion);
    }

    #[test]
    fn invalid_view_modes_fall_back_to_defaults() {
        let dir = TempDir::new("view-modes-invalid");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"interface":{"view_modes":{"tracks":"mosaic","artists":7,"albums":"GRID"}}}"#,
        )
        .expect("settings");
        let loaded = SettingsStore::load(&path);
        assert_eq!(loaded.view_modes.tracks, ViewMode::List);
        assert_eq!(loaded.view_modes.artists, ViewMode::Grid);
        assert_eq!(
            loaded.view_modes.albums,
            ViewMode::Grid,
            "identifiers are matched case-insensitively"
        );
        assert!(!loaded.store.write_protected());
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
