//! Process-level tests for the Linux zenity/kdialog picker boundary.
//!
//! A single test owns PATH so the fake dialog executables cannot race with
//! other tests in this integration-test process.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct PathGuard(Option<OsString>);

impl Drop for PathGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

fn dialog(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake dialog");
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod fake dialog");
    path
}

#[test]
fn linux_picker_selection_cancel_failure_fallback_and_malformed_output() {
    let tmp = std::env::temp_dir().join(format!(
        "flux-dialog-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let bin = tmp.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let _path_guard = PathGuard(std::env::var_os("PATH"));
    unsafe {
        std::env::set_var("PATH", &bin);
    }

    let zenity = bin.join("zenity");
    dialog(
        &bin,
        "zenity",
        "printf '/tmp/α β/one.wav\\n/tmp/α β/two.wav\\n'",
    );
    let paths = flux_platform::pick_audio_files_in(None)
        .expect("zenity selection")
        .expect("two paths");
    assert_eq!(
        paths,
        vec![
            PathBuf::from("/tmp/α β/one.wav"),
            PathBuf::from("/tmp/α β/two.wav")
        ]
    );

    dialog(&bin, "zenity", "exit 1");
    assert_eq!(
        flux_platform::pick_audio_files_in(None).expect("cancel"),
        None
    );

    dialog(&bin, "zenity", "echo 'dialog failed' >&2\nexit 2");
    let error = flux_platform::pick_audio_files_in(None).expect_err("failure");
    assert!(error.contains("dialog failed"), "{error}");

    // A missing zenity falls through to kdialog.
    fs::remove_file(&zenity).expect("remove zenity");
    dialog(&bin, "kdialog", "printf '/tmp/kdialog.wav\\n'");
    assert_eq!(
        flux_platform::pick_audio_files_in(None).expect("kdialog selection"),
        Some(vec![PathBuf::from("/tmp/kdialog.wav")])
    );

    // Folder dialogs are single-selection: multiple lines are malformed, not
    // a path containing a newline.
    dialog(&bin, "zenity", "printf '/tmp/one\\n/tmp/two\\n'");
    let error = flux_platform::pick_folder_in(None).expect_err("multi folder");
    assert!(error.contains("more than one path"), "{error}");
    dialog(&bin, "zenity", "printf '/tmp/only folder\\n'");
    assert_eq!(
        flux_platform::pick_folder_in(None).expect("folder"),
        Some(PathBuf::from("/tmp/only folder"))
    );

    // With neither backend, callers get an actionable message.
    fs::remove_file(bin.join("zenity")).expect("remove zenity");
    fs::remove_file(bin.join("kdialog")).expect("remove kdialog");
    let error = flux_platform::pick_audio_files_in(None).expect_err("no backend");
    assert!(error.contains("install zenity or kdialog"), "{error}");
}
