//! Recursive filesystem scanning: discovers audio files under
//! caller-provided roots and reconciles them into a [`Library`].
//!
//! Behavior summary (all of it deterministic):
//!
//! - Roots may be directories or individual files; directories are
//!   traversed recursively. Candidate files are processed in sorted path
//!   order, so a scan's result never depends on OS directory order.
//! - **Symlinks are never followed and never imported.** A symlink entry
//!   (including a symlink passed as a root, and broken symlinks) is
//!   reported as *skipped*. This makes traversal cycle-safe by
//!   construction; reach a file through its real path instead.
//! - A file is a candidate when its extension is in
//!   [`SUPPORTED_EXTENSIONS`] (ASCII case-insensitive). Everything else
//!   is *skipped*. Entries that error during traversal or cannot be read
//!   are *failed*. Neither outcome aborts the scan.
//! - Metadata comes from embedded tags where readable. Missing,
//!   malformed, or unreadable tags fall back to a filename-derived
//!   title — tag problems never fail an import. Empty/whitespace tag
//!   fields count as missing.
//! - If the metadata reader itself crashes (panics) on a file, the
//!   panic is contained per file: that file alone is reported *failed*
//!   and the scan continues with the next one.
//! - Reconciliation is by path identity (see the crate-level docs): new
//!   paths are *added*, unchanged paths are *deduplicated*, and paths
//!   whose metadata changed are *updated in place*, keeping their
//!   [`TrackId`](flux_core::TrackId). Scanning never removes tracks,
//!   even ones whose files have vanished.
//! - Overlapping or repeated roots are safe: the same path encountered
//!   twice is added once and then counted as deduplicated (or updated).

use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_core::{Track, TrackSource};
use lofty::prelude::*;

use crate::Library;

/// File extensions treated as audio candidates (compared ASCII
/// case-insensitively). This list mirrors the containers `lofty` can
/// probe; the extension is only a first filter — a file whose contents
/// turn out to be unreadable as audio still imports with a
/// filename-derived title (see the module docs), so inclusion here is
/// intentionally generous (for example `mp4`, which can also be video).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "ape", "flac", "m4a", "m4b", "mp3", "mp4", "mpc", "oga", "ogg", "opus",
    "spx", "wav", "wv",
];

/// One path that a scan did not import, with the reason why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanIssue {
    path: PathBuf,
    reason: String,
}

impl ScanIssue {
    pub(crate) fn new(path: PathBuf, reason: String) -> Self {
        Self { path, reason }
    }

    /// The path that was skipped or failed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A human-actionable explanation (for example the underlying IO or
    /// traversal error message).
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The outcome of one [`Library::scan`] call.
///
/// `added + updated + deduplicated` counts every candidate file that was
/// successfully reconciled; `skipped` and `failed` itemize the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    added: usize,
    updated: usize,
    deduplicated: usize,
    skipped: Vec<ScanIssue>,
    failed: Vec<ScanIssue>,
}

impl ScanReport {
    /// Tracks newly inserted into the library.
    #[must_use]
    pub fn added(&self) -> usize {
        self.added
    }

    /// Existing tracks whose metadata changed and was refreshed in place
    /// (TrackId preserved).
    #[must_use]
    pub fn updated(&self) -> usize {
        self.updated
    }

    /// Candidate files that were already present with identical
    /// metadata; nothing changed for these.
    #[must_use]
    pub fn deduplicated(&self) -> usize {
        self.deduplicated
    }

    /// Entries intentionally not imported (symlinks, unsupported or
    /// missing extensions, non-regular files), each with a reason.
    #[must_use]
    pub fn skipped(&self) -> &[ScanIssue] {
        &self.skipped
    }

    /// Entries that looked importable but could not be handled
    /// (traversal errors, unreadable files), each with a reason.
    #[must_use]
    pub fn failed(&self) -> &[ScanIssue] {
        &self.failed
    }
}

impl Library {
    /// Scans each root recursively and reconciles every supported audio
    /// file found into the library, returning a full account of what
    /// happened. See the [`scan`](crate::scan) module docs for the exact
    /// policies (symlinks, fallback titles, determinism, no pruning).
    ///
    /// This method never fails as a whole: a missing root, an unreadable
    /// directory, a malformed file, or a metadata-reader crash on an
    /// individual file is reported in the returned [`ScanReport`] and
    /// the scan continues.
    pub fn scan(&mut self, roots: &[&Path]) -> ScanReport {
        let mut report = ScanReport::default();
        let mut candidates: Vec<PathBuf> = Vec::new();

        for root in roots {
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                match entry {
                    Ok(entry) => {
                        let file_type = entry.file_type();
                        if file_type.is_symlink() {
                            report.skipped.push(ScanIssue::new(
                                entry.into_path(),
                                "symbolic link (links are never followed or imported)".to_owned(),
                            ));
                        } else if file_type.is_dir() {
                            // Traversal continues into it; nothing to record.
                        } else if file_type.is_file() {
                            let path = entry.into_path();
                            if is_supported_audio_path(&path) {
                                candidates.push(path);
                            } else {
                                report.skipped.push(ScanIssue::new(
                                    path,
                                    "unsupported or missing file extension".to_owned(),
                                ));
                            }
                        } else {
                            report.skipped.push(ScanIssue::new(
                                entry.into_path(),
                                "not a regular file".to_owned(),
                            ));
                        }
                    }
                    Err(err) => {
                        let path = err
                            .path()
                            .map_or_else(|| (*root).to_path_buf(), Path::to_path_buf);
                        report
                            .failed
                            .push(ScanIssue::new(path, format!("traversal error: {err}")));
                    }
                }
            }
        }

        // Deterministic processing order regardless of OS directory order.
        candidates.sort();
        for path in candidates {
            self.import_candidate(&path, &mut report);
        }
        report
    }

    /// Reconciles one candidate file into the library, updating `report`.
    fn import_candidate(&mut self, path: &Path, report: &mut ScanReport) {
        self.import_candidate_with(path, report, read_track);
    }

    /// The workhorse behind `import_candidate`, with the per-file
    /// metadata reader injected — the seam the panic-containment test
    /// uses to simulate a crashing tag reader.
    ///
    /// The read runs inside `catch_unwind`: the tag reader processes
    /// untrusted bytes, and a panic inside it must degrade to one
    /// *failed* report entry — never kill the scan (potentially a
    /// background thread) and lose every other file's result. The
    /// guard is deliberately per-file, not one wrapper around the
    /// whole scan, so partial progress always survives. The process
    /// panic hook is left alone (it belongs to the application — the
    /// TUI installs one); a contained crash still prints its single
    /// diagnostic line there, and the panic payload is not surfaced.
    fn import_candidate_with<F>(&mut self, path: &Path, report: &mut ScanReport, read: F)
    where
        F: FnOnce(&Path) -> Result<Track, String>,
    {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(path)));
        match outcome {
            Ok(Ok(track)) => {
                if let Some(id) = self.id_for_path(path) {
                    if self.get(id) == Some(&track) {
                        report.deduplicated += 1;
                    } else {
                        // Cannot fail: the ID came from this path lookup.
                        let updated = self.update_metadata(id, track);
                        debug_assert!(updated);
                        report.updated += 1;
                    }
                } else {
                    self.insert(track);
                    report.added += 1;
                }
            }
            Ok(Err(reason)) => report
                .failed
                .push(ScanIssue::new(path.to_path_buf(), reason)),
            Err(_) => report.failed.push(ScanIssue::new(
                path.to_path_buf(),
                "metadata reader crashed on this file (contained)".to_owned(),
            )),
        }
    }
}

/// Whether `path` has a supported audio extension.
/// Whether `path` has an extension Flux treats as an audio candidate.
pub fn is_supported_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
}

/// Builds a [`Track`] for one candidate file.
///
/// The only `Err` case is when the file cannot be opened at all
/// (permissions, vanishing mid-scan, …), which the report counts as
/// *failed*. Once a file opens, any tag/content problem (malformed
/// audio, unreadable tags) falls back to a filename-derived title
/// instead — an opened file is always importable at this milestone.
/// Reads one audio file into a [`Track`] without touching a library.
///
/// This is the same metadata path used by folder scans: embedded tags win,
/// with a filename-derived title fallback. The only error is that the file
/// cannot be opened; malformed tags fall back exactly as during a scan.
pub fn read_track(path: &Path) -> Result<Track, String> {
    // Distinguish "cannot read the file" (failed entry) from "cannot
    // parse its tags" (fallback title): lofty reports some parse
    // failures as IO-kind errors (e.g. unexpected EOF), so the probe's
    // error kind alone cannot make this distinction reliably. Opening
    // the file ourselves can.
    if let Err(err) = std::fs::File::open(path) {
        return Err(format!("could not read file: {err}"));
    }
    match lofty::read_from_path(path) {
        Ok(tagged_file) => Ok(track_from_tags(path, &tagged_file)),
        Err(_) => Ok(Track::new(
            fallback_title(path),
            TrackSource::local(path.to_path_buf()),
        )),
    }
}

/// Maps a successfully probed file's tags/properties onto a [`Track`].
fn track_from_tags(path: &Path, tagged_file: &lofty::file::TaggedFile) -> Track {
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let title = tag
        .and_then(|t| cleaned(t.title().as_deref()))
        .unwrap_or_else(|| fallback_title(path));
    let mut track = Track::new(title, TrackSource::local(path.to_path_buf()));

    if let Some(tag) = tag {
        if let Some(artist) = cleaned(tag.artist().as_deref()) {
            track = track.with_artist(artist);
        }
        if let Some(album) = cleaned(tag.album().as_deref()) {
            track = track.with_album(album);
        }
        if let Some(number) = tag.track() {
            track = track.with_track_number(number);
        }
        if let Some(disc) = tag.disk() {
            track = track.with_disc_number(disc);
        }
    }

    let duration = tagged_file.properties().duration();
    if duration != Duration::ZERO {
        track = track.with_duration(duration);
    }
    track
}

/// Trims a tag field and treats empty results as missing.
fn cleaned(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Derives a display title from a file name: the stem (name without its
/// final extension), lossily decoded. Falls back to the full file name,
/// then to a fixed placeholder — every imported track gets *some* title.
fn fallback_title(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    match stem {
        Some(stem) if !stem.trim().is_empty() => stem,
        _ => path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Untitled".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::Track;

    use super::SUPPORTED_EXTENSIONS;
    use crate::Library;
    use crate::test_support::{TempDir, flac_bytes};

    fn tagged_song() -> Vec<u8> {
        flac_bytes(&[
            ("TITLE", "Blue Monday"),
            ("ARTIST", "New Order"),
            ("ALBUM", "Power, Corruption & Lies"),
            ("TRACKNUMBER", "3"),
            ("DISCNUMBER", "1"),
        ])
    }

    #[test]
    fn nested_scan_imports_supported_files_and_reads_tags() {
        let dir = TempDir::new("nested");
        let song = dir.write("song.flac", &tagged_song());
        dir.write("sub/plain.flac", &flac_bytes(&[]));
        dir.write("sub/deep/notes.txt", b"not audio");

        let mut library = Library::new();
        let report = library.scan(&[dir.path()]);

        assert_eq!(report.added(), 2);
        assert_eq!(report.updated(), 0);
        assert_eq!(report.deduplicated(), 0);
        assert_eq!(report.failed().len(), 0);
        assert_eq!(report.skipped().len(), 1);
        assert!(report.skipped()[0].path().ends_with("notes.txt"));

        let id = library.id_for_path(&song).expect("song imported");
        let track = library.get(id).expect("track stored");
        assert_eq!(track.title(), "Blue Monday");
        assert_eq!(track.artist(), Some("New Order"));
        assert_eq!(track.album(), Some("Power, Corruption & Lies"));
        assert_eq!(track.track_number(), Some(3));
        assert_eq!(track.disc_number(), Some(1));
        assert_eq!(track.duration().map(|d| d.as_secs()), Some(3));

        // Untagged file: title derived from the filename stem.
        let plain = library
            .id_for_path(&dir.path().join("sub/plain.flac"))
            .and_then(|pid| library.get(pid))
            .expect("plain imported");
        assert_eq!(plain.title(), "plain");
    }

    #[test]
    fn rescan_is_deduplicated_duplicate_roots_are_safe_and_ids_are_stable() {
        let dir = TempDir::new("rescan");
        let song = dir.write("a.flac", &tagged_song());
        dir.write("b.flac", &flac_bytes(&[]));

        let mut library = Library::new();
        let first = library.scan(&[dir.path()]);
        assert_eq!(first.added(), 2);
        let id_before = library.id_for_path(&song).expect("imported");

        let second = library.scan(&[dir.path()]);
        assert_eq!(second.added(), 0);
        assert_eq!(second.updated(), 0);
        assert_eq!(second.deduplicated(), 2);

        // The same root listed twice in one call behaves the same way.
        let third = library.scan(&[dir.path(), dir.path()]);
        assert_eq!(third.added(), 0);
        assert_eq!(third.deduplicated(), 4);

        assert_eq!(library.id_for_path(&song), Some(id_before));
        assert_eq!(library.len(), 2);
    }

    #[test]
    fn changed_tags_refresh_metadata_without_changing_the_id() {
        let dir = TempDir::new("refresh");
        let song = dir.write("song.flac", &tagged_song());

        let mut library = Library::new();
        library.scan(&[dir.path()]);
        let id = library.id_for_path(&song).expect("imported");

        fs::write(&song, flac_bytes(&[("TITLE", "Blue Monday (Remaster)")]))
            .expect("rewrite fixture");
        let report = library.scan(&[dir.path()]);
        assert_eq!(report.updated(), 1);
        assert_eq!(report.added(), 0);
        assert_eq!(library.id_for_path(&song), Some(id));
        assert_eq!(
            library.get(id).map(Track::title),
            Some("Blue Monday (Remaster)"),
            "metadata refreshed in place"
        );
    }

    #[test]
    fn invalid_audio_and_missing_title_fall_back_to_filename() {
        let dir = TempDir::new("fallback");
        dir.write("garbage.mp3", b"this is not an mp3 at all");
        dir.write("empty-title.flac", &flac_bytes(&[("TITLE", "   ")]));
        dir.write("UPPER.FLAC", &flac_bytes(&[]));

        let mut library = Library::new();
        let report = library.scan(&[dir.path()]);
        assert_eq!(report.added(), 3);
        assert_eq!(
            report.failed().len(),
            0,
            "tag problems must not fail imports"
        );

        let titles: Vec<&str> = library.iter().map(|(_, t)| t.title()).collect();
        assert!(titles.contains(&"garbage"));
        assert!(titles.contains(&"empty-title"));
        assert!(titles.contains(&"UPPER"));
    }

    #[test]
    fn missing_root_is_reported_failed_but_other_roots_still_scan() {
        let dir = TempDir::new("missing-root");
        dir.write("ok.flac", &flac_bytes(&[]));
        let missing = dir.path().join("does-not-exist");

        let mut library = Library::new();
        let report = library.scan(&[missing.as_path(), dir.path()]);
        assert_eq!(report.added(), 1);
        assert_eq!(report.failed().len(), 1);
        assert_eq!(report.failed()[0].path(), missing.as_path());
    }

    #[test]
    fn panicking_metadata_read_is_contained_per_file() {
        // The production scan loop reaches each file through
        // `import_candidate`, which delegates to `import_candidate_with`
        // with the real `read_track`. Injecting a reader that panics on
        // the middle file exercises that exact containment boundary
        // without needing a tag file that reliably crashes lofty. (The
        // injected panic still prints via the ambient panic hook; in
        // tests that output is captured by the harness.)
        let dir = TempDir::new("panic-containment");
        let first = dir.write("a-first.flac", &flac_bytes(&[("TITLE", "First")]));
        let boom = dir.write("b-boom.flac", &flac_bytes(&[]));
        let last = dir.write("c-last.flac", &flac_bytes(&[("TITLE", "Last")]));

        fn reader(path: &Path) -> Result<Track, String> {
            if path.ends_with("b-boom.flac") {
                panic!("injected metadata-reader crash");
            }
            super::read_track(path)
        }

        let mut library = Library::new();
        let mut report = super::ScanReport::default();
        // Same sorted order `Library::scan` would process them in.
        for path in [&first, &boom, &last] {
            library.import_candidate_with(path, &mut report, reader);
        }

        // The panicking file became an ordinary itemized failure, and
        // the files before and after it imported normally.
        assert_eq!(report.added(), 2);
        assert_eq!(report.failed().len(), 1);
        assert_eq!(report.failed()[0].path(), boom.as_path());
        assert!(
            report.failed()[0].reason().contains("crashed"),
            "reason should name the crash: {}",
            report.failed()[0].reason()
        );
        let titles: Vec<&str> = library.iter().map(|(_, t)| t.title()).collect();
        assert_eq!(titles, ["First", "Last"]);
        assert!(library.id_for_path(&boom).is_none());
    }

    #[test]
    fn removed_files_are_not_pruned_by_scanning() {
        let dir = TempDir::new("no-prune");
        let song = dir.write("gone.flac", &flac_bytes(&[]));
        let mut library = Library::new();
        library.scan(&[dir.path()]);
        let id = library.id_for_path(&song).expect("imported");

        fs::remove_file(&song).expect("delete fixture file");
        let report = library.scan(&[dir.path()]);
        assert_eq!(report.added(), 0);
        // The track stays: scanning adds/updates only; pruning is an
        // explicit Library::remove by the caller.
        assert!(library.contains(id));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_and_never_followed() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new("symlinks");
        let real = dir.write("real.flac", &flac_bytes(&[]));
        let link = dir.path().join("link.flac");
        symlink(&real, &link).expect("create symlink");
        let broken = dir.path().join("broken.flac");
        symlink(dir.path().join("nowhere.flac"), &broken).expect("create broken symlink");

        let mut library = Library::new();
        let report = library.scan(&[dir.path()]);
        assert_eq!(report.added(), 1, "only the real file is imported");
        assert_eq!(report.failed().len(), 0);
        let skipped: Vec<PathBuf> = report
            .skipped()
            .iter()
            .map(|i| i.path().to_path_buf())
            .collect();
        assert!(skipped.contains(&link));
        assert!(skipped.contains(&broken));
        assert_eq!(library.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_and_directory_fail_without_aborting_the_scan() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("unreadable");
        dir.write("ok.flac", &flac_bytes(&[]));
        let locked_file = dir.write("locked.flac", &flac_bytes(&[]));
        fs::set_permissions(&locked_file, fs::Permissions::from_mode(0o000)).expect("lock file");
        let locked_dir = dir.path().join("locked-dir");
        dir.write("locked-dir/hidden.flac", &flac_bytes(&[]));
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).expect("lock dir");

        let mut library = Library::new();
        let report = library.scan(&[dir.path()]);

        // Restore permissions so TempDir cleanup can proceed.
        let _ = fs::set_permissions(&locked_file, fs::Permissions::from_mode(0o644));
        let _ = fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755));

        assert_eq!(report.added(), 1, "the readable file still imports");
        let failed: Vec<PathBuf> = report
            .failed()
            .iter()
            .map(|i| i.path().to_path_buf())
            .collect();
        assert!(
            failed.contains(&locked_file),
            "unreadable file reported failed: {failed:?}"
        );
        assert!(
            failed.contains(&locked_dir),
            "unreadable dir reported failed: {failed:?}"
        );
    }

    #[test]
    fn supported_extensions_list_covers_the_common_containers() {
        for expected in ["flac", "mp3", "ogg", "opus", "m4a", "wav"] {
            assert!(
                SUPPORTED_EXTENSIONS.contains(&expected),
                "missing {expected}"
            );
        }
        let supported = Path::new("x.FlAc");
        assert!(super::is_supported_audio_path(supported));
        assert!(!super::is_supported_audio_path(Path::new("x.txt")));
        assert!(!super::is_supported_audio_path(Path::new("noextension")));
    }
}
