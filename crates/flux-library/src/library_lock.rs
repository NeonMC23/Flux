//! Exclusive, crash-safe ownership of a Flux library.
//!
//! Flux assumes a single writer per catalogue. This module turns that
//! assumption into an enforced OS-level lease:
//!
//! - the lock identity is the canonical legacy index file, or the
//!   canonical portable library root;
//! - the lock file is a sibling `<index>.lock` (legacy) or
//!   `<root>/.flux.lock` (portable);
//! - the lease is an exclusive `File::try_lock`, which maps to `flock`
//!   on Unix and `LockFileEx` on Windows;
//! - the OS releases the lock when the handle closes or the process
//!   exits, so a crash can never leave a permanently stuck lock.
//!
//! A lock file is deliberately **never deleted**. Deleting a lock file
//! while another process is waiting on it would allow a third process to
//! lock a different inode for the same library. The file's presence is
//! not ownership; only the OS lock is.
//!
//! Locks are advisory: all Flux writers must go through this type.
//! Filesystems with weak or emulated advisory-lock support (some network
//! shares) cannot provide the same guarantee.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Why exclusive ownership of a library could not be acquired.
#[derive(Debug)]
pub enum LibraryLockError {
    /// Another live Flux process already owns this library.
    Busy {
        /// Canonical library identity.
        identity: PathBuf,
        /// The lock file that is held elsewhere.
        lock_path: PathBuf,
    },
    /// The lock file could not be prepared or the OS lock failed.
    Io {
        /// Canonical library identity, when it could be determined.
        identity: PathBuf,
        /// The lock file, when it could be determined.
        lock_path: Option<PathBuf>,
        /// The underlying I/O error.
        error: io::Error,
    },
}

impl std::fmt::Display for LibraryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { identity, .. } => write!(
                f,
                "The library at {} is already open in another Flux process. \
                 Only one Flux window can write to a library at a time. \
                 Close the other Flux window (GUI or TUI) and try again.",
                identity.display()
            ),
            Self::Io {
                identity,
                lock_path,
                error,
            } => match lock_path {
                Some(lock_path) => write!(
                    f,
                    "Could not reserve exclusive access to the library at {} \
                     (lock file {}): {error}",
                    identity.display(),
                    lock_path.display()
                ),
                None => write!(
                    f,
                    "Could not reserve exclusive access to the library at {}: {error}",
                    identity.display()
                ),
            },
        }
    }
}

impl std::error::Error for LibraryLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::Busy { .. } => None,
        }
    }
}

/// An exclusive OS lease over one library's catalogue.
///
/// The lease has no methods that mutate the library; it exists so that
/// dropping it releases ownership. Hold it for at least as long as any
/// thread or worker that may write to the catalogue.
pub struct LibraryLock {
    /// Held only for the lifetime of the lease; dropping it releases the
    /// OS lock.
    _file: File,
    identity: PathBuf,
    lock_path: PathBuf,
}

impl std::fmt::Debug for LibraryLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryLock")
            .field("identity", &self.identity)
            .field("lock_path", &self.lock_path)
            .finish_non_exhaustive()
    }
}

impl LibraryLock {
    /// Acquires exclusive ownership of the legacy index at `index_path`.
    ///
    /// Existing paths are canonicalized, so relative/absolute spellings
    /// and symlinks to the same index resolve to the same lease. A
    /// missing index uses its canonical parent plus the file name; the
    /// parent directory is created if necessary.
    pub fn acquire_legacy_index(index_path: &Path) -> Result<Self, LibraryLockError> {
        let identity = legacy_identity(index_path)?;
        let lock_path = with_lock_suffix(&identity);
        acquire(identity, lock_path)
    }

    /// Acquires exclusive ownership of the portable library at `root`.
    ///
    /// The root must exist and is canonicalized. The lock file is
    /// `<root>/.flux.lock`, outside the `.flux` storage directory.
    pub fn acquire_portable_root(root: &Path) -> Result<Self, LibraryLockError> {
        let identity = fs::canonicalize(root).map_err(|error| LibraryLockError::Io {
            identity: root.to_owned(),
            lock_path: None,
            error,
        })?;
        let lock_path = identity.join(".flux.lock");
        acquire(identity, lock_path)
    }

    /// The canonical library identity this lease protects.
    #[must_use]
    pub fn identity(&self) -> &Path {
        &self.identity
    }

    /// The lock file backing this lease. It remains on disk after the
    /// lease is released.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

fn acquire(identity: PathBuf, lock_path: PathBuf) -> Result<LibraryLock, LibraryLockError> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| LibraryLockError::Io {
            identity: identity.clone(),
            lock_path: Some(lock_path.clone()),
            error,
        })?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| LibraryLockError::Io {
            identity: identity.clone(),
            lock_path: Some(lock_path.clone()),
            error,
        })?;

    match file.try_lock() {
        Ok(()) => Ok(LibraryLock {
            _file: file,
            identity,
            lock_path,
        }),
        Err(TryLockError::WouldBlock) => Err(LibraryLockError::Busy {
            identity,
            lock_path,
        }),
        Err(TryLockError::Error(error)) => Err(LibraryLockError::Io {
            identity,
            lock_path: Some(lock_path),
            error,
        }),
    }
}

fn legacy_identity(index_path: &Path) -> Result<PathBuf, LibraryLockError> {
    match fs::canonicalize(index_path) {
        Ok(identity) => Ok(identity),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let absolute =
                std::path::absolute(index_path).map_err(|error| LibraryLockError::Io {
                    identity: index_path.to_owned(),
                    lock_path: None,
                    error,
                })?;
            let parent = absolute.parent().ok_or_else(|| LibraryLockError::Io {
                identity: absolute.clone(),
                lock_path: None,
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the library index path has no parent directory",
                ),
            })?;
            let file_name = absolute.file_name().ok_or_else(|| LibraryLockError::Io {
                identity: absolute.clone(),
                lock_path: None,
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the library index path has no file name",
                ),
            })?;
            fs::create_dir_all(parent).map_err(|error| LibraryLockError::Io {
                identity: absolute.clone(),
                lock_path: None,
                error,
            })?;
            let parent = fs::canonicalize(parent).map_err(|error| LibraryLockError::Io {
                identity: absolute.clone(),
                lock_path: None,
                error,
            })?;
            Ok(parent.join(file_name))
        }
        Err(error) => Err(LibraryLockError::Io {
            identity: index_path.to_owned(),
            lock_path: None,
            error,
        }),
    }
}

fn with_lock_suffix(identity: &Path) -> PathBuf {
    let mut name = identity.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{LibraryLock, LibraryLockError};
    use crate::test_support::TempDir;

    fn busy(error: LibraryLockError) -> bool {
        matches!(error, LibraryLockError::Busy { .. })
    }

    #[test]
    fn legacy_lock_excludes_second_handle_and_releases_on_drop() {
        let dir = TempDir::new("lock-legacy");
        let index = dir.write("library-index.json", b"catalogue bytes");
        let first = LibraryLock::acquire_legacy_index(&index).expect("first lease");
        let lock_path = first.lock_path().to_owned();
        assert!(lock_path.ends_with("library-index.json.lock"));

        let second = LibraryLock::acquire_legacy_index(&index).expect_err("must be busy");
        assert!(busy(second), "second lease should report Busy");
        assert_eq!(
            fs::read(&index).expect("index still present"),
            b"catalogue bytes",
            "lock contention never touches the catalogue"
        );

        drop(first);
        let third = LibraryLock::acquire_legacy_index(&index).expect("lease reacquired");
        assert_eq!(
            third.identity(),
            fs::canonicalize(&index).expect("canonical")
        );
        drop(third);
        assert!(
            lock_path.exists(),
            "the lock file remains; its presence is not ownership"
        );
    }

    #[test]
    fn equivalent_legacy_spellings_share_one_lease() {
        let dir = TempDir::new("lock-equivalent");
        let index = dir.write("library-index.json", b"{}");
        let odd = dir.path().join("sub").join("..").join("library-index.json");

        let first = LibraryLock::acquire_legacy_index(&index).expect("first lease");
        let second = LibraryLock::acquire_legacy_index(&odd).expect_err("same identity");
        assert!(busy(second));
        drop(first);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_legacy_index_shares_the_target_lease() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new("lock-symlink");
        let index = dir.write("library-index.json", b"{}");
        let link = dir.path().join("alias.json");
        symlink(&index, &link).expect("symlink");

        let first = LibraryLock::acquire_legacy_index(&index).expect("first lease");
        let second = LibraryLock::acquire_legacy_index(&link).expect_err("same target");
        assert!(busy(second));
        drop(first);
    }

    #[test]
    fn cloned_lease_keeps_the_lock_alive_until_the_last_handle_drops() {
        use std::sync::Arc;

        let dir = TempDir::new("lock-arc");
        let index = dir.write("library-index.json", b"{}");
        let first = Arc::new(LibraryLock::acquire_legacy_index(&index).expect("first lease"));
        let worker = Arc::clone(&first);
        drop(first);

        let error = LibraryLock::acquire_legacy_index(&index).expect_err("worker still owns it");
        assert!(busy(error));
        drop(worker);
        LibraryLock::acquire_legacy_index(&index).expect("last handle released the lock");
    }

    #[test]
    fn portable_lock_excludes_second_handle() {
        let dir = TempDir::new("lock-portable");
        let first = LibraryLock::acquire_portable_root(dir.path()).expect("first lease");
        assert!(first.lock_path().ends_with(".flux.lock"));

        let second = LibraryLock::acquire_portable_root(dir.path()).expect_err("must be busy");
        assert!(busy(second));
        drop(first);
        LibraryLock::acquire_portable_root(dir.path()).expect("lease reacquired");
    }
}
