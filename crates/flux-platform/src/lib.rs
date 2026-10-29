//! Operating-system integration for Flux.
//!
//! This crate provides:
//!
//! 1. resolution of the per-user application **data** and **configuration**
//!    directories, and
//! 2. dependency-free native file/folder pickers that invoke the platform's
//!    ordinary dialog facility (zenity/kdialog, macOS `osascript`, or
//!    PowerShell WinForms).
//!
//! Directory resolution follows each platform's standard conventions:
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
//!
//! The picker functions are intentionally thin: they return selected paths or
//! a typed cancellation/error and do not create, move, or inspect the chosen
//! files. Linux behavior is exercised in this project's runtime environment;
//! macOS and Windows command paths are compiled but were not runtime-tested
//! here.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

// ------------------------------------------------------- file pickers ---

/// A native file/folder selection request.
///
/// The picker is intentionally implemented by invoking the platform's
/// ordinary dialog facilities instead of adding a GUI dependency:
///
/// - Linux: `zenity`, then `kdialog`
/// - macOS: `osascript` (`choose file` / `choose folder`)
/// - Windows: PowerShell `System.Windows.Forms` dialogs
///
/// `Ok(None)` means the user cancelled. `Err(message)` means no supported
/// native dialog was available or the invoked dialog failed; callers should
/// offer the in-app browser instead. The macOS and Windows paths are
/// compiled but are not runtime-verified by this Linux-based project.
pub fn pick_audio_files() -> Result<Option<Vec<PathBuf>>, String> {
    pick_audio_files_in(None)
}

/// Native folder picker; see [`pick_audio_files`] for platform behavior.
pub fn pick_folder() -> Result<Option<PathBuf>, String> {
    pick_folder_in(None)
}

/// Native audio-file picker with an optional starting directory.
pub fn pick_audio_files_in(start: Option<&Path>) -> Result<Option<Vec<PathBuf>>, String> {
    #[cfg(target_os = "linux")]
    {
        pick_linux_files(start)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = start;
        pick_macos(false)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = start;
        pick_windows(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = start;
        Err("native file dialogs are not implemented on this platform".to_owned())
    }
}

/// Native folder picker with an optional starting directory.
pub fn pick_folder_in(start: Option<&Path>) -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "linux")]
    {
        pick_linux_folder(start)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = start;
        pick_macos(true)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = start;
        pick_windows(true)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = start;
        Err("native folder dialogs are not implemented on this platform".to_owned())
    }
}

#[cfg(target_os = "linux")]
fn linux_start(start: Option<&Path>) -> OsString {
    start
        .map(|path| path.as_os_str().to_owned())
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_else(|| OsString::from("."))
}

#[cfg(target_os = "linux")]
fn pick_linux_files(start: Option<&Path>) -> Result<Option<Vec<PathBuf>>, String> {
    const FILTER: &str = "*.wav *.flac *.mp3 *.m4a *.m4b *.mp4 *.aac *.ogg *.oga *.opus *.aif *.aiff *.ape *.mpc *.spx *.wv";
    let start = linux_start(start);
    let zenity = std::process::Command::new("zenity")
        .arg("--file-selection")
        .arg("--multiple")
        .arg("--separator=\n")
        .arg("--title=Import music")
        .arg(format!("--filename={}", start.to_string_lossy()))
        .arg(format!("--file-filter=Audio files | {FILTER}"))
        .arg("--file-filter=All files | *")
        .output();
    if let Ok(output) = zenity {
        return parse_zenity_output(output);
    }

    let kdialog = std::process::Command::new("kdialog")
        .arg("--getopenfilename")
        .arg(&start)
        .arg(format!("Audio files ({FILTER})"))
        .arg("--multiple")
        .arg("--separate-output")
        .output();
    match kdialog {
        Ok(output) => parse_zenity_output(output),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(
            "no native file dialog is available (install zenity or kdialog, or use Flux Browser)"
                .to_owned(),
        ),
        Err(err) => Err(format!("could not start the native file dialog: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn pick_linux_folder(start: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let start = linux_start(start);
    let zenity = std::process::Command::new("zenity")
        .arg("--file-selection")
        .arg("--directory")
        .arg("--title=Choose a music folder")
        .arg(format!("--filename={}", start.to_string_lossy()))
        .output();
    if let Ok(output) = zenity {
        return parse_single_output(output).map(|path| path.map(PathBuf::from));
    }
    let kdialog = std::process::Command::new("kdialog")
        .arg("--getexistingdirectory")
        .arg(&start)
        .output();
    match kdialog {
        Ok(output) => parse_single_output(output).map(|path| path.map(PathBuf::from)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(
            "no native folder dialog is available (install zenity or kdialog, or use Flux Browser)"
                .to_owned(),
        ),
        Err(err) => Err(format!("could not start the native folder dialog: {err}")),
    }
}

fn dialog_error(stderr: &[u8], status: Option<i32>) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| {
            !line.starts_with("MESA-")
                && !line.starts_with("MESA:")
                && !line.starts_with("Gtk-WARNING")
                && !line.starts_with("Adwaita-WARNING")
        })
        .or_else(|| stderr.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("the native dialog process failed");
    let detail: String = detail.chars().take(180).collect();
    match status {
        Some(code) => format!("native dialog failed (exit {code}): {detail}"),
        None => format!("native dialog failed: {detail}"),
    }
}

fn parse_zenity_output(output: std::process::Output) -> Result<Option<Vec<PathBuf>>, String> {
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(dialog_error(&output.stderr, output.status.code()));
    }
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        Ok(None)
    } else {
        Ok(Some(paths))
    }
}

fn parse_single_output(output: std::process::Output) -> Result<Option<String>, String> {
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(dialog_error(&output.stderr, output.status.code()));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

#[cfg(target_os = "macos")]
fn pick_macos(folder: bool) -> Result<Option<Vec<PathBuf>>, String> {
    let script = if folder {
        r#"try
set chosen to choose folder with prompt "Choose a music folder"
return POSIX path of chosen
on error number -128
return ""
end try"#
    } else {
        r#"try
set chosen to choose file with prompt "Import music" with multiple selections allowed
set output to ""
repeat with item_ in chosen
set output to output & POSIX path of item_ & linefeed
end repeat
return output
on error number -128
return ""
end try"#
    };
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|err| format!("could not start the native file dialog: {err}"))?;
    if !output.status.success() {
        return Err(dialog_error(&output.stderr, output.status.code()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if folder {
        let value = text.trim();
        return Ok((!value.is_empty()).then(|| vec![PathBuf::from(value)]));
    }
    let paths = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    Ok((!paths.is_empty()).then_some(paths))
}

#[cfg(target_os = "windows")]
fn pick_windows(folder: bool) -> Result<Option<Vec<PathBuf>>, String> {
    const AUDIO_FILTER: &str = "*.wav;*.flac;*.mp3;*.m4a;*.m4b;*.mp4;*.aac;*.ogg;*.oga;*.opus;*.aif;*.aiff;*.ape;*.mpc;*.spx;*.wv";
    let script = if folder {
        format!(
            r#"Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = 'Choose a music folder'
if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}"#
        )
    } else {
        format!(
            r#"Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Multiselect = $true
$d.Title = 'Import music'
$d.Filter = 'Audio files|{AUDIO_FILTER}|All files|*.*'
if ($d.ShowDialog() -eq 'OK') {{ $d.FileNames }}"#
        )
    };
    let output = match std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(&script)
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::process::Command::new("pwsh")
                .arg("-NoProfile")
                .arg("-STA")
                .arg("-Command")
                .arg(&script)
                .output()
                .map_err(|err| format!("could not start the native file dialog: {err}"))?
        }
        Err(err) => return Err(format!("could not start the native file dialog: {err}")),
    };
    if !output.status.success() {
        return Err(dialog_error(&output.stderr, output.status.code()));
    }
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    Ok((!paths.is_empty()).then_some(paths))
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

    #[test]
    fn native_dialog_errors_are_short_and_skip_graphics_noise() {
        let message = super::dialog_error(
            b"MESA-EGL: warning\nGtk-WARNING: unsupported\nUser cancelled by the dialog\nmore",
            Some(2),
        );
        assert_eq!(
            message,
            "native dialog failed (exit 2): User cancelled by the dialog"
        );
        let long = vec![b'x'; 500];
        let message = super::dialog_error(&long, None);
        assert!(message.len() <= "native dialog failed: ".len() + 180);
    }
}
