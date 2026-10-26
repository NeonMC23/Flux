//! Small filesystem primitives shared by Flux's persistence layers.
//!
//! Both the legacy index ([`crate::store`]) and the portable catalogue
//! ([`crate::portable`]) publish a fully written, flushed temporary file
//! by replacing the previous file in one step. That operation must have a
//! narrow, predictable failure contract: on failure the previously
//! published file is still there. Anything that deletes the target to
//! "make room" turns a recoverable write failure into data loss.

use std::fs;
use std::io;
use std::path::Path;

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
/// the parent directory afterwards; neither is implied here.
pub(crate) fn replace_file(temp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::replace_file;
    use crate::test_support::TempDir;

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
}
