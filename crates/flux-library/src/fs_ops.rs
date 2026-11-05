//! Small filesystem primitives shared by Flux's persistence layers.
//!
//! The legacy index, queue, playback history and shared settings publish a
//! fully written, flushed temporary file by replacing the previous file in
//! one step. That operation must have a narrow, predictable failure contract:
//! on failure the previously published file is still there. Anything that
//! deletes the target to "make room" turns a recoverable write failure into
//! data loss. Portable generation publication reuses this replacement primitive;
//! canonical transactions keep their specialized journal-aware rename and
//! backup operations.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Replaces `target` with `temp` in one filesystem rename.
///
/// `fs::rename` replaces an existing file on Unix and on Windows (the
/// standard library documents the platform-specific mechanisms), so no
/// remove-then-rename fallback is needed. On any error — including a
/// missing or unreadable `temp`, a permissions problem, or a cross-device
/// rename — both paths' contents are left as they were: in particular,
/// the previously published `target` is never deleted by this function.
///
/// This is deliberately the *only* replacement primitive the storage
/// layers use. Higher layers own syncing the file beforehand and syncing
/// the parent directory afterwards; neither is implied here. On Windows
/// `std` has no directory-fsync operation, so parent-directory durability
/// is a documented no-op there; file content is flushed before replacement
/// and the journal/backup ladders remain the recovery authority.
pub(crate) fn replace_file(temp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temp, target)
}

/// Atomically publishes `bytes` at `path` using a unique sibling temporary
/// file (`<path>.tmp.<pid>.<counter>`).
///
/// The parent directory must already exist. The temporary file is flushed
/// before replacement, removed after a failed write/replace, and the parent
/// directory is synced on Unix after a successful replacement. A failure
/// before replacement leaves the previous target untouched; a failure after
/// replacement can only be the parent-directory sync, in which case the new
/// file is already visible and the caller should report that durability issue.
///
/// The unique name prevents concurrent writers from truncating or renaming
/// each other's staging file. Replacement is still last-writer-wins; this
/// primitive does not merge independent documents. A hard crash can leave an
/// unreferenced uniquely named temporary file, which is never treated as
/// authoritative state.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = temp_path_for(
        path,
        TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temp_path_for(path: &Path, counter: u64) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp.{}.{counter}", std::process::id()));
    PathBuf::from(name)
}

/// Flushes the parent directory on Unix. `std` has no directory fsync on
/// Windows; there this is the documented no-op used by all Flux stores.
fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{replace_file, write_atomic};
    use crate::test_support::TempDir;

    fn staging_files(dir: &TempDir, target_name: &str) -> Vec<std::path::PathBuf> {
        let prefix = format!("{target_name}.tmp.");
        fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(prefix.as_str()))
            })
            .collect()
    }

    #[test]
    fn replacement_publishes_the_temporary_file() {
        let dir = TempDir::new("fs-ops-replace");
        let target = dir.write("target.flux", b"old");
        let temp = dir.write("target.flux.tmp", b"new");

        replace_file(&temp, &target).expect("replacement succeeds");

        assert_eq!(fs::read(&target).expect("read target"), b"new");
        assert!(!temp.exists(), "the temporary name was consumed");
    }

    #[test]
    fn failed_replacement_never_deletes_the_published_file() {
        let dir = TempDir::new("fs-ops-failed-replace");
        let target = dir.write("catalog.flux", b"previously published");
        // A real publication always writes this file first; a missing
        // source is the clearest deterministic way to make the rename
        // fail while the target exists (the old fallback removed the
        // target and then deleted it again by reusing the same failure).
        let missing_temp = dir.path().join("catalog.flux.tmp");

        let error = replace_file(&missing_temp, &target).expect_err("rename must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            fs::read(&target).expect("target still present"),
            b"previously published",
            "a failed publication must preserve the previous file"
        );
    }

    #[test]
    fn replacement_can_create_a_target_that_did_not_exist() {
        let dir = TempDir::new("fs-ops-create");
        let target = dir.path().join("first.flux");
        let temp = dir.write("first.flux.tmp", b"first");

        replace_file(&temp, &target).expect("first publication succeeds");
        assert_eq!(fs::read(&target).expect("read target"), b"first");
    }

    #[test]
    fn atomic_write_creates_replaces_and_cleans_the_sibling_temp() {
        let dir = TempDir::new("atomic-write");
        let target = dir.path().join("state.json");
        write_atomic(&target, b"first").expect("create");
        assert_eq!(fs::read(&target).expect("read"), b"first");
        write_atomic(&target, b"second").expect("replace");
        assert_eq!(fs::read(&target).expect("read"), b"second");
        assert!(staging_files(&dir, "state.json").is_empty());
    }

    #[test]
    fn failed_atomic_write_keeps_the_previous_target_and_cleans_temp() {
        let dir = TempDir::new("atomic-write-failure");
        let target = dir.path().join("state.json");
        fs::create_dir_all(&target).expect("blocking directory");

        let error = write_atomic(&target, b"new").expect_err("rename over directory fails");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::IsADirectory
                | std::io::ErrorKind::AlreadyExists
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::Other
        ));
        assert!(target.is_dir(), "previous target remains a directory");
        assert!(staging_files(&dir, "state.json").is_empty());
    }

    #[test]
    fn concurrent_atomic_writers_never_publish_a_mixed_document() {
        let dir = TempDir::new("atomic-write-concurrent");
        let target = dir.path().join("state.json");
        let first = vec![b'a'; 8 * 1024];
        let second = vec![b'b'; 8 * 1024];
        let successes = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for payload in [&first, &second] {
                let target = target.clone();
                let successes = &successes;
                scope.spawn(move || {
                    for _ in 0..50 {
                        // Windows can report a transient sharing failure while
                        // another replacement is in flight; isolated failures
                        // must still clean their staging file and never publish
                        // a partial payload.
                        if write_atomic(&target, payload).is_ok() {
                            successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        assert!(
            successes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "at least one concurrent publication must succeed"
        );
        let final_bytes = fs::read(&target).expect("final target");
        assert!(
            final_bytes == first || final_bytes == second,
            "the published file must be one complete writer payload"
        );
        assert!(staging_files(&dir, "state.json").is_empty());
    }

    #[test]
    fn atomic_write_does_not_create_a_missing_parent() {
        let dir = TempDir::new("atomic-write-missing-parent");
        let target = dir.path().join("missing/state.json");
        let error = write_atomic(&target, b"new").expect_err("parent must exist");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!dir.path().join("missing").exists());
    }
}
