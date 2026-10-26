//! True process-level evidence that `LibraryLock` excludes another Flux
//! process and that an OS lock is released when its holder is killed.
//!
//! The child test is normally a quick no-op. The parent tests re-exec the
//! same test binary with `--exact child_lock_helper`, wait until the child
//! has acquired and announced the lock, then prove a second acquisition is
//! refused. Killing the child proves crash recovery: the OS releases the
//! lock even though the lock file remains on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use flux_library::{LibraryLock, LibraryLockError};

const CHILD_DIR: &str = "FLUX_LOCK_CHILD_DIR";
const CHILD_KIND: &str = "FLUX_LOCK_CHILD_KIND";
const CHILD_READY: &str = "FLUX_LOCK_CHILD_READY";

/// Runs only when the parent re-execs this binary with the child
/// environment; otherwise it is an inert normal test.
#[test]
fn child_lock_helper() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let kind = std::env::var(CHILD_KIND).expect("child kind");
    let ready = PathBuf::from(std::env::var(CHILD_READY).expect("child ready path"));
    let dir = PathBuf::from(dir);

    let _lease = match kind.as_str() {
        "legacy" => LibraryLock::acquire_legacy_index(&dir.join("index.json")),
        "portable" => LibraryLock::acquire_portable_root(&dir),
        other => panic!("unknown child lock kind {other}"),
    }
    .expect("child acquires its lock");

    fs::write(&ready, b"ready").expect("child writes ready marker");
    std::thread::sleep(Duration::from_secs(60));
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unique_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "flux-lock-process-{}-{name}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("temp dir");
    path
}

fn wait_for_ready(ready: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("lock-holder child exited before becoming ready: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("lock-holder child did not become ready in time");
}

fn acquire(kind: &str, dir: &Path) -> Result<LibraryLock, LibraryLockError> {
    match kind {
        "legacy" => LibraryLock::acquire_legacy_index(&dir.join("index.json")),
        "portable" => LibraryLock::acquire_portable_root(dir),
        other => panic!("unknown lock kind {other}"),
    }
}

fn process_case(kind: &str) {
    let dir = unique_dir(kind);
    let _cleanup = Cleanup(dir.clone());
    if kind == "legacy" {
        fs::write(dir.join("index.json"), b"catalogue bytes").expect("index fixture");
    }
    let ready = dir.join("ready");

    let child = Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "child_lock_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, &dir)
        .env(CHILD_KIND, kind)
        .env(CHILD_READY, &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lock-holder child");
    let mut child = KillOnDrop(child);
    wait_for_ready(&ready, &mut child.0);

    match acquire(kind, &dir) {
        Err(LibraryLockError::Busy { .. }) => {}
        other => panic!("second process must be refused, got {other:?}"),
    }
    if kind == "legacy" {
        assert_eq!(
            fs::read(dir.join("index.json")).expect("index bytes"),
            b"catalogue bytes",
            "contention must not modify the catalogue"
        );
    }

    child.0.kill().expect("kill lock-holder child");
    child.0.wait().expect("reap lock-holder child");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match acquire(kind, &dir) {
            Ok(lease) => {
                drop(lease);
                break;
            }
            Err(LibraryLockError::Busy { .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("lock was not released after process death: {error}"),
        }
    }
}

#[test]
fn process_level_legacy_lock_is_exclusive_and_crash_recoverable() {
    process_case("legacy");
}

#[test]
fn process_level_portable_lock_is_exclusive_and_crash_recoverable() {
    process_case("portable");
}
