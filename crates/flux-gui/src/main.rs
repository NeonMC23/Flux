//! Flux desktop GUI (`flux`), built on Iced.
//!
//! A minimal functional player over the shared [`flux_app::Controller`]
//! — the same controller the TUI uses. This file owns only view state
//! (the folder-path and search text fields); every playback or library
//! decision lives in the controller. Filesystem scanning, index
//! persistence, decoding, and device I/O all happen on controller/
//! engine worker threads; the UI thread only sends commands and, on a
//! 150 ms timer subscription, drains controller events — no busy
//! polling, no blocking calls in `update` or `view`.
//!
//! Real audio output requires a build with `--features cpal`;
//! otherwise a banner says so and playback attempts report why (see
//! the README).

use std::path::PathBuf;
use std::time::Duration;

use flux_app::{
    Controller, MigrationPlan, ReconcileReport, StorageMode, default_index_path, format_duration,
    is_track_playable,
};
use flux_core::{APP_NAME, MILESTONE, PlaybackState, TrackId};
use iced::widget::{button, column, container, row, scrollable, slider, text, text_input};
use iced::{Element, Length, Subscription, Task, window};

impl Flux {
    /// Housekeeping after a successful library switch: the search
    /// text and any pending migration state referred to the old
    /// library, so they are cleared. (The controller itself already
    /// stopped playback and rebuilt its queue from the new library.)
    fn after_switch(&mut self) {
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.lifecycle_msg = None;
    }
}

/// What the user reviews before confirming a migration: what will
/// migrate, what is excluded and why, and the safety guarantees that
/// hold either way.
fn migration_plan_summary(plan: &MigrationPlan) -> String {
    format!(
        "Migration plan: {} track(s) will migrate ({} present, {} missing — \
         kept and marked unavailable). Excluded: {} outside the destination \
         folder, {} reached through symlinks, {} with unrepresentable paths, \
         {} ambiguous after path normalization. Track IDs and the ID counter \
         are preserved; the legacy index and all audio files stay untouched. \
         Choose “Execute migration” to proceed, or Cancel to write nothing.",
        plan.migrated_count(),
        plan.present_count(),
        plan.missing_count(),
        plan.external_count(),
        plan.symlinked_count(),
        plan.unrepresentable_count(),
        plan.ambiguous_count(),
    )
}

/// The reconciliation result in words: what was refreshed, what is
/// missing, and whether possible new locations were found (applying
/// one is a separate validated step the GUI does not automate).
fn reconcile_summary(report: &ReconcileReport) -> String {
    let mut summary = format!(
        "Reconciliation: {} track(s) checked — {} unchanged, {} changed, \
         {} present again, {} missing (records kept), {} could not be inspected.",
        report.inspected_count(),
        report.unchanged_count() + report.established_count(),
        report.changed_count(),
        report.reappeared_count(),
        report.missing_count(),
        report.inspection_error_count() + report.fingerprint_unavailable_count(),
    );
    if report.unique_candidate_count() > 0 || report.ambiguous_candidate_count() > 0 {
        summary.push_str(&format!(
            " Possible new locations found for {} missing track(s) ({} ambiguous); \
             nothing was moved automatically.",
            report.unique_candidate_count() + report.ambiguous_candidate_count(),
            report.ambiguous_candidate_count(),
        ));
    }
    summary
}

pub fn main() -> iced::Result {
    iced::application(Flux::boot, Flux::update, Flux::view)
        .title(APP_NAME)
        .subscription(Flux::subscription)
        .window_size((920.0, 640.0))
        // Intercept the close request so the index can be saved first.
        .exit_on_close_request(false)
        .run()
}

struct Flux {
    controller: Controller,
    dir_input: String,
    search: String,
    /// Explicit path entry for the portable library folder (open /
    /// create / migration destination). There is no native directory
    /// picker in the platform layer, so this mirrors the scan path
    /// entry above.
    lib_input: String,
    /// Legacy index file selected for migration.
    migrate_source: String,
    /// A planned-but-not-yet-confirmed migration. Planning wrote
    /// nothing; dropping this is a complete cancel.
    pending_plan: Option<MigrationPlan>,
    /// Destination of a completed migration, offered for activation.
    migrated_dest: Option<PathBuf>,
    /// Latest library-lifecycle outcome (plan summary, errors, …).
    lifecycle_msg: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    DirInputChanged(String),
    LibInputChanged(String),
    OpenLibraryPressed,
    CreateLibraryPressed,
    UseLegacyIndexPressed,
    AcknowledgeRecoveryPressed,
    MigrateSourceChanged(String),
    PlanMigrationPressed,
    ExecuteMigrationPressed,
    CancelMigrationPressed,
    OpenMigratedPressed,
    ReconcilePressed,
    ScanPressed,
    RefreshPressed,
    SearchChanged(String),
    PlayTrack(TrackId),
    TogglePlayPause,
    Stop,
    Next,
    Previous,
    CycleRepeat,
    VolumeChanged(f32),
    /// Timer tick: drain controller events / refresh the position.
    Tick,
    WindowCloseRequested(window::Id),
}

impl Flux {
    fn boot() -> Self {
        // Index loading reads one small file; it happens here, before
        // the first frame, never inside update/view.
        // Startup uses the legacy app index, exactly as before:
        // opening, creating, migrating, or switching libraries only
        // ever happens through the explicit controls below.
        Self {
            controller: Controller::open_default(),
            dir_input: String::new(),
            search: String::new(),
            lib_input: String::new(),
            migrate_source: String::new(),
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(150)).map(|_| Message::Tick),
            window::close_requests().map(Message::WindowCloseRequested),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::DirInputChanged(value) => self.dir_input = value,
            Message::LibInputChanged(value) => self.lib_input = value,
            // Switch/create outcomes are mirrored by the controller
            // into last_error/last_status (shown at the bottom); a
            // failed switch leaves the active library intact.
            Message::OpenLibraryPressed => {
                let path = PathBuf::from(self.lib_input.trim());
                if self.controller.switch_to_portable(&path).is_ok() {
                    self.after_switch();
                }
            }
            Message::CreateLibraryPressed => {
                let path = PathBuf::from(self.lib_input.trim());
                if self
                    .controller
                    .switch_to_new_portable_library(&path)
                    .is_ok()
                {
                    self.after_switch();
                }
            }
            Message::UseLegacyIndexPressed => {
                if self
                    .controller
                    .switch_to_legacy_index(default_index_path())
                    .is_ok()
                {
                    self.after_switch();
                }
            }
            Message::AcknowledgeRecoveryPressed => {
                if let Err(err) = self.controller.acknowledge_portable_recovery() {
                    self.lifecycle_msg = Some(err);
                }
            }
            Message::MigrateSourceChanged(value) => self.migrate_source = value,
            Message::PlanMigrationPressed => {
                let source = PathBuf::from(self.migrate_source.trim());
                let destination = PathBuf::from(self.lib_input.trim());
                match flux_app::plan_legacy_migration(&source, &destination) {
                    Ok(plan) => {
                        self.lifecycle_msg = Some(migration_plan_summary(&plan));
                        self.pending_plan = Some(plan);
                        self.migrated_dest = None;
                    }
                    Err(err) => {
                        self.lifecycle_msg = Some(format!("Migration planning failed: {err}"));
                        self.pending_plan = None;
                    }
                }
            }
            Message::ExecuteMigrationPressed => {
                if let Some(plan) = self.pending_plan.take() {
                    let destination = plan.destination_root().to_path_buf();
                    match flux_app::execute_legacy_migration(&plan) {
                        Ok(report) => {
                            let done = report.plan();
                            let excluded = done.records().len() - done.migrated_count();
                            self.migrated_dest = Some(destination);
                            self.lifecycle_msg = Some(format!(
                                "Migration complete: {} track(s) migrated \
                                 ({} present, {} missing), {} excluded. The active \
                                 library is unchanged — choose “Open migrated \
                                 library” to switch to the result.",
                                done.migrated_count(),
                                done.present_count(),
                                done.missing_count(),
                                excluded,
                            ));
                        }
                        Err(err) => {
                            self.lifecycle_msg = Some(format!("Migration failed: {err}"));
                        }
                    }
                }
            }
            Message::CancelMigrationPressed => {
                // Planning wrote nothing, so dropping the plan is a
                // complete cancellation.
                self.pending_plan = None;
                self.lifecycle_msg = Some("Migration cancelled — nothing was written.".to_owned());
            }
            Message::OpenMigratedPressed => {
                if let Some(destination) = self.migrated_dest.clone()
                    && self.controller.switch_to_portable(&destination).is_ok()
                {
                    self.after_switch();
                }
            }
            Message::ReconcilePressed => {
                // Failures are mirrored into the controller's error
                // line; the detailed summary goes to the lifecycle
                // message area.
                if let Ok(report) = self.controller.reconcile_portable_library() {
                    self.lifecycle_msg = Some(reconcile_summary(&report));
                }
            }
            Message::ScanPressed => {
                let path = self.dir_input.trim();
                if !path.is_empty() {
                    self.controller.begin_scan(PathBuf::from(path));
                }
            }
            Message::RefreshPressed => self.controller.refresh(),
            Message::SearchChanged(value) => self.search = value,
            Message::PlayTrack(id) => self.controller.play_track(id),
            Message::TogglePlayPause => self.controller.toggle_play_pause(),
            Message::Stop => self.controller.stop(),
            Message::Next => self.controller.next(),
            Message::Previous => self.controller.previous(),
            Message::CycleRepeat => self.controller.cycle_repeat_mode(),
            Message::VolumeChanged(volume) => self.controller.set_volume(volume),
            Message::Tick => {
                self.controller.poll();
            }
            Message::WindowCloseRequested(id) => {
                // Best-effort save (scans save automatically), then
                // actually close.
                let _ = self.controller.save_index();
                return window::close(id);
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let controller = &self.controller;

        let header = row![
            text(APP_NAME).size(28),
            text(format!("Milestone {MILESTONE}")).size(14),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center);

        let mut content = column![header].spacing(10);

        if let Some(reason) = controller.audio_status().reason() {
            content = content.push(
                container(text(format!("Audio output unavailable — {reason}")).size(13))
                    .padding(8)
                    .width(Length::Fill),
            );
        }

        // ---- library controls ----
        let scanning = controller.is_scanning();
        let scan_row = row![
            text_input("Music folder path…", &self.dir_input)
                .on_input(Message::DirInputChanged)
                .on_submit(Message::ScanPressed)
                .width(Length::Fill),
            button(text("Scan folder")).on_press_maybe(
                (!scanning && !self.dir_input.trim().is_empty()).then_some(Message::ScanPressed)
            ),
            button(text("Refresh")).on_press_maybe((!scanning).then_some(Message::RefreshPressed)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        content = content.push(scan_row);
        if scanning {
            content = content.push(text("Scanning…").size(13));
        }

        // ---- library lifecycle (open / create / migrate / switch) ----
        let current_library = match controller.storage_mode() {
            StorageMode::Legacy => match controller.legacy_index_path() {
                Some(path) => format!("Library: legacy app index — {}", path.display()),
                None => "Library: legacy (no persistence)".to_owned(),
            },
            StorageMode::Portable => match controller.portable_root() {
                Some(root) => format!("Library: portable — {}", root.display()),
                None => "Library: portable".to_owned(),
            },
        };
        content = content.push(text(current_library).size(13));

        let lib_path_ready = !scanning && !self.lib_input.trim().is_empty();
        let lib_row = row![
            text_input("Portable library folder…", &self.lib_input)
                .on_input(Message::LibInputChanged)
                .on_submit(Message::OpenLibraryPressed)
                .width(Length::Fill),
            button(text("Open library"))
                .on_press_maybe(lib_path_ready.then_some(Message::OpenLibraryPressed)),
            button(text("Create library"))
                .on_press_maybe(lib_path_ready.then_some(Message::CreateLibraryPressed)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        content = content.push(lib_row);

        if controller.storage_mode() == StorageMode::Portable {
            let portable_actions = row![
                button(text("Use app library index"))
                    .on_press_maybe((!scanning).then_some(Message::UseLegacyIndexPressed)),
                button(text("Reconcile library"))
                    .on_press_maybe((!scanning).then_some(Message::ReconcilePressed)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            content = content.push(portable_actions);
        }
        if controller.portable_recovery().is_some() {
            content = content.push(
                button(text("Acknowledge recovered library"))
                    .on_press_maybe((!scanning).then_some(Message::AcknowledgeRecoveryPressed)),
            );
        }

        let migrate_row = row![
            text_input("Legacy index file to migrate…", &self.migrate_source)
                .on_input(Message::MigrateSourceChanged)
                .width(Length::Fill),
            button(text("Plan migration")).on_press_maybe(
                (lib_path_ready && !self.migrate_source.trim().is_empty())
                    .then_some(Message::PlanMigrationPressed)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        content = content.push(migrate_row);

        if self.pending_plan.is_some() {
            let confirm_row = row![
                button(text("Execute migration")).on_press(Message::ExecuteMigrationPressed),
                button(text("Cancel")).on_press(Message::CancelMigrationPressed),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            content = content.push(confirm_row);
        }
        if self.migrated_dest.is_some() {
            content = content
                .push(button(text("Open migrated library")).on_press(Message::OpenMigratedPressed));
        }
        if let Some(message) = &self.lifecycle_msg {
            content = content.push(text(message).size(13));
        }

        content = content.push(
            text_input("Search title, artist, album…", &self.search)
                .on_input(Message::SearchChanged),
        );

        // ---- track list ----
        let rows = controller.search(&self.search);
        let current = controller.current_id();
        let state = controller.player_state();
        let list: Element<'_, Message> = if controller.tracks().is_empty() {
            container(text(
                "Your library is empty.\nEnter a music folder above and choose “Scan folder”.",
            ))
            .padding(16)
            .into()
        } else if rows.is_empty() {
            container(text("No tracks match the current search."))
                .padding(16)
                .into()
        } else {
            let mut list = column![].spacing(2);
            for (id, track) in rows {
                let marker = if Some(id) == current {
                    match state {
                        PlaybackState::Playing => "▶",
                        PlaybackState::Paused => "⏸",
                        PlaybackState::Stopped => "·",
                    }
                } else {
                    " "
                };
                let mut title = match track.artist() {
                    Some(artist) => format!("{marker} {} — {artist}", track.title()),
                    None => format!("{marker} {}", track.title()),
                };
                if !is_track_playable(track) {
                    title.push_str("  [indexed — not playable yet]");
                }
                let duration = track.duration().map(format_duration).unwrap_or_default();
                let row = row![text(title).width(Length::Fill), text(duration)]
                    .spacing(8)
                    .align_y(iced::Alignment::Center);
                list = list.push(
                    button(row)
                        .width(Length::Fill)
                        .on_press(Message::PlayTrack(id)),
                );
            }
            scrollable(list).height(Length::Fill).into()
        };
        content = content.push(container(list).height(Length::Fill).width(Length::Fill));

        // ---- transport ----
        let now_playing = match controller.current_track() {
            Some(track) => {
                let total = track
                    .duration()
                    .or_else(|| controller.stream_info().and_then(|i| i.duration))
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".to_owned());
                let title = match track.artist() {
                    Some(artist) => format!("{} — {artist}", track.title()),
                    None => track.title().to_owned(),
                };
                format!(
                    "{title}    {} / {total}",
                    format_duration(controller.position())
                )
            }
            None => "Nothing selected".to_owned(),
        };
        let play_label = if state == PlaybackState::Playing {
            "⏸ Pause"
        } else {
            "▶ Play"
        };
        let repeat_label = match controller.repeat_mode() {
            flux_core::RepeatMode::Off => "Repeat: off",
            flux_core::RepeatMode::All => "Repeat: all",
            flux_core::RepeatMode::One => "Repeat: one",
        };
        let volume = controller.volume();
        let transport = row![
            button(text("⏮")).on_press(Message::Previous),
            button(text(play_label)).on_press(Message::TogglePlayPause),
            button(text("⏹ Stop")).on_press(Message::Stop),
            button(text("⏭")).on_press(Message::Next),
            button(text(repeat_label)).on_press(Message::CycleRepeat),
            text("Vol"),
            slider(0.0..=1.0, volume, Message::VolumeChanged)
                .step(0.01_f32)
                .width(160),
            text(format!("{}%", (volume * 100.0).round() as u32)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        content = content.push(text(now_playing).size(16));
        content = content.push(transport);

        // ---- status / error ----
        if let Some(error) = controller.last_error() {
            content = content.push(text(error).size(13));
        } else if let Some(status) = controller.last_status() {
            content = content.push(text(status).size(13));
        }

        content.padding(16).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-gui-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A Flux state over a hermetic legacy controller (temp index,
    /// headless engine) — the update()-level wiring is what is under
    /// test; lifecycle semantics themselves live in `flux-app`.
    fn gui(dir: &TempDir) -> Flux {
        let (engine, audio) = flux_app::default_engine();
        Flux {
            controller: Controller::new(engine, audio, Some(dir.path().join("library-index.json"))),
            dir_input: String::new(),
            search: String::new(),
            lib_input: String::new(),
            migrate_source: String::new(),
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
        }
    }

    fn send(gui: &mut Flux, message: Message) {
        let _task = gui.update(message);
    }

    #[test]
    fn create_library_message_creates_and_switches() {
        let dir = TempDir::new("create-switch");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut gui = gui(&dir);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::CreateLibraryPressed);

        assert_eq!(gui.controller.storage_mode(), StorageMode::Portable);
        assert_eq!(
            gui.controller.portable_root(),
            Some(std::fs::canonicalize(&root).expect("canonical").as_path())
        );
        assert!(
            root.join(".flux").exists(),
            "store was created via flux-app"
        );
        gui.controller.shutdown();
    }

    #[test]
    fn failed_open_message_keeps_active_library() {
        let dir = TempDir::new("failed-open");
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).expect("empty dir");
        let mut gui = gui(&dir);

        send(
            &mut gui,
            Message::LibInputChanged(empty.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);

        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);
        assert!(gui.controller.last_error().is_some(), "failure is visible");
        assert!(!empty.join(".flux").exists(), "opening created nothing");
        gui.controller.shutdown();
    }

    #[test]
    fn migration_messages_plan_cancel_execute_and_activate() {
        let dir = TempDir::new("migration-flow");
        // Destination folder holding one real (tiny, valid) WAV.
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("root");
        let song = root.join("song.wav");
        std::fs::write(&song, minimal_wav()).expect("song");
        // A version-1 legacy index referencing it as ID 1.
        let index = dir.path().join("legacy.json");
        std::fs::write(
            &index,
            format!(
                r#"{{"version":1,"next_id":2,"tracks":[{{"id":1,"title":"Song","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}]}}"#,
                song.display()
            ),
        )
        .expect("index");

        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(
            &mut gui,
            Message::MigrateSourceChanged(index.display().to_string()),
        );

        // Plan → cancel: pending cleared, nothing written, same library.
        send(&mut gui, Message::PlanMigrationPressed);
        assert!(gui.pending_plan.is_some(), "plan awaits confirmation");
        assert!(gui.lifecycle_msg.is_some(), "plan summary is shown");
        send(&mut gui, Message::CancelMigrationPressed);
        assert!(gui.pending_plan.is_none());
        assert!(!root.join(".flux").exists(), "cancel wrote nothing");
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        // Plan → execute: store exists, active library still legacy.
        send(&mut gui, Message::PlanMigrationPressed);
        send(&mut gui, Message::ExecuteMigrationPressed);
        assert!(gui.pending_plan.is_none());
        assert_eq!(gui.migrated_dest.as_deref(), Some(root.as_path()));
        assert!(root.join(".flux").exists());
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        // Activate: only now does the controller switch.
        send(&mut gui, Message::OpenMigratedPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Portable);
        let ids: Vec<u64> = gui
            .controller
            .tracks()
            .iter()
            .map(|(id, _)| id.get())
            .collect();
        assert_eq!(ids, vec![1], "migrated ID preserved through activation");
        gui.controller.shutdown();
    }

    #[test]
    fn reconcile_message_reports_summary_and_never_switches() {
        let dir = TempDir::new("reconcile");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut gui = gui(&dir);

        // Legacy mode: refused and visible, active library untouched.
        send(&mut gui, Message::ReconcilePressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);
        assert!(gui.controller.last_error().is_some());
        assert!(gui.lifecycle_msg.is_none(), "no false success summary");

        // Portable mode: the summary reaches the lifecycle area.
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::CreateLibraryPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Portable);
        send(&mut gui, Message::ReconcilePressed);
        let message = gui.lifecycle_msg.clone().expect("summary shown");
        assert!(
            message.starts_with("Reconciliation: 0 track(s) checked"),
            "{message}"
        );
        assert_eq!(
            gui.controller.storage_mode(),
            StorageMode::Portable,
            "reconciling never switches libraries"
        );
        gui.controller.shutdown();
    }

    /// The smallest valid PCM WAV (44-byte header, no samples) —
    /// enough for the scanner/migration to treat the file as audio
    /// metadata-wise; nothing here plays it.
    fn minimal_wav() -> Vec<u8> {
        let mut wav = Vec::with_capacity(44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&88_200u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav
    }
}
