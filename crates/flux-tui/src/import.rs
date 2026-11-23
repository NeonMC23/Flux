//! Import review, worker lifecycle, asset attachment and fixed-target handling.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use flux_app::{
    ImportDecision, ImportPlan, ImportProgress, ImportRequest, ImportStorage, PreparedImport,
    ReleaseType, prepare_import_files, prepare_import_plan,
};

use crate::*;

/// Short, precise review qualifier for one duplicate state.
pub(crate) fn candidate_status_detail(status: &CandidateStatus) -> Option<&'static str> {
    match status {
        CandidateStatus::New => None,
        CandidateStatus::ExistingPath(_) => Some("same managed file"),
        CandidateStatus::SameContent {
            track_id: Some(_), ..
        } => Some("same audio · unique match"),
        CandidateStatus::SameContent { track_id: None, .. } => Some("duplicate within this batch"),
        CandidateStatus::SimilarMetadata(_) => Some("metadata matches existing Track"),
        CandidateStatus::SameTitleDifferentAudio(_) => Some("same title · different audio"),
        CandidateStatus::ProbableAudio(_) => Some("probable duplicate · verify"),
        CandidateStatus::AmbiguousAudio(_) => Some("ambiguous match · decide manually"),
    }
}

pub(crate) struct TuiImport {
    pub(crate) progress: Arc<Mutex<ImportProgress>>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) rx: Receiver<ImportWorkerMessage>,
    pub(crate) worker: Option<JoinHandle<()>>,
    pub(crate) plan: Option<ImportPlan>,
    pub(crate) selected: usize,
    pub(crate) scroll: usize,
    pub(crate) error: Option<String>,
}

pub(crate) enum ImportWorkerMessage {
    Plan(Box<ImportPlan>),
    Prepared(Box<PreparedImport>),
}

fn join_worker_bounded(worker: JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
    } else {
        drop(worker);
    }
}

impl Tui {
    pub(crate) fn open_import_form(&mut self, index: usize) {
        let Some(ui) = self.import_ui.as_ref() else {
            return;
        };
        let Some(plan) = ui.plan.as_ref() else {
            return;
        };
        let Some(candidate) = plan.candidates.get(index) else {
            return;
        };
        let fields = vec![
            FormField {
                label: "Title",
                value: candidate.title().to_owned(),
                hint: "Required",
            },
            FormField {
                label: "Artist",
                value: candidate.artist_name().unwrap_or_default().to_owned(),
                hint: "Optional display credit",
            },
            FormField {
                label: "Album",
                value: candidate.album().unwrap_or_default().to_owned(),
                hint: "Empty imports a standalone track",
            },
            FormField {
                label: "Track number",
                value: candidate
                    .source_track()
                    .track_number()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                hint: "Optional",
            },
            FormField {
                label: "Disc number",
                value: candidate
                    .source_track()
                    .disc_number()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                hint: "Optional",
            },
        ];
        self.form = Some(FormState::new(
            format!("Review metadata · {}", candidate.title()),
            FormKind::ImportCandidate(index),
            fields,
        ));
        self.mode = Mode::FormInput;
    }

    /// Starts cancellable discovery/hashing in a worker using the same shared
    /// import pipeline as the GUI. No catalogue write occurs in this phase.
    pub(crate) fn start_import_review(
        &mut self,
        sources: Vec<PathBuf>,
        recursive: bool,
    ) -> Result<(), String> {
        if sources.is_empty() {
            return Err("Enter at least one file or folder path.".to_owned());
        }
        let Some(root) = self.controller.canonical_root().map(Path::to_path_buf) else {
            return Err(
                "The TUI import manager requires a canonical library; open or create one first."
                    .to_owned(),
            );
        };
        let existing_tracks = self.controller.tracks().to_vec();
        let existing_artists = existing_tracks
            .iter()
            .filter_map(|(id, _)| Some((*id, self.controller.track_artist_id(*id)?)))
            .collect::<Vec<_>>();
        let existing_credits = existing_tracks
            .iter()
            .map(|(id, _)| (*id, self.controller.track_credits(*id)))
            .collect::<Vec<_>>();
        let existing_album_ids = existing_tracks
            .iter()
            .filter_map(|(id, _)| Some((*id, self.controller.track_album_id(*id)?)))
            .collect::<Vec<_>>();
        let request = ImportRequest {
            sources,
            recursive,
            storage: ImportStorage::Copy,
            library_root: root,
            existing_tracks,
            existing_artists,
            existing_credits,
            existing_album_ids,
            target_album: None,
            generation: self.controller.generation(),
            library_session: self.controller.library_session(),
        };
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_progress = Arc::clone(&progress);
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || {
            let plan = prepare_import_plan(request, worker_progress, worker_cancel);
            let _ = tx.send(ImportWorkerMessage::Plan(Box::new(plan)));
        });
        self.import_ui = Some(TuiImport {
            progress,
            cancel,
            rx,
            worker: Some(worker),
            plan: None,
            selected: 0,
            scroll: 0,
            error: None,
        });
        self.mode = Mode::ImportScanning;
        self.lifecycle_msg = Some("Discovering files and checking duplicates…".to_owned());
        Ok(())
    }

    /// Polls the worker once per UI tick. Results are applied on the UI thread
    /// only after the worker has joined, so no partial plan can leak.
    pub(crate) fn poll_import_worker(&mut self) {
        if !matches!(self.mode, Mode::ImportScanning | Mode::ImportPreparing) {
            return;
        }
        let received = self.import_ui.as_ref().map(|ui| ui.rx.try_recv());
        let message = match received {
            Some(Ok(message)) => message,
            Some(Err(TryRecvError::Empty)) | None => return,
            Some(Err(TryRecvError::Disconnected)) => {
                if let Some(worker) = self.import_ui.as_mut().and_then(|ui| ui.worker.take()) {
                    join_worker_bounded(worker);
                }
                self.import_ui = None;
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some("The import worker stopped without a result.".to_owned());
                return;
            }
        };
        if let Some(ui) = self.import_ui.as_mut()
            && let Some(worker) = ui.worker.take()
        {
            join_worker_bounded(worker);
        }
        match message {
            ImportWorkerMessage::Plan(plan) => {
                let plan = *plan;
                let cancelled = plan.cancelled
                    || self
                        .import_ui
                        .as_ref()
                        .is_some_and(|ui| ui.cancel.load(Ordering::Acquire));
                if cancelled {
                    self.import_ui = None;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg =
                        Some("Import cancelled; no catalogue changes were made.".to_owned());
                    return;
                }
                if plan.candidates.is_empty() {
                    let mut lines = plan
                        .issues
                        .iter()
                        .map(|issue| format!("{} — {}", issue.path().display(), issue.reason()))
                        .collect::<Vec<_>>();
                    if lines.is_empty() {
                        lines.push("No supported audio files were discovered.".to_owned());
                    }
                    self.import_ui = None;
                    self.lifecycle_msg =
                        Some("Import scan found no supported audio files.".to_owned());
                    self.open_info("Import scan results", lines);
                    return;
                }
                if let Some(ui) = self.import_ui.as_mut() {
                    ui.selected = plan
                        .candidates
                        .iter()
                        .position(|candidate| candidate.decision() != ImportDecision::Skip)
                        .unwrap_or(0);
                    ui.scroll = 0;
                    ui.plan = Some(plan);
                    ui.error = None;
                }
                self.mode = Mode::ImportReview;
                self.lifecycle_msg = Some(
                    "Review the discovered candidates; no files have been copied yet.".to_owned(),
                );
            }
            ImportWorkerMessage::Prepared(prepared) => {
                let prepared = *prepared;
                let cancel_requested = self
                    .import_ui
                    .as_ref()
                    .is_some_and(|ui| ui.cancel.load(Ordering::Acquire));
                let plan = self.import_ui.as_mut().and_then(|ui| ui.plan.take());
                if prepared.cancelled || cancel_requested {
                    if cancel_requested && !prepared.cancelled {
                        for path in &prepared.created_files {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                    self.import_ui = None;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg =
                        Some("Import cancelled during preparation; no catalogue changes were made and unpublished copies were removed.".to_owned());
                    return;
                }
                let Some(plan) = plan else {
                    self.import_ui = None;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg = Some("The reviewed import plan was lost.".to_owned());
                    return;
                };
                self.import_ui = None;
                match self.controller.apply_import_plan(&plan, &prepared) {
                    Ok(report) => {
                        let summary = format!(
                            "Import complete: {} new, {} updated, {} skipped, {} failed, {} discovery issue(s).",
                            report.imported,
                            report.updated,
                            report.skipped,
                            report.failed,
                            plan.issues.len()
                        );
                        let mut details = plan
                            .issues
                            .iter()
                            .map(|issue| format!("{} — {}", issue.path().display(), issue.reason()))
                            .collect::<Vec<_>>();
                        details.extend(
                            report
                                .items
                                .iter()
                                .filter(|item| item.state == flux_app::ImportItemState::Failed)
                                .map(|item| item.message.clone()),
                        );
                        if details.is_empty() {
                            self.lifecycle_msg = Some(summary);
                            self.mode = Mode::Normal;
                        } else {
                            self.lifecycle_msg = Some(summary.clone());
                            details.insert(0, summary);
                            self.open_info("Import results", details);
                        }
                    }
                    Err(error) => {
                        self.mode = Mode::Normal;
                        self.lifecycle_msg = Some(error);
                    }
                }
            }
        }
    }

    pub(crate) fn import_progress_snapshot(&self) -> ImportProgress {
        self.import_ui
            .as_ref()
            .and_then(|ui| ui.progress.lock().ok().map(|progress| progress.clone()))
            .unwrap_or_default()
    }

    pub(crate) fn toggle_import_decision(&mut self) {
        let Some(ui) = self.import_ui.as_mut() else {
            return;
        };
        let Some(candidate) = ui
            .plan
            .as_mut()
            .and_then(|plan| plan.candidates.get_mut(ui.selected))
        else {
            return;
        };
        match candidate.decision() {
            ImportDecision::ImportNew | ImportDecision::UpdateExisting => {
                candidate.set_decision(ImportDecision::Skip);
            }
            ImportDecision::Skip => candidate.set_decision(ImportDecision::ImportNew),
        }
    }

    pub(crate) fn toggle_import_update(&mut self) {
        let Some(ui) = self.import_ui.as_mut() else {
            return;
        };
        let Some(candidate) = ui
            .plan
            .as_mut()
            .and_then(|plan| plan.candidates.get_mut(ui.selected))
        else {
            return;
        };
        if candidate.status().can_update_metadata() && candidate.existing_match().is_some() {
            candidate.set_decision(match candidate.decision() {
                ImportDecision::UpdateExisting => ImportDecision::Skip,
                _ => ImportDecision::UpdateExisting,
            });
        } else {
            ui.error = Some(
                "This candidate has no unique exact-audio match to update. Use Import or Skip."
                    .to_owned(),
            );
        }
    }

    pub(crate) fn bulk_import_decision(&mut self, decision: ImportDecision) {
        let Some(ui) = self.import_ui.as_mut() else {
            return;
        };
        let Some(plan) = ui.plan.as_mut() else {
            return;
        };
        for candidate in &mut plan.candidates {
            let allowed = match decision {
                ImportDecision::ImportNew => true,
                ImportDecision::UpdateExisting => {
                    candidate.status().can_update_metadata() && candidate.existing_match().is_some()
                }
                ImportDecision::Skip => true,
            };
            candidate.set_decision(if allowed {
                decision
            } else {
                ImportDecision::Skip
            });
        }
        ui.error = None;
    }

    pub(crate) fn prepare_reviewed_import(&mut self) {
        let Some(ui) = self.import_ui.as_mut() else {
            return;
        };
        let Some(plan) = ui.plan.clone() else {
            return;
        };
        if !plan
            .candidates
            .iter()
            .any(|candidate| candidate.decision() != ImportDecision::Skip)
        {
            ui.error = Some("Select at least one candidate to import or update.".to_owned());
            return;
        }
        if !plan.can_import() {
            ui.error = Some(plan.blocking_reason().unwrap_or_else(|| {
                "Fix the highlighted review fields before preparing the import.".to_owned()
            }));
            return;
        }
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_progress = Arc::clone(&progress);
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || {
            let prepared = prepare_import_files(&plan, worker_progress, worker_cancel);
            let _ = tx.send(ImportWorkerMessage::Prepared(Box::new(prepared)));
        });
        ui.progress = progress;
        ui.cancel = cancel;
        ui.rx = rx;
        ui.worker = Some(worker);
        ui.error = None;
        self.mode = Mode::ImportPreparing;
        self.lifecycle_msg = Some("Preparing copies; catalogue has not changed yet.".to_owned());
    }

    /// Cancels and joins any in-flight import worker. Called on quit.
    pub(crate) fn stop_import_worker(&mut self) {
        if let Some(ui) = self.import_ui.as_mut() {
            ui.cancel.store(true, Ordering::Release);
            if let Some(worker) = ui.worker.take() {
                join_worker_bounded(worker);
            }
        }
        self.import_ui = None;
        if matches!(
            self.mode,
            Mode::ImportScanning | Mode::ImportReview | Mode::ImportPreparing
        ) {
            self.mode = Mode::Normal;
        }
    }

    pub(crate) fn close_import_review(&mut self) {
        if matches!(self.mode, Mode::ImportScanning | Mode::ImportPreparing) {
            if let Some(ui) = self.import_ui.as_mut() {
                ui.cancel.store(true, Ordering::Release);
            }
            self.lifecycle_msg =
                Some("Cancelling import; waiting for the worker to release files…".to_owned());
            return;
        }
        let issues = self
            .import_ui
            .as_ref()
            .and_then(|ui| ui.plan.as_ref())
            .map_or(0, |plan| plan.issues.len());
        self.import_ui = None;
        self.mode = Mode::Normal;
        self.lifecycle_msg = Some(if issues > 0 {
            format!(
                "Import review closed; nothing was copied or published. {issues} discovery issue(s) were not shown in the review."
            )
        } else {
            "Import review closed; nothing was copied or published.".to_owned()
        });
    }

    pub(crate) fn handle_import_progress_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c || key.code == KeyCode::Char('q') {
            self.stop_import_worker();
            return true;
        }
        if key.code == KeyCode::Esc {
            self.close_import_review();
        }
        false
    }

    pub(crate) fn handle_import_review_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c || key.code == KeyCode::Char('q') {
            self.import_ui = None;
            self.mode = Mode::Normal;
            return true;
        }
        match key.code {
            KeyCode::Esc => self.close_import_review(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ui) = self.import_ui.as_mut() {
                    ui.selected = ui.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ui) = self.import_ui.as_mut() {
                    let max = ui
                        .plan
                        .as_ref()
                        .map_or(0, |plan| plan.candidates.len().saturating_sub(1));
                    ui.selected = (ui.selected + 1).min(max);
                }
            }
            KeyCode::PageUp => {
                if let Some(ui) = self.import_ui.as_mut() {
                    ui.selected = ui.selected.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if let Some(ui) = self.import_ui.as_mut() {
                    let max = ui
                        .plan
                        .as_ref()
                        .map_or(0, |plan| plan.candidates.len().saturating_sub(1));
                    ui.selected = (ui.selected + 10).min(max);
                }
            }
            KeyCode::Char(' ') => self.toggle_import_decision(),
            KeyCode::Char('e') => {
                self.open_import_form(self.import_ui.as_ref().map_or(0, |ui| ui.selected))
            }
            KeyCode::Char('F') => self.open_import_album_picker(),
            KeyCode::Char('1') => self.apply_import_release_type(ReleaseType::Album),
            KeyCode::Char('2') => self.apply_import_release_type(ReleaseType::Ep),
            KeyCode::Char('3') => self.apply_import_release_type(ReleaseType::Single),
            KeyCode::Char('4') => self.apply_import_release_type(ReleaseType::Compilation),
            KeyCode::Char('5') => self.apply_import_release_type(ReleaseType::Other),
            KeyCode::Char('A') => self.begin_import_asset(ImportAssetKind::AlbumArtwork),
            KeyCode::Char('T') => self.begin_import_asset(ImportAssetKind::TrackArtwork),
            KeyCode::Char('L') => self.begin_import_asset(ImportAssetKind::Lyrics),
            KeyCode::Char('u') => self.toggle_import_update(),
            KeyCode::Char('a') => self.bulk_import_decision(ImportDecision::ImportNew),
            KeyCode::Char('s') => self.bulk_import_decision(ImportDecision::Skip),
            KeyCode::Enter | KeyCode::Char('y') => self.prepare_reviewed_import(),
            _ => {}
        }
        false
    }

    pub(crate) fn import_album_targets(&self) -> Vec<ImportAlbumTarget> {
        let mut albums = self.controller.albums();
        albums.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        albums
            .into_iter()
            .filter_map(|album| {
                let owner = self.controller.artist(album.owner_artist_id)?;
                Some(ImportAlbumTarget {
                    album_id: album.id,
                    owner_artist_id: album.owner_artist_id,
                    owner_name: owner.name().to_owned(),
                    title: album.title,
                    release_type: album.release_type,
                    cover: album.cover,
                    track_order: album.track_order,
                    generation: self.controller.generation(),
                })
            })
            .collect()
    }

    pub(crate) fn open_import_album_picker(&mut self) {
        if self.import_ui.is_none() {
            return;
        }
        self.import_album_cursor = 0;
        self.mode = Mode::ImportAlbumPick;
    }

    pub(crate) fn apply_import_album_target(&mut self) {
        let targets = self.import_album_targets();
        let Some(target) = targets.get(self.import_album_cursor).cloned() else {
            self.mode = Mode::ImportReview;
            return;
        };
        let Some(ui) = self.import_ui.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(plan) = ui.plan.as_mut() else {
            self.mode = Mode::ImportReview;
            return;
        };
        plan.target_album = Some(target.clone());
        for candidate in &mut plan.candidates {
            candidate.force_album_target(&target);
        }
        self.mode = Mode::ImportReview;
        self.lifecycle_msg = Some(format!(
            "Importing reviewed tracks into “{}”; owner, album and owner credit were applied to every candidate.",
            target.title
        ));
    }

    pub(crate) fn apply_import_release_type(&mut self, kind: ReleaseType) {
        let Some(ui) = self.import_ui.as_mut() else {
            return;
        };
        let Some(plan) = ui.plan.as_mut() else {
            return;
        };
        let indices = (0..plan.candidates.len()).collect::<Vec<_>>();
        plan.apply_release_type_to(&indices, kind);
        self.lifecycle_msg = Some(format!(
            "Release type set to {} for newly created albums.",
            kind.as_str()
        ));
    }

    pub(crate) fn begin_import_asset(&mut self, kind: ImportAssetKind) {
        self.import_asset = Some(kind);
        self.input.clear();
        self.mode = Mode::ImportAssetInput;
    }

    pub(crate) fn apply_import_asset(&mut self, path_text: &str) {
        let Some(kind) = self.import_asset.take() else {
            self.mode = Mode::ImportReview;
            return;
        };
        let path = PathBuf::from(path_text.trim());
        if path.as_os_str().is_empty() {
            self.lifecycle_msg = Some("Enter an asset path, or - to clear.".to_owned());
            self.import_asset = Some(kind);
            self.mode = Mode::ImportAssetInput;
            return;
        }
        let Some(ui) = self.import_ui.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(plan) = ui.plan.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let index = ui.selected;
        let Some(candidate) = plan.candidates.get_mut(index) else {
            self.mode = Mode::ImportReview;
            return;
        };
        let result = match kind {
            ImportAssetKind::AlbumArtwork => {
                if path.as_os_str() == "-" {
                    candidate.clear_album_artwork();
                    Ok(())
                } else if candidate.album().is_none() {
                    Err("Album artwork requires an album title for this track.".to_owned())
                } else {
                    candidate.set_album_artwork(&path)
                }
            }
            ImportAssetKind::TrackArtwork => {
                if path.as_os_str() == "-" {
                    candidate.clear_artwork();
                    Ok(())
                } else {
                    candidate.set_artwork(&path)
                }
            }
            ImportAssetKind::Lyrics => {
                if path.as_os_str() == "-" {
                    candidate.set_lyrics_text(None);
                    Ok(())
                } else {
                    plan.apply_lyrics_from_path(&[index], &path)
                }
            }
        };
        self.mode = Mode::ImportReview;
        self.lifecycle_msg = Some(match result {
            Ok(()) => match kind {
                ImportAssetKind::AlbumArtwork => "Updated album artwork for the review.".to_owned(),
                ImportAssetKind::TrackArtwork => "Updated track artwork for the review.".to_owned(),
                ImportAssetKind::Lyrics => "Attached lyrics to the review.".to_owned(),
            },
            Err(error) => error,
        });
    }
}
