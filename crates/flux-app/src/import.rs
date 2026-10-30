//! Review-first import planning and execution for portable libraries.
//!
//! The import manager is deliberately split into three phases:
//!
//! 1. [`prepare_import_plan`] discovers files and infers candidate metadata
//!    only from filenames/directories without touching the active library
//!    or reading audio tags. It produces editable
//!    [`ImportCandidate`]s, duplicate/conflict hints, and issues.
//! 2. [`prepare_import_files`] performs the potentially slow copy phase in
//!    the background, with cooperative cancellation. It creates no
//!    catalogue entries.
//! 3. [`Controller::apply_import_plan`] inserts/updates tracks through the
//!    existing in-memory library and portable store, then publishes once.
//!
//! A cancelled or failed copy phase therefore cannot leave a catalogue
//! record pointing at an incomplete file. Reviewed imports never rewrite
//! audio tags; user edits become sparse Flux overrides.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flux_core::{ArtistId, Track, TrackId, TrackSource};
use flux_library::portable::{FLUX_DIR_NAME, sync_library_to_portable_store};
use flux_library::{MetadataOverrides, PortableStore, is_supported_audio_path};

use crate::{Controller, ControllerEvent, ImportIssue, StorageMode};

/// How the selected files will reach the portable library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportStorage {
    /// Keep sources in place. Only files already inside the library root
    /// can be referenced safely by the current portable path model.
    Reference,
    /// Copy external sources into the library root, preserving originals.
    #[default]
    Copy,
}

impl ImportStorage {
    /// Human label for review screens.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reference => "Reference existing files",
            Self::Copy => "Copy into the library",
        }
    }
}

/// Coarse stage of an import operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportStage {
    /// Nothing has started.
    #[default]
    Idle,
    /// Walking the selected files/folders.
    Discovering,
    /// Reading audio metadata one file at a time.
    ReadingMetadata,
    /// Comparing paths, fingerprints, and metadata with the catalogue.
    DetectingDuplicates,
    /// Copying selected files (no catalogue changes yet).
    Copying,
    /// Applying reviewed records and publishing the catalogue.
    WritingCatalogue,
    /// Everything requested completed.
    Completed,
    /// The user cancelled a scan/copy phase.
    Cancelled,
    /// A fatal planning/apply error stopped the operation.
    Failed,
}

/// State of one candidate during scanning/copying/applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportItemState {
    /// Not started.
    #[default]
    Pending,
    /// Metadata is being read.
    Scanning,
    /// Scanned and ready for review.
    Ready,
    /// Copying or inserting.
    Importing,
    /// Imported/updated successfully.
    Imported,
    /// Deliberately skipped.
    Skipped,
    /// Failed; the reason is in the progress/result item.
    Failed,
}

/// Background progress update shared with the GUI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportProgress {
    /// Current stage.
    pub stage: ImportStage,
    /// Completed units for the current stage.
    pub done: usize,
    /// Total units, or zero when not measurable.
    pub total: usize,
    /// Human status line.
    pub message: String,
    /// Per-candidate state, indexed by plan candidate order.
    pub items: Vec<ImportItemState>,
}

/// Why a candidate resembles something already in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateStatus {
    /// No path/content/metadata conflict found.
    New,
    /// The exact same path is already in this library.
    ExistingPath(TrackId),
    /// Different path, but the file content fingerprint and size match.
    SameContent {
        /// Existing catalogue track, when the match is against the store.
        track_id: Option<TrackId>,
        /// Another selected source with the same content, if any.
        other_source: Option<PathBuf>,
    },
    /// Different file, but title/artist/album match an existing track.
    SimilarMetadata(TrackId),
}

impl CandidateStatus {
    /// Whether the status blocks a new duplicate catalogue entry.
    #[must_use]
    pub fn is_exact_path(&self) -> bool {
        matches!(self, Self::ExistingPath(_))
    }

    /// Stable track ID associated with an exact/similar existing match.
    #[must_use]
    pub fn matched_track(&self) -> Option<TrackId> {
        match self {
            Self::ExistingPath(id) | Self::SimilarMetadata(id) => Some(*id),
            Self::SameContent { track_id, .. } => *track_id,
            Self::New => None,
        }
    }
}

/// What the user chose for one reviewed candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportDecision {
    /// Add a new catalogue record (or update via an explicit exact-path choice).
    ImportNew,
    /// Update the existing exact-path track's Flux metadata, preserving its ID.
    UpdateExisting,
    /// Do nothing for this candidate.
    Skip,
}

/// One ordered credit in the import review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCredit {
    /// Existing stable artist, or `None` for a name that still needs resolution.
    pub artist_id: Option<ArtistId>,
    /// Display/name input used when no stable ID is selected.
    pub name: String,
    /// Presentation hint only; the canonical model stores ordered credits.
    pub featuring: bool,
}

impl ImportCredit {
    /// Constructs a credit.
    #[must_use]
    pub fn new(artist_id: Option<ArtistId>, name: impl Into<String>, featuring: bool) -> Self {
        Self {
            artist_id,
            name: name.into(),
            featuring,
        }
    }
}

/// One scanned file in the import review.
#[derive(Debug, Clone)]
pub struct ImportCandidate {
    source_path: PathBuf,
    source_track: Track,
    file_size: u64,
    format: String,
    status: CandidateStatus,
    decision: ImportDecision,
    destination: Option<PathBuf>,
    storage_error: Option<String>,
    base_title: String,
    base_artist_name: Option<String>,
    base_album: Option<String>,
    base_track_number: Option<u32>,
    base_disc_number: Option<u32>,
    base_artist_id: Option<ArtistId>,
    title: String,
    artist_name: Option<String>,
    album: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    artist_id: Option<ArtistId>,
    credits: Vec<ImportCredit>,
    owner_index: usize,
    base_credits: Vec<ImportCredit>,
    metadata_incomplete: bool,
}

impl ImportCandidate {
    /// Source path selected by the user.
    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// The source metadata read from the file (fallbacks included).
    #[must_use]
    pub fn source_track(&self) -> &Track {
        &self.source_track
    }

    /// File size in bytes.
    #[must_use]
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Short format label (for example `FLAC`).
    #[must_use]
    pub fn format(&self) -> &str {
        &self.format
    }

    /// Duplicate/conflict hint.
    #[must_use]
    pub fn status(&self) -> &CandidateStatus {
        &self.status
    }

    /// Current decision.
    #[must_use]
    pub fn decision(&self) -> ImportDecision {
        self.decision
    }

    /// Planned destination for copy mode, or the in-place path for a file
    /// already inside the library. `None` means reference mode cannot safely
    /// represent this external source.
    #[must_use]
    pub fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    /// Why reference mode cannot represent this source, when applicable.
    #[must_use]
    pub fn storage_error(&self) -> Option<&str> {
        self.storage_error.as_deref()
    }

    /// Current reviewed title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Current reviewed artist display name.
    #[must_use]
    pub fn artist_name(&self) -> Option<&str> {
        self.artist_name.as_deref()
    }

    /// Current reviewed album.
    #[must_use]
    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    /// Current reviewed track number.
    #[must_use]
    pub fn track_number(&self) -> Option<u32> {
        self.track_number
    }

    /// Current reviewed disc number.
    #[must_use]
    pub fn disc_number(&self) -> Option<u32> {
        self.disc_number
    }

    /// Selected stable artist identity, if any.
    #[must_use]
    pub fn artist_id(&self) -> Option<ArtistId> {
        self.artist_id
    }

    /// Ordered credits currently under review.
    #[must_use]
    pub fn credits(&self) -> &[ImportCredit] {
        &self.credits
    }

    /// Index of the physical owner inside [`credits`](Self::credits).
    #[must_use]
    pub fn owner_index(&self) -> usize {
        self.owner_index
    }

    /// Adds an ordered credit. The first credit becomes the default owner when
    /// there was no valid owner index.
    pub fn add_credit(&mut self, credit: ImportCredit) {
        if self.credits.is_empty() {
            self.owner_index = 0;
        }
        self.credits.push(credit);
    }

    /// Removes one credit. Refuses to remove the physical owner.
    pub fn remove_credit(&mut self, index: usize) -> bool {
        if index >= self.credits.len() || index == self.owner_index || self.credits.len() == 1 {
            return false;
        }
        self.credits.remove(index);
        if index < self.owner_index {
            self.owner_index -= 1;
        }
        self.sync_owner_fields();
        true
    }

    /// Moves a credit up or down while preserving the owner by identity.
    pub fn move_credit(&mut self, index: usize, delta: isize) -> bool {
        if index >= self.credits.len() {
            return false;
        }
        let target = index as isize + delta;
        if target < 0 || target as usize >= self.credits.len() {
            return false;
        }
        let target = target as usize;
        self.credits.swap(index, target);
        if self.owner_index == index {
            self.owner_index = target;
        } else if self.owner_index == target {
            self.owner_index = index;
        }
        self.sync_owner_fields();
        true
    }

    fn sync_owner_fields(&mut self) {
        if let Some(credit) = self.credits.get(self.owner_index) {
            self.artist_name = Some(credit.name.clone());
            self.artist_id = credit.artist_id;
        }
    }

    /// Selects a credit as the physical owner. The list must remain non-empty;
    /// invalid indexes are ignored.
    pub fn set_owner_index(&mut self, index: usize) -> bool {
        if index >= self.credits.len() {
            return false;
        }
        self.owner_index = index;
        self.artist_name = Some(self.credits[index].name.clone());
        self.artist_id = self.credits[index].artist_id;
        true
    }

    /// Updates the credit at `index` to an existing stable artist.
    pub fn set_credit_artist(&mut self, index: usize, artist_id: ArtistId, name: String) {
        if let Some(credit) = self.credits.get_mut(index) {
            credit.artist_id = Some(artist_id);
            credit.name = name;
            if index == self.owner_index {
                self.artist_id = Some(artist_id);
                self.artist_name = Some(self.credits[index].name.clone());
            }
        }
    }

    /// First validation problem that must block canonical confirmation.
    #[must_use]
    pub fn validation_error(&self) -> Option<String> {
        if self.title.trim().is_empty() {
            return Some("Track title is required.".to_owned());
        }
        if self.credits.is_empty() {
            return Some("At least one credited artist is required.".to_owned());
        }
        if self.owner_index >= self.credits.len() {
            return Some("Select a physical owner from the credited artists.".to_owned());
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for credit in &self.credits {
            if let Some(id) = credit.artist_id {
                if !ids.insert(id) {
                    return Some("The same artist cannot appear twice in the credits.".to_owned());
                }
            } else {
                let name = credit.name.trim();
                if name.is_empty() {
                    return Some("Every credit needs a name or an existing artist.".to_owned());
                }
                if !names.insert(name.to_lowercase()) {
                    return Some(
                        "The same artist name cannot appear twice; select identities explicitly."
                            .to_owned(),
                    );
                }
            }
        }
        None
    }

    /// Whether source metadata was incomplete.
    #[must_use]
    pub fn metadata_incomplete(&self) -> bool {
        self.metadata_incomplete
    }

    /// Whether any reviewed field differs from its source/base value.
    #[must_use]
    pub fn is_edited(&self) -> bool {
        self.title != self.base_title
            || self.artist_name != self.base_artist_name
            || self.album != self.base_album
            || self.track_number != self.base_track_number
            || self.disc_number != self.base_disc_number
            || self.artist_id != self.base_artist_id
            || self.owner_index != 0
            || self.credits != self.base_credits
    }

    /// Sets the reviewed title. An empty title means the display fallback.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Sets the reviewed artist display name. The stable relationship is
    /// controlled separately with [`set_artist_id`](Self::set_artist_id).
    pub fn set_artist_name(&mut self, artist: Option<String>) {
        self.artist_name = artist
            .map(|artist| artist.trim().to_owned())
            .filter(|artist| !artist.is_empty());
        if let Some(credit) = self.credits.get_mut(self.owner_index) {
            credit.name = self
                .artist_name
                .clone()
                .unwrap_or_else(|| "Unknown Artist".to_owned());
            credit.artist_id = self.artist_id;
        }
    }

    /// Sets the reviewed album.
    pub fn set_album(&mut self, album: Option<String>) {
        self.album = album
            .map(|album| album.trim().to_owned())
            .filter(|album| !album.is_empty());
    }

    /// Sets the reviewed track number.
    pub fn set_track_number(&mut self, number: Option<u32>) {
        self.track_number = number;
    }

    /// Sets the reviewed disc number.
    pub fn set_disc_number(&mut self, number: Option<u32>) {
        self.disc_number = number;
    }

    /// Selects an existing stable artist identity and optionally updates the
    /// display label shown by the review UI.
    pub fn set_artist_id(&mut self, artist_id: Option<ArtistId>, display_name: Option<String>) {
        self.artist_id = artist_id;
        if let Some(name) = display_name {
            self.set_artist_name(Some(name));
        } else if let Some(credit) = self.credits.get_mut(self.owner_index) {
            credit.artist_id = artist_id;
            self.artist_name = Some(credit.name.clone());
        }
    }

    /// Chooses what to do with this candidate.
    ///
    /// Exact-path duplicates can only be skipped or updated; they can never
    /// create a second catalogue owner for the same path. Non-exact
    /// candidates cannot be marked update-existing.
    pub fn set_decision(&mut self, decision: ImportDecision) {
        self.decision = match (self.status.is_exact_path(), decision) {
            (true, ImportDecision::ImportNew) | (false, ImportDecision::UpdateExisting) => {
                ImportDecision::Skip
            }
            (_, decision) => decision,
        };
    }

    fn set_destination(&mut self, destination: Option<PathBuf>, storage_error: Option<String>) {
        self.destination = destination;
        self.storage_error = storage_error;
    }

    fn base_fields(&self) -> (&str, Option<&str>, Option<&str>, Option<u32>, Option<u32>) {
        (
            &self.base_title,
            self.base_artist_name.as_deref(),
            self.base_album.as_deref(),
            self.base_track_number,
            self.base_disc_number,
        )
    }
}

/// A complete review plan. Edits and decisions live here until commit.
#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// Selected storage policy.
    pub storage: ImportStorage,
    /// Portable library destination root.
    pub library_root: PathBuf,
    /// Whether folder sources were scanned recursively.
    pub recursive: bool,
    /// Editable candidates in discovery order.
    pub candidates: Vec<ImportCandidate>,
    /// Per-path discovery/metadata issues.
    pub issues: Vec<ImportIssue>,
    /// True when a scan/copy phase was cancelled before completion.
    pub cancelled: bool,
    /// Existing catalogue path spellings, used for destination previews.
    existing_paths: Vec<PathBuf>,
}

impl ImportPlan {
    /// Changes storage policy and recomputes destination previews.
    pub fn set_storage(&mut self, storage: ImportStorage) {
        self.storage = storage;
        self.recompute_destinations();
    }

    /// Sets every eligible exact-path duplicate to update-existing.
    pub fn bulk_update_existing(&mut self) {
        for candidate in &mut self.candidates {
            if candidate.status.is_exact_path() {
                candidate.decision = ImportDecision::UpdateExisting;
            }
        }
    }

    /// Skips every candidate.
    pub fn bulk_skip(&mut self) {
        for candidate in &mut self.candidates {
            candidate.decision = ImportDecision::Skip;
        }
    }

    /// Imports every non-exact-path candidate separately and skips exact
    /// path duplicates unless they already opted into update-existing.
    pub fn bulk_import_separate(&mut self) {
        for candidate in &mut self.candidates {
            if candidate.status.is_exact_path() {
                if candidate.decision == ImportDecision::ImportNew {
                    candidate.decision = ImportDecision::Skip;
                }
            } else {
                candidate.decision = ImportDecision::ImportNew;
            }
        }
    }

    /// Number of candidates that will produce a new record.
    #[must_use]
    pub fn new_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.decision == ImportDecision::ImportNew)
            .count()
    }

    /// Number of candidates that will update existing tracks.
    #[must_use]
    pub fn update_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.decision == ImportDecision::UpdateExisting)
            .count()
    }

    /// Number of candidates deliberately skipped.
    #[must_use]
    pub fn skip_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.decision == ImportDecision::Skip)
            .count()
    }

    /// Whether the reviewed plan can be committed as-is.
    #[must_use]
    pub fn can_import(&self) -> bool {
        !self.cancelled
            && (self.new_count() > 0 || self.update_count() > 0)
            && self.candidates.iter().all(|candidate| {
                candidate.decision != ImportDecision::ImportNew
                    || (candidate.storage_error.is_none() && candidate.validation_error().is_none())
            })
    }

    /// First blocking reason for the reviewed plan, if any.
    #[must_use]
    pub fn blocking_reason(&self) -> Option<String> {
        self.candidates
            .iter()
            .find(|candidate| candidate.decision == ImportDecision::ImportNew)
            .and_then(|candidate| {
                candidate
                    .validation_error()
                    .or_else(|| candidate.storage_error.clone())
            })
    }

    fn recompute_destinations(&mut self) {
        let mut reserved: HashSet<PathBuf> = self.existing_paths.iter().cloned().collect();
        for candidate in &mut self.candidates {
            if candidate.decision != ImportDecision::ImportNew {
                candidate.set_destination(candidate.destination.clone(), None);
                continue;
            }
            if candidate.source_path.starts_with(&self.library_root) {
                candidate.set_destination(Some(candidate.source_path.clone()), None);
                continue;
            }
            match self.storage {
                ImportStorage::Reference => candidate.set_destination(
                    None,
                    Some(
                        "External source; enable Copy into the library or move the file inside \
                         the library first."
                            .to_owned(),
                    ),
                ),
                ImportStorage::Copy => {
                    let destination = preview_copy_path(
                        &self.library_root,
                        &candidate.source_path,
                        &mut reserved,
                    );
                    candidate.set_destination(Some(destination), None);
                }
            }
        }
    }
}

/// Background preparation request. All values are owned so it can move to a
/// worker/task without borrowing the live controller.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// Selected files/folders.
    pub sources: Vec<PathBuf>,
    /// Whether folder sources include descendants.
    pub recursive: bool,
    /// Initial storage policy.
    pub storage: ImportStorage,
    /// Portable library root.
    pub library_root: PathBuf,
    /// Existing library tracks (for duplicate/metadata comparison).
    pub existing_tracks: Vec<(TrackId, Track)>,
    /// Existing track → artist relationships.
    pub existing_artists: Vec<(TrackId, ArtistId)>,
}

/// One file ready for the catalogue after the copy phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImportItem {
    /// Index into [`ImportPlan::candidates`].
    pub index: usize,
    /// Final path to catalogue (or to update), if file preparation succeeded.
    pub final_path: Option<PathBuf>,
    /// Whether this run created a copy.
    pub copied: bool,
    /// Per-item failure reason.
    pub error: Option<String>,
}

/// Result of the background copy phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreparedImport {
    /// Per-candidate preparation results.
    pub items: Vec<PreparedImportItem>,
    /// Files created by this run (removed if the run is cancelled).
    pub created_files: Vec<PathBuf>,
    /// True when the user cancelled before catalogue publication.
    pub cancelled: bool,
}

/// Per-candidate outcome of the catalogue apply phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedImportItem {
    /// Index into [`ImportPlan::candidates`].
    pub index: usize,
    /// New or updated track ID.
    pub track_id: Option<TrackId>,
    /// Final state.
    pub state: ImportItemState,
    /// Detail/error line.
    pub message: String,
}

/// Result of publishing reviewed imports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportApplyReport {
    /// New tracks inserted.
    pub imported: usize,
    /// Existing tracks updated.
    pub updated: usize,
    /// Candidates skipped by choice.
    pub skipped: usize,
    /// Candidates that could not be imported.
    pub failed: usize,
    /// Per-item outcomes.
    pub items: Vec<AppliedImportItem>,
}

// ------------------------------------------------------------- progress ---

fn update_progress(
    progress: &Mutex<ImportProgress>,
    stage: ImportStage,
    done: usize,
    total: usize,
    message: impl Into<String>,
) {
    if let Ok(mut progress) = progress.lock() {
        progress.stage = stage;
        progress.done = done;
        progress.total = total;
        progress.message = message.into();
    }
}

fn set_progress_item(progress: &Mutex<ImportProgress>, index: usize, state: ImportItemState) {
    if let Ok(mut progress) = progress.lock()
        && let Some(item) = progress.items.get_mut(index)
    {
        *item = state;
    }
}

fn resize_progress_items(progress: &Mutex<ImportProgress>, count: usize) {
    if let Ok(mut progress) = progress.lock() {
        progress.items.resize(count, ImportItemState::Pending);
    }
}

fn is_cancelled(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Acquire)
}

// ------------------------------------------------------------ discovery ---

fn collect_sources(
    sources: &[PathBuf],
    recursive: bool,
    cancel: &AtomicBool,
    progress: &Mutex<ImportProgress>,
    issues: &mut Vec<ImportIssue>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<PathBuf> = sources.iter().rev().cloned().collect();

    while let Some(source) = stack.pop() {
        if is_cancelled(cancel) {
            break;
        }
        let metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(err) => {
                issues.push(ImportIssue {
                    path: source,
                    reason: format!("could not inspect the source: {err}"),
                });
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            issues.push(ImportIssue {
                path: source,
                reason: "symbolic links are never imported".to_owned(),
            });
            continue;
        }
        if metadata.is_file() {
            if is_supported_audio_path(&source) {
                let canonical = std::fs::canonicalize(&source).unwrap_or(source);
                if seen.insert(canonical.clone()) {
                    candidates.push(canonical);
                    if let Ok(mut progress) = progress.lock() {
                        progress.message = format!(
                            "Discovering files… {} found",
                            progress.done + candidates.len()
                        );
                    }
                }
            } else {
                issues.push(ImportIssue {
                    path: source,
                    reason: "unsupported or missing audio extension".to_owned(),
                });
            }
            continue;
        }
        if !metadata.is_dir() {
            issues.push(ImportIssue {
                path: source,
                reason: "not a regular file or directory".to_owned(),
            });
            continue;
        }

        let mut children: Vec<PathBuf> = Vec::new();
        match std::fs::read_dir(&source) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.file_name().and_then(|name| name.to_str()) == Some(FLUX_DIR_NAME) {
                        continue;
                    }
                    children.push(path);
                }
                children.sort();
            }
            Err(err) => {
                issues.push(ImportIssue {
                    path: source,
                    reason: format!("could not read directory: {err}"),
                });
                continue;
            }
        }

        for child in children {
            let child_metadata = match std::fs::symlink_metadata(&child) {
                Ok(metadata) => metadata,
                Err(err) => {
                    issues.push(ImportIssue {
                        path: child,
                        reason: format!("could not inspect entry: {err}"),
                    });
                    continue;
                }
            };
            if child_metadata.file_type().is_symlink() {
                issues.push(ImportIssue {
                    path: child,
                    reason: "symbolic links are never imported".to_owned(),
                });
            } else if child_metadata.is_dir() {
                if recursive {
                    stack.push(child);
                }
            } else if child_metadata.is_file() {
                if is_supported_audio_path(&child) {
                    let canonical = std::fs::canonicalize(&child).unwrap_or(child);
                    if seen.insert(canonical.clone()) {
                        candidates.push(canonical);
                    }
                } else {
                    issues.push(ImportIssue {
                        path: child,
                        reason: "unsupported or missing audio extension".to_owned(),
                    });
                }
            }
        }

        if let Ok(mut progress) = progress.lock() {
            progress.message = format!("Discovering files… {} found", candidates.len());
        }
    }

    candidates.sort();
    candidates
}

fn probe_candidates(
    paths: Vec<PathBuf>,
    cancel: &AtomicBool,
    progress: &Mutex<ImportProgress>,
    issues: &mut Vec<ImportIssue>,
) -> Vec<(PathBuf, Track)> {
    let total = paths.len();
    resize_progress_items(progress, total);
    update_progress(
        progress,
        ImportStage::ReadingMetadata,
        0,
        total,
        "Inferring metadata from filenames and folders…",
    );
    let mut candidates = Vec::with_capacity(total);

    for (index, path) in paths.into_iter().enumerate() {
        if is_cancelled(cancel) {
            break;
        }
        set_progress_item(progress, index, ImportItemState::Scanning);
        if let Some(inferred) = crate::infer_canonical_import(&path) {
            let mut track = Track::new(inferred.title, TrackSource::local(path.clone()));
            track = track.with_artist(inferred.artist_name);
            if let Some(album) = inferred.album_title {
                track = track.with_album(album);
            }
            if let Some(number) = inferred.track_number {
                track = track.with_track_number(number);
            }
            if let Some(disc) = inferred.disc_number {
                track = track.with_disc_number(disc);
            }
            candidates.push((path, track));
            set_progress_item(progress, index, ImportItemState::Ready);
        } else {
            issues.push(ImportIssue {
                path,
                reason: "the filename cannot be used for metadata inference".to_owned(),
            });
            set_progress_item(progress, index, ImportItemState::Failed);
        }
        update_progress(
            progress,
            ImportStage::ReadingMetadata,
            index + 1,
            total,
            format!("Inferring metadata… {}/{}", index + 1, total),
        );
    }
    candidates
}

fn format_label(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_uppercase())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "AUDIO".to_owned())
}

fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_owned())
}

fn normalized(value: Option<&str>) -> String {
    value.map(str::trim).unwrap_or_default().to_lowercase()
}

fn same_metadata(left: &Track, right: &Track) -> bool {
    normalized(Some(left.title())) == normalized(Some(right.title()))
        && normalized(left.artist()) == normalized(right.artist())
        && normalized(left.album()) == normalized(right.album())
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

fn content_hash(path: &Path) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        buffer[..read].hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn existing_path_map(existing: &[(TrackId, Track)]) -> HashMap<PathBuf, TrackId> {
    let mut map = HashMap::new();
    for (id, track) in existing {
        let path = std::fs::canonicalize(track.source().path())
            .unwrap_or_else(|_| track.source().path().to_path_buf());
        map.insert(path, *id);
    }
    map
}

fn base_for_existing(
    existing: &[(TrackId, Track)],
    artist_links: &[(TrackId, ArtistId)],
    id: TrackId,
) -> Option<(Track, Option<ArtistId>)> {
    let track = existing
        .iter()
        .find(|(track_id, _)| *track_id == id)
        .map(|(_, track)| track.clone())?;
    let artist = artist_links
        .iter()
        .find(|(track_id, _)| *track_id == id)
        .map(|(_, artist)| *artist);
    Some((track, artist))
}

fn make_candidate(
    source_path: PathBuf,
    source_track: Track,
    status: CandidateStatus,
    existing: &[(TrackId, Track)],
    artist_links: &[(TrackId, ArtistId)],
) -> ImportCandidate {
    let base = match &status {
        CandidateStatus::ExistingPath(id) => base_for_existing(existing, artist_links, *id),
        _ => None,
    };
    let (base_track, base_artist_id) = base.unwrap_or_else(|| (source_track.clone(), None));
    let base_title = base_track.title().to_owned();
    let base_artist_name = base_track.artist().map(ToOwned::to_owned);
    let base_album = base_track.album().map(ToOwned::to_owned);
    let base_track_number = base_track.track_number();
    let base_disc_number = base_track.disc_number();
    let decision = match status {
        CandidateStatus::ExistingPath(_) | CandidateStatus::SameContent { .. } => {
            ImportDecision::Skip
        }
        CandidateStatus::New | CandidateStatus::SimilarMetadata(_) => ImportDecision::ImportNew,
    };
    let metadata_incomplete = source_track.artist().is_none()
        || source_track.album().is_none()
        || source_track.title() == filename_stem(&source_path);
    let base_credits = vec![ImportCredit::new(
        base_artist_id,
        base_artist_name
            .clone()
            .unwrap_or_else(|| "Unknown Artist".to_owned()),
        false,
    )];
    ImportCandidate {
        source_path: source_path.clone(),
        source_track,
        file_size: file_size(&source_path).unwrap_or(0),
        format: format_label(&source_path),
        status,
        decision,
        destination: None,
        storage_error: None,
        title: base_title.clone(),
        artist_name: base_artist_name.clone(),
        album: base_album.clone(),
        track_number: base_track_number,
        disc_number: base_disc_number,
        artist_id: base_artist_id,
        credits: base_credits.clone(),
        owner_index: 0,
        base_credits,
        base_title,
        base_artist_name,
        base_album,
        base_track_number,
        base_disc_number,
        base_artist_id,
        metadata_incomplete,
    }
}

fn duplicate_status(
    path: &Path,
    track: &Track,
    existing: &[(TrackId, Track)],
    path_map: &HashMap<PathBuf, TrackId>,
    hash_cache: &mut HashMap<TrackId, Option<u64>>,
    candidate_hashes: &mut HashMap<u64, PathBuf>,
) -> CandidateStatus {
    if let Some(id) = path_map.get(path) {
        return CandidateStatus::ExistingPath(*id);
    }

    if let Some(hash) = content_hash(path) {
        if let Some(other) = candidate_hashes.insert(hash, path.to_path_buf()) {
            return CandidateStatus::SameContent {
                track_id: None,
                other_source: Some(other),
            };
        }
        let size = file_size(path).unwrap_or(0);
        for (id, existing_track) in existing {
            if file_size(existing_track.source().path()) != Some(size) {
                continue;
            }
            let existing_hash = hash_cache
                .entry(*id)
                .or_insert_with(|| content_hash(existing_track.source().path()));
            if *existing_hash == Some(hash) {
                return CandidateStatus::SameContent {
                    track_id: Some(*id),
                    other_source: None,
                };
            }
        }
    }

    if let Some((id, _)) = existing
        .iter()
        .find(|(_, existing_track)| same_metadata(track, existing_track))
    {
        return CandidateStatus::SimilarMetadata(*id);
    }
    CandidateStatus::New
}

/// Discovers and prepares a review plan without modifying the library.
///
/// Progress is updated through `progress`; setting `cancel` stops discovery
/// or metadata reading at the next safe boundary and marks the plan
/// cancelled. All filesystem errors are itemized in the returned plan.
#[must_use]
pub fn prepare_import_plan(
    request: ImportRequest,
    progress: Arc<Mutex<ImportProgress>>,
    cancel: Arc<AtomicBool>,
) -> ImportPlan {
    let ImportRequest {
        sources,
        recursive,
        storage,
        library_root,
        existing_tracks,
        existing_artists,
    } = request;
    let mut issues = Vec::new();
    update_progress(
        &progress,
        ImportStage::Discovering,
        0,
        0,
        "Discovering files…",
    );
    let paths = collect_sources(&sources, recursive, &cancel, &progress, &mut issues);
    let mut was_cancelled = is_cancelled(&cancel);
    let mut candidates = if was_cancelled {
        Vec::new()
    } else {
        probe_candidates(paths, &cancel, &progress, &mut issues)
    };
    was_cancelled |= is_cancelled(&cancel);

    let mut plan = ImportPlan {
        storage,
        library_root: library_root.clone(),
        recursive,
        candidates: Vec::new(),
        issues,
        cancelled: was_cancelled,
        existing_paths: existing_tracks
            .iter()
            .map(|(_, track)| track.source().path().to_path_buf())
            .collect(),
    };
    if plan.cancelled {
        update_progress(
            &progress,
            ImportStage::Cancelled,
            0,
            0,
            "Import cancelled before any catalogue changes.",
        );
        return plan;
    }

    let total = candidates.len();
    resize_progress_items(&progress, total);
    update_progress(
        &progress,
        ImportStage::DetectingDuplicates,
        0,
        total,
        "Checking duplicates and conflicts…",
    );
    let path_map = existing_path_map(&existing_tracks);
    let mut hash_cache: HashMap<TrackId, Option<u64>> = HashMap::new();
    let mut candidate_hashes: HashMap<u64, PathBuf> = HashMap::new();
    for (index, (path, track)) in candidates.drain(..).enumerate() {
        if is_cancelled(&cancel) {
            plan.cancelled = true;
            break;
        }
        let status = duplicate_status(
            &path,
            &track,
            &existing_tracks,
            &path_map,
            &mut hash_cache,
            &mut candidate_hashes,
        );
        plan.candidates.push(make_candidate(
            path,
            track,
            status,
            &existing_tracks,
            &existing_artists,
        ));
        set_progress_item(&progress, index, ImportItemState::Ready);
        update_progress(
            &progress,
            ImportStage::DetectingDuplicates,
            index + 1,
            total,
            format!("Checking duplicates… {}/{}", index + 1, total),
        );
    }
    plan.recompute_destinations();
    if plan.cancelled {
        update_progress(
            &progress,
            ImportStage::Cancelled,
            plan.candidates.len(),
            total,
            "Import cancelled before any catalogue changes.",
        );
    } else {
        update_progress(
            &progress,
            ImportStage::Completed,
            total,
            total,
            format!("Ready to import {} file(s).", total),
        );
    }
    plan
}

// ------------------------------------------------------------- copy phase ---

fn unique_destination(root: &Path, source: &Path, reserved: &mut HashSet<PathBuf>) -> PathBuf {
    let file_name = source
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| std::ffi::OsString::from("imported-audio"));
    let source_path = Path::new(&file_name);
    let stem = source_path.file_stem().unwrap_or(&file_name);
    let extension = source_path.extension();
    for attempt in 0..10_000u32 {
        let name = if attempt == 0 {
            file_name.clone()
        } else {
            let mut name = std::ffi::OsString::from(stem);
            name.push(format!(" ({attempt})"));
            if let Some(extension) = extension {
                name.push(".");
                name.push(extension);
            }
            name
        };
        let candidate = root.join(name);
        if !candidate.exists() && reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
    root.join("imported-audio")
}

fn preview_copy_path(root: &Path, source: &Path, reserved: &mut HashSet<PathBuf>) -> PathBuf {
    unique_destination(root, source, reserved)
}

fn copy_reviewed_file(
    root: &Path,
    source: &Path,
    preferred: Option<&Path>,
) -> Result<PathBuf, String> {
    let mut reserved = HashSet::new();
    let destination = match preferred {
        Some(path) if path.starts_with(root) && !path.exists() => path.to_path_buf(),
        _ => unique_destination(root, source, &mut reserved),
    };
    let mut input = std::fs::File::open(source)
        .map_err(|err| format!("could not read {}: {err}", source.display()))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|err| format!("could not create {}: {err}", destination.display()))?;
    if let Err(err) = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all()) {
        let _ = std::fs::remove_file(&destination);
        return Err(format!("copy failed: {err}"));
    }
    Ok(destination)
}

/// Copies selected files, with cooperative cancellation. No catalogue
/// changes occur here. If cancelled, copies created by this run are removed.
#[must_use]
pub fn prepare_import_files(
    plan: &ImportPlan,
    progress: Arc<Mutex<ImportProgress>>,
    cancel: Arc<AtomicBool>,
) -> PreparedImport {
    prepare_import_files_with_hook(plan, progress, cancel, |_| {})
}

fn prepare_import_files_with_hook<F>(
    plan: &ImportPlan,
    progress: Arc<Mutex<ImportProgress>>,
    cancel: Arc<AtomicBool>,
    mut after_item: F,
) -> PreparedImport
where
    F: FnMut(usize),
{
    let selected: Vec<usize> = plan
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.decision == ImportDecision::ImportNew)
        .map(|(index, _)| index)
        .collect();
    let total = selected.len();
    resize_progress_items(&progress, plan.candidates.len());
    update_progress(
        &progress,
        ImportStage::Copying,
        0,
        total,
        if plan.storage == ImportStorage::Copy {
            "Copying selected files…"
        } else {
            "Checking referenced paths…"
        },
    );
    let mut prepared = PreparedImport::default();
    for (position, index) in selected.into_iter().enumerate() {
        if is_cancelled(&cancel) {
            prepared.cancelled = true;
            break;
        }
        let candidate = &plan.candidates[index];
        set_progress_item(&progress, index, ImportItemState::Importing);
        let canonical = plan.library_root.join("catalog.flux").is_file();
        let result = if canonical {
            std::fs::metadata(&candidate.source_path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|_| candidate.source_path.clone())
                .ok_or_else(|| {
                    format!(
                        "source is missing or not a regular file: {}",
                        candidate.source_path.display()
                    )
                })
        } else if candidate.source_path.starts_with(&plan.library_root) {
            Ok(candidate.source_path.clone())
        } else if plan.storage == ImportStorage::Reference {
            Err("external source cannot be referenced; choose Copy into the library".to_owned())
        } else {
            copy_reviewed_file(
                &plan.library_root,
                &candidate.source_path,
                candidate.destination.as_deref(),
            )
        };
        let item = match result {
            Ok(path) => {
                let copied = path != candidate.source_path;
                if copied {
                    prepared.created_files.push(path.clone());
                }
                set_progress_item(&progress, index, ImportItemState::Ready);
                PreparedImportItem {
                    index,
                    final_path: Some(path),
                    copied,
                    error: None,
                }
            }
            Err(error) => {
                set_progress_item(&progress, index, ImportItemState::Failed);
                PreparedImportItem {
                    index,
                    final_path: None,
                    copied: false,
                    error: Some(error),
                }
            }
        };
        prepared.items.push(item);
        after_item(index);
        update_progress(
            &progress,
            ImportStage::Copying,
            position + 1,
            total,
            format!("Preparing files… {}/{}", position + 1, total),
        );
    }

    if prepared.cancelled {
        for path in &prepared.created_files {
            let _ = std::fs::remove_file(path);
        }
        update_progress(
            &progress,
            ImportStage::Cancelled,
            0,
            total,
            "Import cancelled; temporary copies were removed and the catalogue was not changed.",
        );
    } else {
        update_progress(
            &progress,
            ImportStage::Completed,
            total,
            total,
            "Files prepared; catalogue changes are ready to commit.",
        );
    }
    prepared
}

// ---------------------------------------------------------- apply phase ---

fn track_from_source(source: &Track, path: PathBuf) -> Track {
    let mut track = Track::new(source.title(), TrackSource::local(path));
    if let Some(artist) = source.artist() {
        track = track.with_artist(artist);
    }
    if let Some(album) = source.album() {
        track = track.with_album(album);
    }
    if let Some(number) = source.track_number() {
        track = track.with_track_number(number);
    }
    if let Some(disc) = source.disc_number() {
        track = track.with_disc_number(disc);
    }
    if let Some(duration) = source.duration() {
        track = track.with_duration(duration);
    }
    track
}

fn set_optional_text(overrides: &mut MetadataOverrides, current: Option<&str>, base: Option<&str>) {
    if current == base {
        return;
    }
    match current {
        Some(value) => overrides.set_album(value.to_owned()),
        None => overrides.clear_album(),
    }
}

fn set_optional_number(
    overrides: &mut MetadataOverrides,
    current: Option<u32>,
    base: Option<u32>,
    set: fn(&mut MetadataOverrides, u32),
    clear: fn(&mut MetadataOverrides),
) {
    if current == base {
        return;
    }
    match current {
        Some(value) => set(overrides, value),
        None => clear(overrides),
    }
}

fn apply_candidate_metadata(
    store: &mut PortableStore,
    track_id: TrackId,
    candidate: &ImportCandidate,
) -> Result<(), String> {
    let Some(stored) = store.get(track_id) else {
        return Err(format!("track {track_id} is not in the portable catalogue"));
    };
    let mut overrides = stored.overrides().clone();
    let (base_title, base_artist, base_album, base_track_number, base_disc_number) =
        candidate.base_fields();

    let title = candidate.title.trim();
    if title.is_empty() {
        overrides.revert_title();
    } else if title != base_title {
        overrides.set_title(title.to_owned());
    }

    set_optional_text(&mut overrides, candidate.album.as_deref(), base_album);
    set_optional_number(
        &mut overrides,
        candidate.track_number,
        base_track_number,
        MetadataOverrides::set_track_number,
        MetadataOverrides::clear_track_number,
    );
    set_optional_number(
        &mut overrides,
        candidate.disc_number,
        base_disc_number,
        MetadataOverrides::set_disc_number,
        MetadataOverrides::clear_disc_number,
    );

    let artist_name = candidate.artist_name.as_deref();
    let artist_changed =
        artist_name != base_artist || candidate.artist_id != candidate.base_artist_id;
    if artist_changed {
        let desired = match (candidate.artist_id, artist_name) {
            (Some(id), _) => Some(id),
            (None, Some(name)) => Some(
                store
                    .resolve_or_create_artist(name)
                    .map_err(|err| format!("could not resolve artist {name:?}: {err}"))?,
            ),
            (None, None) => None,
        };
        if desired.is_some() {
            overrides.revert_artist();
        } else {
            overrides.clear_artist();
        }
        store
            .set_track_artist(track_id, desired)
            .map_err(|err| format!("could not assign artist: {err}"))?;
    } else if candidate.artist_id.is_some() && candidate.base_artist_id.is_none() {
        overrides.revert_artist();
        store
            .set_track_artist(track_id, candidate.artist_id)
            .map_err(|err| format!("could not assign artist: {err}"))?;
    } else if candidate.artist_id.is_none() && base_artist.is_some() && artist_name.is_none() {
        overrides.clear_artist();
        store
            .set_track_artist(track_id, None)
            .map_err(|err| format!("could not clear artist: {err}"))?;
    }

    store
        .set_overrides(track_id, overrides)
        .map_err(|err| format!("could not store metadata overrides: {err}"))?;
    Ok(())
}

fn resolve_canonical_credits(
    controller: &mut Controller,
    candidate: &ImportCandidate,
) -> Result<(Vec<ArtistId>, ArtistId), String> {
    let credits = candidate.credits();
    if credits.is_empty() {
        return Err("At least one credited artist is required.".to_owned());
    }
    let mut ids = Vec::with_capacity(credits.len());
    for credit in credits {
        let id = if let Some(id) = credit.artist_id {
            if controller.artist(id).is_none() {
                return Err(format!("Artist {id} is not in this library."));
            }
            id
        } else {
            let name = credit.name.trim();
            if name.is_empty() {
                return Err("Every credit needs a name or an existing artist.".to_owned());
            }
            let matches = controller
                .artists()
                .into_iter()
                .filter(|artist| artist.name() == name)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [artist] => artist.id(),
                [] => controller.create_artist(name)?,
                _ => {
                    return Err(format!(
                        "Several artists are named {name:?}; select the intended identity explicitly."
                    ));
                }
            }
        };
        if ids.contains(&id) {
            return Err("The same artist cannot appear twice in the credit list.".to_owned());
        }
        ids.push(id);
    }
    if candidate.owner_index() >= ids.len() {
        return Err("Select a physical owner from the credited artists.".to_owned());
    }
    let owner = ids[candidate.owner_index()];
    Ok((ids, owner))
}

fn apply_import_plan_canonical(
    controller: &mut Controller,
    plan: &ImportPlan,
) -> Result<ImportApplyReport, String> {
    use crate::TrackMetadataEdit;
    use flux_library::canonical::CanonicalImportedTrack;

    if plan.storage == ImportStorage::Reference {
        return Err(
            "Canonical libraries copy every import; reference-only storage is disabled.".to_owned(),
        );
    }
    if controller.is_scanning() {
        return Err("A library scan is running; import after it finishes.".to_owned());
    }
    let mut report = ImportApplyReport::default();
    for (index, candidate) in plan.candidates.iter().enumerate() {
        match candidate.decision {
            ImportDecision::Skip => {
                report.skipped += 1;
                report.items.push(AppliedImportItem {
                    index,
                    track_id: candidate.status.matched_track(),
                    state: ImportItemState::Skipped,
                    message: "Skipped".to_owned(),
                });
            }
            ImportDecision::UpdateExisting => {
                let Some(id) = candidate.status.matched_track() else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "No existing track to update.".to_owned(),
                    });
                    continue;
                };
                let artist_id = candidate
                    .artist_id
                    .or_else(|| controller.track_artist_id(id));
                let result = controller.edit_track_metadata(
                    id,
                    TrackMetadataEdit {
                        title: Some(candidate.title.clone()),
                        artist_id,
                        album: candidate.album.clone(),
                        track_number: candidate.track_number,
                        disc_number: candidate.disc_number,
                    },
                );
                match result {
                    Ok(()) => {
                        let current_owner = controller.track_artist_id(id);
                        match resolve_canonical_credits(controller, candidate) {
                            Ok((credits, owner)) if current_owner == Some(owner) => {
                                if let Err(message) =
                                    controller.update_track_credits(id, credits)
                                {
                                    report.items.push(AppliedImportItem {
                                        index,
                                        track_id: Some(id),
                                        state: ImportItemState::Failed,
                                        message,
                                    });
                                    continue;
                                }
                                report.updated += 1;
                                report.items.push(AppliedImportItem {
                                    index,
                                    track_id: Some(id),
                                    state: ImportItemState::Imported,
                                    message: "Updated existing track".to_owned(),
                                });
                            }
                            Ok(_) => report.items.push(AppliedImportItem {
                                index,
                                track_id: Some(id),
                                state: ImportItemState::Failed,
                                message: "Changing physical ownership requires an ownership transfer, not an import update.".to_owned(),
                            }),
                            Err(message) => report.items.push(AppliedImportItem {
                                index,
                                track_id: Some(id),
                                state: ImportItemState::Failed,
                                message,
                            }),
                        }
                    }
                    Err(message) => report.items.push(AppliedImportItem {
                        index,
                        track_id: Some(id),
                        state: ImportItemState::Failed,
                        message,
                    }),
                }
            }
            ImportDecision::ImportNew => {
                let resolved_credits = resolve_canonical_credits(controller, candidate);
                let (credited_artist_ids, owner_id) = match resolved_credits {
                    Ok(value) => value,
                    Err(message) => {
                        report.items.push(AppliedImportItem {
                            index,
                            track_id: None,
                            state: ImportItemState::Failed,
                            message,
                        });
                        continue;
                    }
                };
                let Some(owner_name) = controller
                    .artist(owner_id)
                    .map(|artist| artist.name().to_owned())
                else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "The selected physical owner is unavailable.".to_owned(),
                    });
                    continue;
                };
                let Some(album_title) = candidate
                    .album
                    .as_deref()
                    .map(str::trim)
                    .filter(|album| !album.is_empty())
                else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "Album is required before importing.".to_owned(),
                    });
                    continue;
                };
                let Some(store) = controller.canonical_store.as_mut() else {
                    return Err("The canonical library is unavailable.".to_owned());
                };
                let imported = CanonicalImportedTrack {
                    source: candidate.source_path.clone(),
                    title: candidate.title.clone(),
                    artist_id: Some(owner_id),
                    artist_name: owner_name,
                    credited_artist_ids,
                    album_title: Some(album_title.to_owned()),
                    release_type: flux_library::canonical::ReleaseType::Album,
                    track_number: candidate.track_number,
                    disc_number: candidate.disc_number,
                    duration_ms: None,
                };
                match store.import_track(imported) {
                    Ok(id) => {
                        report.imported += 1;
                        report.items.push(AppliedImportItem {
                            index,
                            track_id: Some(id),
                            state: ImportItemState::Imported,
                            message: "Imported".to_owned(),
                        });
                    }
                    Err(err) => report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: err.to_string(),
                    }),
                }
            }
        }
    }
    controller.refresh_from_canonical();
    controller.sync_default_queue();
    controller.outbox.push(ControllerEvent::LibraryChanged);
    report.failed = report
        .items
        .iter()
        .filter(|item| item.state == ImportItemState::Failed)
        .count();
    report.items.sort_by_key(|item| item.index);
    controller.report_status(format!(
        "Canonical import complete: {} new track(s), {} updated, {} skipped, {} failed.",
        report.imported, report.updated, report.skipped, report.failed
    ));
    Ok(report)
}

/// Applies reviewed decisions and prepared files. This is the only point
/// where import changes enter the catalogue. New tracks are inserted into
/// the runtime library, synced to the portable store, given overrides/artist
/// links, and published through the store's atomic commit protocol.
///
/// # Errors
///
/// A user-facing message when the backend is legacy/read-only/unavailable,
/// a prepared file is missing, the library has a new path conflict, or the
/// catalogue publish fails. Files copied by the preparation phase are never
/// deleted here; a failed publish leaves them as reported recoverable
/// leftovers.
impl Controller {
    /// Publishes a reviewed import plan after [`prepare_import_files`] has
    /// completed. See [`apply_import_plan`] for the exact guarantees.
    ///
    /// # Errors
    ///
    /// The same user-facing failures as [`apply_import_plan`].
    pub fn apply_import_plan(
        &mut self,
        plan: &ImportPlan,
        prepared: &PreparedImport,
    ) -> Result<ImportApplyReport, String> {
        apply_import_plan(self, plan, prepared)
    }
}

/// Applies reviewed decisions and prepared files. This is the only point
/// where import changes enter the catalogue. New tracks are inserted into
/// the runtime library, synced to the portable store, given overrides/artist
/// links, and published through the store's atomic commit protocol.
///
/// # Errors
///
/// A user-facing message when the backend is legacy/read-only/unavailable,
/// a prepared file is missing, the library has a new path conflict, or the
/// catalogue publish fails. Files copied by the preparation phase are never
/// deleted here; a failed publish leaves them as reported recoverable
/// leftovers.
pub fn apply_import_plan(
    controller: &mut Controller,
    plan: &ImportPlan,
    prepared: &PreparedImport,
) -> Result<ImportApplyReport, String> {
    if plan.cancelled || prepared.cancelled {
        return Err("The import was cancelled; no catalogue changes were made.".to_owned());
    }
    if controller.storage_mode() == StorageMode::Canonical {
        return apply_import_plan_canonical(controller, plan);
    }
    if controller.storage_mode() != StorageMode::Portable {
        return Err(
            "Importing requires a portable library. Create or open one in Library setup."
                .to_owned(),
        );
    }
    if controller.is_scanning() {
        return Err("A library scan is running; import after it finishes.".to_owned());
    }
    if controller.portable_recovery().is_some() {
        return Err(crate::PORTABLE_RECOVERY_READ_ONLY.to_owned());
    }
    let Some(library) = controller.library.as_mut() else {
        return Err("A library scan is running; import after it finishes.".to_owned());
    };
    let Some(store) = controller.portable_store.as_mut() else {
        return Err(crate::PORTABLE_STORE_UNAVAILABLE.to_owned());
    };

    let mut report = ImportApplyReport::default();
    let mut inserts: Vec<(usize, TrackId)> = Vec::new();
    let mut updates: Vec<(usize, TrackId)> = Vec::new();
    let prepared_by_index: HashMap<usize, &PreparedImportItem> = prepared
        .items
        .iter()
        .map(|item| (item.index, item))
        .collect();

    for (index, candidate) in plan.candidates.iter().enumerate() {
        match candidate.decision {
            ImportDecision::Skip => {
                report.skipped += 1;
                report.items.push(AppliedImportItem {
                    index,
                    track_id: candidate.status.matched_track(),
                    state: ImportItemState::Skipped,
                    message: "Skipped".to_owned(),
                });
            }
            ImportDecision::UpdateExisting => {
                let Some(id) = candidate.status.matched_track() else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "No existing track to update.".to_owned(),
                    });
                    continue;
                };
                if store.get(id).is_none() {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: Some(id),
                        state: ImportItemState::Failed,
                        message: "The existing track is no longer in the catalogue.".to_owned(),
                    });
                    continue;
                }
                updates.push((index, id));
            }
            ImportDecision::ImportNew => {
                let Some(item) = prepared_by_index.get(&index) else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "The file was not prepared.".to_owned(),
                    });
                    continue;
                };
                let Some(final_path) = item.final_path.as_ref() else {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: item
                            .error
                            .clone()
                            .unwrap_or_else(|| "The file could not be prepared.".to_owned()),
                    });
                    continue;
                };
                if library.id_for_path(final_path).is_some() {
                    report.items.push(AppliedImportItem {
                        index,
                        track_id: None,
                        state: ImportItemState::Failed,
                        message: "Another catalogue record now owns this path.".to_owned(),
                    });
                    continue;
                }
                let track = track_from_source(candidate.source_track(), final_path.clone());
                let id = library.insert(track);
                inserts.push((index, id));
            }
        }
    }

    if inserts.is_empty() && updates.is_empty() {
        report.failed = report
            .items
            .iter()
            .filter(|item| item.state == ImportItemState::Failed)
            .count();
        return Ok(report);
    }

    let sync = sync_library_to_portable_store(library, store)
        .map_err(|err| format!("Could not sync imported tracks into the catalogue: {err}"))?;
    if !sync.skipped.is_empty() {
        return Err(format!(
            "{} imported file(s) could not be stored in the portable catalogue.",
            sync.skipped.len()
        ));
    }

    for (index, id) in inserts.iter().chain(updates.iter()) {
        let candidate = &plan.candidates[*index];
        apply_candidate_metadata(store, *id, candidate)
            .map_err(|err| format!("Could not apply reviewed metadata: {err}"))?;
        report.items.push(AppliedImportItem {
            index: *index,
            track_id: Some(*id),
            state: ImportItemState::Imported,
            message: if candidate.decision == ImportDecision::UpdateExisting {
                "Updated existing track".to_owned()
            } else {
                "Imported".to_owned()
            },
        });
    }
    store
        .commit()
        .map_err(|err| format!("Could not publish the import: {err}"))?;

    report.imported = inserts.len();
    report.updated = updates.len();
    report.failed = report
        .items
        .iter()
        .filter(|item| item.state == ImportItemState::Failed)
        .count();
    report.items.sort_by_key(|item| item.index);
    controller.refresh_from_store();
    controller.sync_default_queue();
    controller.outbox.push(ControllerEvent::LibraryChanged);
    controller.report_status(format!(
        "Import complete: {} new track(s), {} updated, {} skipped.",
        report.imported, report.updated, report.skipped
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use flux_audio::engine::EngineHandle;
    use flux_audio::output::NullControl;
    use flux_core::TrackId;

    use crate::{AudioStatus, Controller, create_portable_library};

    use super::{
        ArtistId, CandidateStatus, ImportCredit, ImportDecision, ImportItemState, ImportPlan,
        ImportProgress, ImportRequest, ImportStage, ImportStorage, prepare_import_files,
        prepare_import_plan,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-import-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
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

    fn wav(dir: &Path, name: &str, frames: usize, value: f32) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("parents");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
        let sample = (value * 32_767.0) as i32;
        for _ in 0..frames {
            writer.write_sample(sample).expect("sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    fn portable_controller(root: &Path) -> (Controller, NullControl) {
        fs::create_dir_all(root).expect("root");
        create_portable_library(root).expect("portable library");
        let (engine, control) = EngineHandle::spawn_null(48_000, 1);
        let controller = Controller::new_portable(engine, AudioStatus::Ready, root)
            .expect("open portable controller");
        (controller, control)
    }

    fn reopen_portable(root: &Path) -> (Controller, NullControl) {
        let (engine, control) = EngineHandle::spawn_null(48_000, 1);
        let controller = Controller::new_portable(engine, AudioStatus::Ready, root)
            .expect("reopen portable controller");
        (controller, control)
    }

    fn request(
        controller: &Controller,
        sources: Vec<PathBuf>,
        recursive: bool,
        storage: ImportStorage,
    ) -> ImportRequest {
        ImportRequest {
            sources,
            recursive,
            storage,
            library_root: controller.portable_root().expect("portable").to_path_buf(),
            existing_tracks: controller.tracks().to_vec(),
            existing_artists: controller
                .tracks()
                .iter()
                .filter_map(|(id, _)| controller.track_artist_id(*id).map(|artist| (*id, artist)))
                .collect(),
        }
    }

    fn plan(
        controller: &Controller,
        sources: Vec<PathBuf>,
        recursive: bool,
        storage: ImportStorage,
    ) -> (ImportPlan, Arc<Mutex<ImportProgress>>, Arc<AtomicBool>) {
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = prepare_import_plan(
            request(controller, sources, recursive, storage),
            Arc::clone(&progress),
            Arc::clone(&cancel),
        );
        (plan, progress, cancel)
    }

    fn prepare(
        plan: &ImportPlan,
        progress: Arc<Mutex<ImportProgress>>,
        cancel: Arc<AtomicBool>,
    ) -> super::PreparedImport {
        prepare_import_files(plan, progress, cancel)
    }

    #[test]
    fn folder_discovery_respects_recursive_and_ignores_duplicate_paths() {
        let dir = TempDir::new("discover");
        let root = dir.path().join("library");
        let (controller, _control) = portable_controller(&root);
        let source = dir.path().join("source");
        wav(&source, "top.wav", 441, 0.1);
        wav(&source, "nested/deep.wav", 441, 0.2);
        fs::write(source.join("notes.txt"), b"not audio").expect("notes");

        let (shallow, progress, _cancel) = plan(
            &controller,
            vec![source.clone()],
            false,
            ImportStorage::Copy,
        );
        assert!(!shallow.cancelled);
        assert_eq!(shallow.candidates.len(), 1);
        assert_eq!(shallow.candidates[0].title(), "top");
        assert!(shallow.candidates[0].metadata_incomplete());
        assert!(
            shallow
                .issues
                .iter()
                .any(|issue| issue.path().ends_with("notes.txt"))
        );
        assert_eq!(
            progress.lock().expect("progress").stage,
            ImportStage::Completed
        );

        let (deep, _progress, _cancel) = plan(
            &controller,
            vec![source.clone(), source],
            true,
            ImportStorage::Copy,
        );
        assert_eq!(deep.candidates.len(), 2, "nested files are found once");
        assert!(
            deep.candidates
                .iter()
                .any(|candidate| candidate.title() == "deep")
        );
        controller.shutdown();
    }

    #[test]
    fn exact_path_and_content_duplicates_update_without_changing_ids_or_queue() {
        let dir = TempDir::new("duplicates");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let canonical_root = root.canonicalize().expect("canonical");
        let source = wav(&canonical_root, "one.wav", 4_410, 0.2);

        let (first, progress, cancel) = plan(
            &controller,
            vec![source.clone()],
            false,
            ImportStorage::Reference,
        );
        assert_eq!(first.candidates.len(), 1);
        assert!(matches!(first.candidates[0].status(), CandidateStatus::New));
        let prepared = prepare(&first, progress, cancel);
        assert!(prepared.items[0].error.is_none());
        let applied = controller
            .apply_import_plan(&first, &prepared)
            .expect("first import");
        assert_eq!(applied.imported, 1);
        let id = controller.tracks()[0].0;
        let queue_current = controller.current_queue_entry_id();

        let (mut second, progress, cancel) = plan(
            &controller,
            vec![source.clone()],
            false,
            ImportStorage::Reference,
        );
        assert!(matches!(
            second.candidates[0].status(),
            CandidateStatus::ExistingPath(existing) if *existing == id
        ));
        assert_eq!(second.candidates[0].decision(), ImportDecision::Skip);
        assert!(!second.can_import());
        second.candidates[0].set_decision(ImportDecision::UpdateExisting);
        second.candidates[0].set_title("Updated one");
        assert!(second.can_import());
        let prepared = prepare(&second, progress, cancel);
        let applied = controller
            .apply_import_plan(&second, &prepared)
            .expect("update import");
        assert_eq!(applied.updated, 1);
        assert_eq!(controller.tracks()[0].0, id);
        assert_eq!(
            controller.track(id).map(|track| track.title()),
            Some("Updated one")
        );
        assert_eq!(controller.current_queue_entry_id(), queue_current);

        // Identical bytes at a different path are reported as potential
        // content duplicates, never silently added.
        let copy = canonical_root.join("one-copy.wav");
        fs::copy(&source, &copy).expect("copy fixture");
        let (mut content, progress, cancel) =
            plan(&controller, vec![copy], false, ImportStorage::Reference);
        assert!(matches!(
            content.candidates[0].status(),
            CandidateStatus::SameContent {
                track_id: Some(existing),
                ..
            } if *existing == id
        ));
        assert_eq!(content.candidates[0].decision(), ImportDecision::Skip);
        content.candidates[0].set_decision(ImportDecision::ImportNew);
        let prepared = prepare(&content, progress, cancel);
        let applied = controller
            .apply_import_plan(&content, &prepared)
            .expect("separate duplicate import");
        assert_eq!(applied.imported, 1);
        assert_eq!(controller.tracks().len(), 2);
        assert!(
            controller
                .tracks()
                .iter()
                .all(|(track_id, _)| *track_id == id || *track_id != id)
        );
        controller.shutdown();
    }

    #[test]
    fn reviewed_edits_copy_safely_and_survive_restart() {
        let dir = TempDir::new("reviewed");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let canonical_root = root.canonicalize().expect("canonical");
        let existing = wav(&canonical_root, "song.wav", 4_410, -0.2);
        let existing_bytes = fs::read(&existing).expect("bytes");
        let source_dir = dir.path().join("Reviewed Artist/Base album");
        let source = wav(&source_dir, "song.wav", 4_410, 0.3);
        let source_bytes = fs::read(&source).expect("source bytes");

        let (mut plan, progress, cancel) = plan(
            &controller,
            vec![source.clone()],
            false,
            ImportStorage::Copy,
        );
        assert_eq!(plan.candidates.len(), 1);
        assert!(
            plan.candidates[0]
                .destination()
                .is_some_and(|path| path.ends_with("song (1).wav")),
            "collision-safe destination preview"
        );
        plan.candidates[0].set_title("Reviewed title");
        plan.candidates[0].set_album(Some("Reviewed album".to_owned()));
        plan.candidates[0].set_artist_name(Some("Reviewed Artist".to_owned()));
        assert_eq!(plan.candidates[0].artist_name(), Some("Reviewed Artist"));
        plan.candidates[0].set_track_number(Some(2));
        let prepared = prepare(&plan, progress, cancel);
        assert_eq!(prepared.created_files.len(), 1);
        assert_eq!(fs::read(&existing).expect("existing"), existing_bytes);
        assert_eq!(fs::read(&source).expect("source"), source_bytes);
        let applied = controller
            .apply_import_plan(&plan, &prepared)
            .expect("reviewed import");
        assert_eq!(applied.imported, 1);
        let new_track = controller
            .tracks()
            .iter()
            .find(|(_, track)| track.title() == "Reviewed title")
            .map(|(id, track)| (*id, track.source().path().to_path_buf()))
            .expect("imported track");
        assert_ne!(new_track.1, existing);
        assert_eq!(
            controller.track(new_track.0).and_then(crate::Track::artist),
            Some("Reviewed Artist")
        );
        assert_eq!(
            controller.track(new_track.0).and_then(crate::Track::album),
            Some("Reviewed album")
        );
        assert_eq!(controller.artists().len(), 1);
        let artist_id = controller
            .track_artist_id(new_track.0)
            .expect("stable artist");
        let stored = controller
            .portable_store
            .as_ref()
            .and_then(|store| store.get(new_track.0))
            .expect("stored imported track");
        assert_eq!(
            stored.tags().title.as_deref(),
            Some("song"),
            "the source tag/fallback snapshot stays separate from the review edit"
        );
        assert_eq!(
            stored.overrides().title(),
            Some(Some("Reviewed title")),
            "the reviewed title is a Flux override"
        );
        assert_eq!(stored.overrides().album(), Some(Some("Reviewed album")));
        let imported_id = new_track.0;
        controller.shutdown();

        let (reopened, _control) = reopen_portable(&root);
        assert_eq!(
            reopened.track(imported_id).map(crate::Track::title),
            Some("Reviewed title")
        );
        assert_eq!(
            reopened.track(imported_id).and_then(crate::Track::artist),
            Some("Reviewed Artist")
        );
        assert_eq!(reopened.track_artist_id(imported_id), Some(artist_id));
        assert_eq!(fs::read(&source).expect("source"), source_bytes);
        reopened.shutdown();
    }

    #[test]
    fn reference_mode_blocks_external_sources_and_copy_previews_are_unique() {
        let dir = TempDir::new("storage");
        let root = dir.path().join("library");
        let (controller, _control) = portable_controller(&root);
        let source = wav(&dir.path().join("external"), "unicode-café.wav", 441, 0.1);
        let (mut plan, _progress, _cancel) =
            plan(&controller, vec![source], false, ImportStorage::Reference);
        assert!(plan.candidates[0].storage_error().is_some());
        assert!(!plan.can_import());
        plan.set_storage(ImportStorage::Copy);
        assert!(plan.can_import());
        assert!(
            plan.candidates[0]
                .destination()
                .is_some_and(|path| path.ends_with("unicode-café.wav"))
        );
        controller.shutdown();
    }

    #[test]
    fn invalid_sources_and_partial_copy_failures_are_itemized() {
        let dir = TempDir::new("partial");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let source_dir = dir.path().join("source");
        let good = wav(&source_dir, "good.wav", 441, 0.1);
        let vanished = wav(&source_dir, "vanished.wav", 441, 0.2);
        let unsupported = source_dir.join("notes.txt");
        fs::write(&unsupported, b"text").expect("notes");
        let (plan, progress, cancel) = plan(
            &controller,
            vec![
                good.clone(),
                vanished.clone(),
                unsupported,
                dir.path().join("missing.wav"),
            ],
            false,
            ImportStorage::Copy,
        );
        assert_eq!(plan.candidates.len(), 2);
        assert!(plan.issues.len() >= 2, "{:?}", plan.issues);
        fs::remove_file(&vanished).expect("vanish");
        let prepared = prepare(&plan, progress, cancel);
        assert_eq!(prepared.items.len(), 2);
        let failed = prepared
            .items
            .iter()
            .find(|item| item.error.is_some())
            .expect("one copy failure");
        assert!(failed.final_path.is_none());
        let applied = controller
            .apply_import_plan(&plan, &prepared)
            .expect("partial import");
        assert_eq!(applied.imported, 1);
        assert_eq!(applied.failed, 1);
        assert_eq!(controller.tracks().len(), 1);
        assert!(controller.tracks()[0].1.title() == "good");
        controller.shutdown();
    }

    #[test]
    fn cancellation_removes_copies_and_never_publishes_a_catalogue_entry() {
        let dir = TempDir::new("cancel");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let source = wav(&dir.path().join("source"), "cancel.wav", 441, 0.1);
        let (plan, progress, cancel) = plan(&controller, vec![source], false, ImportStorage::Copy);
        cancel.store(true, std::sync::atomic::Ordering::Release);
        let prepared = prepare_import_files(&plan, progress, cancel);
        assert!(prepared.cancelled);
        assert!(prepared.created_files.is_empty());
        assert!(controller.apply_import_plan(&plan, &prepared).is_err());
        assert_eq!(controller.tracks().len(), 0);
        controller.shutdown();
    }

    #[test]
    fn progress_items_and_stages_track_the_actual_work() {
        let dir = TempDir::new("progress");
        let root = dir.path().join("library");
        let (controller, _control) = portable_controller(&root);
        let source = wav(&dir.path().join("source"), "progress.wav", 441, 0.1);
        let (plan, progress, cancel) = plan(&controller, vec![source], false, ImportStorage::Copy);
        let captured = progress.lock().expect("progress").clone();
        assert_eq!(captured.stage, ImportStage::Completed);
        assert_eq!(captured.total, 1);
        assert_eq!(captured.items, vec![ImportItemState::Ready]);
        let prepared = prepare_import_files(&plan, progress, cancel);
        assert_eq!(prepared.items[0].index, 0);
        assert!(prepared.items[0].copied);
        assert_eq!(controller.tracks().len(), 0);
        controller.shutdown();
    }

    #[test]
    fn similar_metadata_is_reported_but_not_treated_as_identity() {
        let dir = TempDir::new("similar-meta");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let canonical_root = root.canonicalize().expect("canonical");
        let first = wav(
            &canonical_root.join("Artist/Album"),
            "same-name.wav",
            441,
            0.1,
        );
        let (first_plan, progress, cancel) =
            plan(&controller, vec![first], false, ImportStorage::Reference);
        let prepared = prepare(&first_plan, progress, cancel);
        controller
            .apply_import_plan(&first_plan, &prepared)
            .expect("first");
        let id = controller.tracks()[0].0;

        let second_dir = dir.path().join("other/Artist/Album");
        let second = wav(&second_dir, "same-name.wav", 441, -0.4);
        let (second_plan, _progress, _cancel) =
            plan(&controller, vec![second], false, ImportStorage::Copy);
        assert!(matches!(
            second_plan.candidates[0].status(),
            CandidateStatus::SimilarMetadata(existing) if *existing == id
        ));
        assert_eq!(
            second_plan.candidates[0].decision(),
            ImportDecision::ImportNew
        );
        assert!(second_plan.can_import());
        controller.shutdown();
    }

    #[test]
    fn cancellation_during_copy_removes_only_this_run_s_completed_copies() {
        let dir = TempDir::new("cancel-copy");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let source_dir = dir.path().join("source");
        let one = wav(&source_dir, "one.wav", 44_100, 0.1);
        let two = wav(&source_dir, "two.wav", 44_100, 0.2);
        let (plan, progress, cancel) =
            plan(&controller, vec![one, two], false, ImportStorage::Copy);
        let prepared =
            super::prepare_import_files_with_hook(&plan, progress, Arc::clone(&cancel), |_| {
                cancel.store(true, std::sync::atomic::Ordering::Release)
            });
        assert!(prepared.cancelled);
        assert_eq!(prepared.created_files.len(), 1);
        assert!(
            prepared.created_files.iter().all(|path| !path.exists()),
            "partial run copies are removed on cancellation"
        );
        assert!(controller.apply_import_plan(&plan, &prepared).is_err());
        assert_eq!(controller.tracks().len(), 0);
        controller.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn catalogue_publish_failure_keeps_copied_files_and_retry_persists() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("publish-fail");
        let root = dir.path().join("library");
        let (mut controller, _control) = portable_controller(&root);
        let source = wav(&dir.path().join("source"), "publish.wav", 441, 0.1);
        let (mut plan, progress, cancel) =
            plan(&controller, vec![source], false, ImportStorage::Copy);
        plan.candidates[0].set_title("Pending title".to_owned());
        let prepared = prepare(&plan, progress, cancel);
        let copied = prepared.created_files[0].clone();
        let flux_dir = root.join(".flux");
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o555))
            .expect("read-only catalogue");
        let error = controller
            .apply_import_plan(&plan, &prepared)
            .expect_err("publish must fail");
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o755))
            .expect("restore catalogue permissions");
        assert!(error.contains("Could not publish"), "{error}");
        assert!(
            copied.exists(),
            "recoverable copied file is reported/left in place"
        );
        let published = flux_library::PortableStore::open(&root)
            .expect("open store")
            .store;
        assert_eq!(
            published.len(),
            0,
            "failed publish wrote no catalogue record"
        );
        drop(published);
        controller
            .save_index()
            .expect("retry commit persists pending reviewed import");
        let reopened = flux_library::PortableStore::open(&root)
            .expect("reopen")
            .store;
        assert_eq!(reopened.len(), 1);
        assert_eq!(
            reopened
                .get(TrackId::new(1))
                .map(|track| track.effective_metadata().title),
            Some("Pending title".to_owned())
        );
        controller.shutdown();
    }

    #[test]
    fn cancellation_before_scanning_returns_a_cancelled_plan_without_writes() {
        let dir = TempDir::new("cancel-scan");
        let root = dir.path().join("library");
        let (controller, _control) = portable_controller(&root);
        let source = wav(&dir.path().join("source"), "scan.wav", 441, 0.1);
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(true));
        let plan = prepare_import_plan(
            request(&controller, vec![source], false, ImportStorage::Copy),
            progress,
            cancel,
        );
        assert!(plan.cancelled);
        assert!(plan.candidates.is_empty());
        assert_eq!(controller.tracks().len(), 0);
        controller.shutdown();
    }

    #[test]
    fn credit_review_preserves_order_owner_and_validation() {
        let dir = TempDir::new("credit-review");
        let root = dir.path().join("library");
        let source = wav(&dir.path().join("Artist/Album"), "Song.wav", 441, 0.1);
        let (controller, _control) = portable_controller(&root);
        let (plan, _progress, _cancel) =
            plan(&controller, vec![source], false, ImportStorage::Copy);
        let candidate = &plan.candidates[0];
        assert_eq!(candidate.credits().len(), 1);
        assert_eq!(candidate.owner_index(), 0);
        assert_eq!(candidate.validation_error(), None);
        let mut candidate = candidate.clone();
        candidate.add_credit(ImportCredit::new(None, "Featured Artist", false));
        assert_eq!(candidate.credits().len(), 2);
        assert!(!candidate.remove_credit(0), "owner cannot be removed");
        assert!(candidate.set_owner_index(1));
        assert_eq!(candidate.owner_index(), 1);
        assert_eq!(candidate.artist_name(), Some("Featured Artist"));
        assert!(candidate.move_credit(1, -1));
        assert_eq!(candidate.owner_index(), 0);
        assert_eq!(candidate.credits()[0].name, "Featured Artist");
        candidate.set_credit_artist(0, ArtistId::new(99), "Named".to_owned());
        assert_eq!(candidate.credits()[0].artist_id, Some(ArtistId::new(99)));
        controller.shutdown();
    }
}
