//! Embeds the canonical Flux icon, version resource and Windows manifest into
//! the GUI executable on MSVC Windows builds.
//!
//! The resource compiler is part of the Visual Studio C++ tools used by the
//! Rust MSVC target. When it cannot be found the build continues with a warning
//! so ordinary development still works; release packaging requires the embedded
//! identity and verifies the executable separately.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/flux.ico");
    println!("cargo:rerun-if-changed=../../packaging/windows/flux.manifest");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let icon = root.join("assets/icons/flux.ico");
    let manifest = root.join("packaging/windows/flux.manifest");
    if !icon.is_file() || !manifest.is_file() {
        println!(
            "cargo:warning=Windows icon or manifest is missing; building without embedded identity"
        );
        return;
    }

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let numeric = numeric_version(&version);
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out.join("flux-resource.rc");
    fs::write(
        &rc,
        format!(
            concat!(
                "1 ICON \"{icon}\"\n",
                "1 24 \"{manifest}\"\n",
                "1 VERSIONINFO\n",
                " FILEVERSION {numeric}\n",
                " PRODUCTVERSION {numeric}\n",
                " FILEFLAGSMASK 0x3fL\n",
                " FILEFLAGS 0x0L\n",
                " FILEOS 0x40004L\n",
                " FILETYPE 0x1L\n",
                " FILESUBTYPE 0x0L\n",
                " BEGIN\n",
                "  BLOCK \"StringFileInfo\"\n",
                "  BEGIN\n",
                "   BLOCK \"040904B0\"\n",
                "   BEGIN\n",
                "    VALUE \"CompanyName\", \"NeonMC23\"\n",
                "    VALUE \"FileDescription\", \"Flux local-first music player\"\n",
                "    VALUE \"FileVersion\", \"{version}\"\n",
                "    VALUE \"InternalName\", \"flux\"\n",
                "    VALUE \"LegalCopyright\", \"Copyright (c) 2026 NeonMC23\"\n",
                "    VALUE \"OriginalFilename\", \"flux.exe\"\n",
                "    VALUE \"ProductName\", \"Flux\"\n",
                "    VALUE \"ProductVersion\", \"{version}\"\n",
                "   END\n",
                "  END\n",
                "  BLOCK \"VarFileInfo\"\n",
                "  BEGIN\n",
                "   VALUE \"Translation\", 0x409, 1200\n",
                "  END\n",
                " END\n",
            ),
            icon = escape(&icon),
            manifest = escape(&manifest),
            numeric = numeric,
            version = version
        ),
    )
    .expect("write resource script");

    let Some(compiler) = find_resource_compiler() else {
        println!(
            "cargo:warning=rc.exe was not found; building flux.exe without embedded icon/version"
        );
        return;
    };
    let resource = out.join("flux-resource.res");
    let status = Command::new(&compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&rc)
        .status();
    match status {
        Ok(status) if status.success() && resource.is_file() => {
            println!("cargo:rustc-link-arg={}", resource.display());
        }
        Ok(status) => println!(
            "cargo:warning=Windows resource compiler exited with {status}; identity was not embedded"
        ),
        Err(error) => {
            println!("cargo:warning=could not run Windows resource compiler: {error}");
        }
    }
}

fn numeric_version(version: &str) -> String {
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .take(4)
        .collect::<Vec<_>>();
    while parts.len() < 4 {
        parts.push(0);
    }
    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn escape(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn find_resource_compiler() -> Option<PathBuf> {
    if let Ok(output) = Command::new("where").arg("rc.exe").output()
        && output.status.success()
    {
        let first = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from);
        if let Some(path) = first
            && path.is_file()
        {
            return Some(path);
        }
    }

    let program_files_x86 = env::var_os("ProgramFiles(x86)").map(PathBuf::from);
    if let Some(vswhere) = program_files_x86
        .as_ref()
        .map(|root| root.join("Microsoft Visual Studio/Installer/vswhere.exe"))
        .filter(|path| path.is_file())
    {
        if let Ok(output) = Command::new(&vswhere)
            .args(["-latest", "-products", "*", "-property", "installationPath"])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let installation = PathBuf::from(line.trim());
                if let Some(rc) = find_rc_under(&installation.join("VC/Tools/MSVC")) {
                    return Some(rc);
                }
            }
        }
        if let Ok(output) = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-find",
                r"VC\Tools\MSVC\**\bin\Host*\*\rc.exe",
            ])
            .output()
            && let Some(path) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .find(|path| path.is_file())
        {
            return Some(path);
        }
    }

    // Last-resort known Visual Studio roots. This keeps release builds working
    // without relying on a Developer Command Prompt being active.
    let mut roots = Vec::new();
    if let Some(root) = env::var_os("ProgramFiles").map(PathBuf::from) {
        roots.push(root.join("Microsoft Visual Studio"));
    }
    if let Some(root) = program_files_x86 {
        roots.push(root.join("Microsoft Visual Studio"));
    }
    roots.into_iter().find_map(|root| find_rc_under(&root))
}

fn find_rc_under(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 8 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((path, depth + 1));
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("rc.exe"))
            {
                return Some(path);
            }
        }
    }
    None
}
