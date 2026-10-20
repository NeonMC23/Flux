//! Operating-system integration for Flux.
//!
//! Currently this crate provides exactly one thing: resolution of the
//! per-user application **data** and **configuration** directories on
//! Linux (and other XDG-style Unixes), macOS, and Windows, following each
//! platform's standard conventions:
//!
//! | Platform | Data | Config |
//! |---|---|---|
//! | Linux/XDG | `$XDG_DATA_HOME` or `~/.local/share` | `$XDG_CONFIG_HOME` or `~/.config` |
//! | macOS | `~/Library/Application Support` | same (macOS has a single convention) |
//! | Windows | `%LOCALAPPDATA%` (machine-local) | `%APPDATA%` (roaming) |
//!
//! The application name is appended to each base directory. Nothing is
//! created on disk — creating directories is the caller's decision.
//!
//! This is implemented with the standard library only. The obvious
//! crate for this job (`directories`) was evaluated and rejected for
//! now: its `dirs-sys` dependency pulls in `option-ext`, which is
//! MPL-2.0-licensed and outside this project's dependency license
//! policy, and the subset of behavior Flux needs is small and easily
//! testable without it. Resolution is environment-variable based (on
//! Windows it does not call the Known Folders API, whose results the
//! standard environment variables normally mirror).
//!
//! [`resolve_dirs`] takes the platform and an environment lookup as
//! parameters, so every platform's behavior is deterministically
//! testable on any host; [`app_dirs`] is the convenience wrapper using
//! the real process environment.

use std::ffi::OsString;
use std::path::PathBuf;

/// An operating-system family with its own directory conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux and other Unixes following the XDG Base Directory spec.
    Linux,
    /// Apple macOS.
    MacOs,
    /// Microsoft Windows.
    Windows,
}

impl Platform {
    /// The platform this program is running on, or `None` for operating
    /// systems Flux does not currently define conventions for.
    #[must_use]
    pub fn current() -> Option<Self> {
        match std::env::consts::OS {
            "linux" | "freebsd" | "openbsd" | "netbsd" | "dragonfly" => Some(Self::Linux),
            "macos" => Some(Self::MacOs),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }
}

/// Per-user application directories for one application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    data_dir: PathBuf,
    config_dir: PathBuf,
}

impl AppDirs {
    /// Machine-local application data (library indexes, caches of record).
    #[must_use]
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    /// User configuration (settings intended to roam where the platform
    /// distinguishes the two).
    #[must_use]
    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }
}

/// Resolves [`AppDirs`] for `app_name` (for example `"flux"`) from the
/// real process environment on the current platform.
///
/// Returns `None` when the platform is unsupported, `app_name` is not a
/// single plain path component, or the environment lacks the variables
/// needed to locate a home/base directory. No directories are created.
#[must_use]
pub fn app_dirs(app_name: &str) -> Option<AppDirs> {
    resolve_dirs(app_name, Platform::current()?, &|key| std::env::var_os(key))
}

/// Resolves [`AppDirs`] for an explicit platform and environment lookup:
/// the deterministic seam behind [`app_dirs`], suitable for tests and
/// for callers that need to honor a redirected environment.
///
/// Environment values that are empty are treated as unset; XDG variables
/// are honored only when absolute (per the XDG specification).
#[must_use]
pub fn resolve_dirs(
    app_name: &str,
    platform: Platform,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<AppDirs> {
    if app_name.is_empty()
        || app_name == "."
        || app_name == ".."
        || app_name.contains('/')
        || app_name.contains('\\')
    {
        return None;
    }

    let get = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);

    let (data_base, config_base) = match platform {
        Platform::Linux => {
            let home = get("HOME")?;
            let xdg = |key: &str| get(key).filter(|p| p.is_absolute());
            let data = xdg("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
            let config = xdg("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
            (data, config)
        }
        Platform::MacOs => {
            let base = get("HOME")?.join("Library/Application Support");
            (base.clone(), base)
        }
        Platform::Windows => {
            let roaming = get("APPDATA")?;
            let local = get("LOCALAPPDATA").unwrap_or_else(|| roaming.clone());
            (local, roaming)
        }
    };

    Some(AppDirs {
        data_dir: data_base.join(app_name),
        config_dir: config_base.join(app_name),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Platform, app_dirs, resolve_dirs};

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn linux_defaults_to_xdg_base_under_home() {
        let env = env_from(&[("HOME", "/home/ada")]);
        let dirs = resolve_dirs("flux", Platform::Linux, &env).expect("resolves");
        assert_eq!(
            dirs.data_dir(),
            &PathBuf::from("/home/ada/.local/share/flux")
        );
        assert_eq!(dirs.config_dir(), &PathBuf::from("/home/ada/.config/flux"));
    }

    #[test]
    fn linux_honors_absolute_xdg_overrides_and_ignores_bad_ones() {
        let env = env_from(&[
            ("HOME", "/home/ada"),
            ("XDG_DATA_HOME", "/srv/data"),
            ("XDG_CONFIG_HOME", "relative/config"), // must be ignored (XDG spec)
        ]);
        let dirs = resolve_dirs("flux", Platform::Linux, &env).expect("resolves");
        assert_eq!(dirs.data_dir(), &PathBuf::from("/srv/data/flux"));
        assert_eq!(dirs.config_dir(), &PathBuf::from("/home/ada/.config/flux"));

        // Empty values count as unset; without HOME there is nothing.
        let env = env_from(&[("XDG_DATA_HOME", "")]);
        assert_eq!(resolve_dirs("flux", Platform::Linux, &env), None);
    }

    #[test]
    fn macos_uses_application_support_for_both() {
        let env = env_from(&[("HOME", "/Users/ada")]);
        let dirs = resolve_dirs("flux", Platform::MacOs, &env).expect("resolves");
        assert_eq!(
            dirs.data_dir(),
            &PathBuf::from("/Users/ada/Library/Application Support/flux")
        );
        assert_eq!(dirs.config_dir(), dirs.data_dir());
        let empty = env_from(&[]);
        assert_eq!(resolve_dirs("flux", Platform::MacOs, &empty), None);
    }

    #[test]
    fn windows_uses_local_for_data_and_roaming_for_config() {
        let env = env_from(&[
            ("APPDATA", r"C:\Users\ada\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\ada\AppData\Local"),
        ]);
        let dirs = resolve_dirs("flux", Platform::Windows, &env).expect("resolves");
        assert_eq!(
            dirs.data_dir(),
            &PathBuf::from(r"C:\Users\ada\AppData\Local").join("flux")
        );
        assert_eq!(
            dirs.config_dir(),
            &PathBuf::from(r"C:\Users\ada\AppData\Roaming").join("flux")
        );

        // Without LOCALAPPDATA, data falls back to the roaming base;
        // without APPDATA there is no answer at all.
        let env = env_from(&[("APPDATA", r"C:\Users\ada\AppData\Roaming")]);
        let dirs = resolve_dirs("flux", Platform::Windows, &env).expect("resolves via fallback");
        assert_eq!(dirs.data_dir(), dirs.config_dir());
        let empty = env_from(&[]);
        assert_eq!(resolve_dirs("flux", Platform::Windows, &empty), None);
    }

    #[test]
    fn invalid_app_names_are_rejected() {
        let env = env_from(&[("HOME", "/home/ada")]);
        for bad in ["", ".", "..", "a/b", r"a\b"] {
            assert_eq!(
                resolve_dirs(bad, Platform::Linux, &env),
                None,
                "app name {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn real_environment_resolution_is_sane() {
        // HOME (or the Windows equivalent) exists on every realistic dev
        // or CI machine; the resolved dirs must end with the app name.
        if let Some(dirs) = app_dirs("flux") {
            assert!(dirs.data_dir().ends_with("flux"));
            assert!(dirs.config_dir().ends_with("flux"));
        }
    }
}
