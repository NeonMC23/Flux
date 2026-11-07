//! Flux terminal interface (`flux-tui`).
//!
//! A minimal interactive player over the shared [`flux_app::Controller`]
//! — the same controller the GUI uses, so behavior matches exactly.
//! Rendering is hand-rolled on top of `crossterm` (no widget
//! framework): a header, a now-playing line, a scrollable track or queue
//! list, a status/error line, and a help/input line. Playback, scanning,
//! import, catalogue and persistence decisions all stay in the shared
//! Controller; this thread only draws and routes keys.
//!
//! Usage: `flux-tui [MUSIC_DIR]` — with a directory argument, a copy-only
//! recursive import starts immediately. Real audio output requires a build
//! with `--features cpal`; otherwise the app says so and playback commands
//! report why (see the README).
//!
//! Keys:
//! - `↑`/`k`, `↓`/`j` move; `PgUp`/`PgDn`, `g`/`G` jump
//! - `Enter` play selected, `Space` play/pause, `s` stop
//! - `n` next, `p` previous, `+`/`-` volume, `R` cycle repeat
//! - `K` toggle crossfade, `[`/`]` decrease/increase its duration
//! - `a` add selected track to the queue, `x` play it next
//! - `Q` open the queue-entry view: `Enter` plays the selected stable
//!   QueueEntryId, `Delete`/`d` removes it, `Ctrl+↑`/`Ctrl+↓` reorders it;
//!   duplicate tracks remain distinct entries
//! - `o` scan a folder (type a path), `r` refresh scanned folders
//! - `i`/`I` quick copy-import paths into the canonical library;
//!   per-path issues open a scrollable result panel (the GUI has the full
//!   review-first Import Manager), `e` edit the selected track,
//!   `P` create an artist, `Y` rename an artist (`artist-id|new-name`),
//!   `V` list artist IDs, `X` reconcile, `L` relink a unique reported
//!   move candidate
//! - `/` search/filter, `Esc` clear search or cancel input, `?` opens
//!   the full scrollable help/limitations panel
//! - `q` or `Ctrl-C` quit (terminal state is always restored)
//! - `U` preserve and replace an unreadable legacy app index only after
//!   an explicit `y` confirmation
//! - `O` open a canonical library (type its folder), `C` create one,
//!   `M` migrate a legacy index/portable library (type source, then
//!   destination; review the plan, `y` executes, `Esc` cancels; in the
//!   migration-complete screen, `a` opens the migrated library), `B`
//!   explains that legacy runtime switching is disabled, `A` acknowledges
//!   a recovered portable library only after explicit confirmation,
//!   `X` reconciles the portable catalogue with the filesystem
//!   (missing records are kept and possible locations are never
//!   auto-applied)
//!
//! Startup uses `Controller::open_default` (canonical by default). Opening,
//! creating, migrating, or switching libraries only ever happens through the
//! explicit keys above, and a failed/cancelled switch leaves the active
//! library untouched.

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use flux_app::{
    AudioStatus, CanonicalMigrationPlan, Controller, ImportReport, ReconcileReport, StorageMode,
    TrackMetadataEdit, format_duration, is_track_playable,
};
use flux_core::{ArtistId, MILESTONE, PlaybackState, Track, TrackId};
use unicode_width::UnicodeWidthChar;

fn main() -> ExitCode {
    let initial_dir = std::env::args_os().nth(1).map(PathBuf::from);
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("{}", flux_core::startup_message());
        eprintln!("flux-tui needs an interactive terminal (stdin/stdout are not TTYs).");
        return ExitCode::FAILURE;
    }
    match run(initial_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("flux-tui: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Restores the user's terminal: show cursor, leave the alternate
/// screen, disable raw mode. Called on every exit path, including
/// panics.
fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = execute!(out, Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(err) = execute!(std::io::stdout(), EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    ScanInput,
    SearchInput,
    /// Typing a folder whose portable library to open (`O`).
    OpenInput,
    /// Typing a folder for a new portable library (`C`).
    CreateInput,
    /// Typing semicolon-separated audio file paths (`i`/`I` copy into the library).
    ImportInput,
    /// Editing the selected track as `title|album|track|disc|artist-id`.
    TrackEditInput,
    /// Creating a stable artist by name (`P`).
    ArtistCreateInput,
    /// Renaming an artist as `artist-id|new-name` (`Y`).
    ArtistRenameInput,
    /// Typing the legacy index file to migrate (`M`, step 1).
    MigrateSourceInput,
    /// Typing the migration destination folder (`M`, step 2).
    MigrateDestInput,
    /// A migration plan awaits confirmation: `y` executes, anything
    /// else cancels (planning wrote nothing).
    MigrateConfirm,
    /// A migration finished: `a` opens the result, anything else
    /// keeps the current library.
    MigratedDone,
    /// Read-only managed lyrics for the selected track (`y`).
    LyricsView,
    /// Scrollable help or import-result details.
    InfoView,
    /// Stable QueueEntryId list with playback, removal and reordering.
    QueueView,
    /// Explicit confirmation before preserving and replacing a protected index.
    ConfirmPreserveIndex,
    /// Explicit confirmation before acknowledging portable recovery.
    ConfirmAcknowledgeRecovery,
}

struct RowsCache {
    session: u64,
    generation: u64,
    search: String,
    favorites_only: bool,
    ids: Rc<Vec<TrackId>>,
}

struct Tui {
    controller: Controller,
    /// Cursor position within the currently visible (filtered) rows.
    cursor: usize,
    /// First visible row index (scroll window).
    offset: usize,
    mode: Mode,
    /// Text being typed in an input mode.
    input: String,
    /// The active library filter (empty = everything).
    search: String,
    /// Cached visible IDs for the current library/search/favorites key.
    rows_cache: RefCell<Option<RowsCache>>,
    /// Show only favorite tracks.
    favorites_only: bool,
    /// Legacy index file chosen at migration step 1.
    migrate_source: PathBuf,
    /// Planned-but-unconfirmed migration (nothing written yet).
    pending_plan: Option<CanonicalMigrationPlan>,
    /// Destination of a completed migration, offered for activation.
    migrated_dest: Option<PathBuf>,
    /// Latest library-lifecycle outcome (plan summary, errors, …).
    lifecycle_msg: Option<String>,
    /// Kept true: canonical imports always copy; no external references exist.
    import_copy: bool,
    /// Last reconciliation, used by the explicit `L` unique-relink action.
    last_reconcile: Option<ReconcileReport>,
    /// Read-only lyrics lines for the selected track.
    lyrics_lines: Vec<String>,
    /// First visible lyrics line.
    lyrics_scroll: usize,
    /// Human-readable lyrics title/status.
    lyrics_title: String,
    /// Cursor within the stable queue-entry list.
    queue_cursor: usize,
    /// First visible queue row.
    queue_offset: usize,
    /// Title and lines for help/import-detail info views.
    info_title: String,
    info_lines: Vec<String>,
    info_scroll: usize,
}

impl Tui {
    /// IDs of the visible rows. Only IDs are collected; track metadata
    /// is looked up for the handful of rows actually drawn. Cloning every
    /// `Track` on each redraw was unnecessary work for large libraries.
    fn rows(&self) -> Rc<Vec<TrackId>> {
        let session = self.controller.library_session();
        let generation = self.controller.generation();
        if let Some(cache) = self.rows_cache.borrow().as_ref()
            && cache.session == session
            && cache.generation == generation
            && cache.search == self.search
            && cache.favorites_only == self.favorites_only
        {
            return Rc::clone(&cache.ids);
        }
        let ids = Rc::new(
            self.controller
                .search(&self.search)
                .into_iter()
                .filter(|(id, _)| {
                    !self.favorites_only
                        || self
                            .controller
                            .rating(*id)
                            .is_some_and(|rating| rating.favorite)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
        );
        *self.rows_cache.borrow_mut() = Some(RowsCache {
            session,
            generation,
            search: self.search.clone(),
            favorites_only: self.favorites_only,
            ids: Rc::clone(&ids),
        });
        ids
    }

    fn library_full_identity(&self) -> String {
        match self.controller.storage_mode() {
            StorageMode::Legacy => self.controller.legacy_index_path().map_or_else(
                || "legacy index (session-only)".to_owned(),
                |path| format!("legacy index {}", path.display()),
            ),
            StorageMode::Portable => self.controller.portable_root().map_or_else(
                || "portable library".to_owned(),
                |root| format!("portable {}", root.display()),
            ),
            StorageMode::Canonical => self.controller.canonical_root().map_or_else(
                || "canonical library".to_owned(),
                |root| format!("canonical {}", root.display()),
            ),
        }
    }

    fn library_header_identity(&self, width: usize) -> String {
        let full = self.library_full_identity();
        if width >= 96 {
            return full;
        }
        let path = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .or_else(|| self.controller.legacy_index_path());
        let Some(path) = path else {
            return full;
        };
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        match self.controller.storage_mode() {
            StorageMode::Legacy => format!("legacy {name}"),
            StorageMode::Portable => format!("portable {name}"),
            StorageMode::Canonical => format!("canonical {name}"),
        }
    }

    fn empty_rows_message(&self) -> String {
        if self.controller.tracks().is_empty() {
            return "Library is empty — press o to scan a folder, i to import files, or ? for help."
                .to_owned();
        }
        if self.favorites_only {
            if self.search.trim().is_empty() {
                "No favorite tracks — press F to show all tracks.".to_owned()
            } else {
                "No favorite tracks match the current search — Esc clears search, F shows all."
                    .to_owned()
            }
        } else if self.search.trim().is_empty() {
            "No tracks in this library view — press ? for help.".to_owned()
        } else {
            "No tracks match the current search — Esc clears it.".to_owned()
        }
    }

    fn open_info(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.info_title = title.into();
        self.info_lines = lines;
        self.info_scroll = 0;
        self.mode = Mode::InfoView;
    }

    fn open_queue_view(&mut self) {
        self.queue_cursor = self
            .controller
            .current_queue_entry_id()
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .unwrap_or(0)
            .min(self.controller.queue_len().saturating_sub(1));
        self.queue_offset = self.queue_cursor;
        self.mode = Mode::QueueView;
    }

    fn event_loop(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        loop {
            self.controller.poll();
            self.draw(out)?;
            // Windows terminals report key releases too; act only on
            // presses (and repeats, for held keys).
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && self.handle_key(key)
            {
                return Ok(());
            }
        }
    }

    fn handle_info_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c || key.code == KeyCode::Char('q') {
            return true;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('Q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.info_scroll = self.info_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.info_scroll = self.info_scroll.saturating_add(1);
            }
            KeyCode::PageUp => self.info_scroll = self.info_scroll.saturating_sub(10),
            KeyCode::PageDown => self.info_scroll = self.info_scroll.saturating_add(10),
            KeyCode::Home | KeyCode::Char('g') => self.info_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.info_scroll = self.info_lines.len().saturating_sub(1);
            }
            _ => {}
        }
        false
    }

    fn handle_queue_view_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c {
            return true;
        }
        let len = self.controller.queue_len();
        self.queue_cursor = self.queue_cursor.min(len.saturating_sub(1));
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            let delta = if key.code == KeyCode::Up { -1 } else { 1 };
            if let Some(entry) = self
                .controller
                .queue_entries()
                .get(self.queue_cursor)
                .copied()
            {
                match self.controller.move_queue_entry_by(entry.id(), delta) {
                    Ok(true) => {
                        if let Some(index) = self.controller.queue_entry_index(entry.id()) {
                            self.queue_cursor = index;
                        }
                        self.lifecycle_msg = Some(format!(
                            "Moved queue entry #{} by one position.",
                            entry.id()
                        ));
                    }
                    Ok(false) => {
                        self.lifecycle_msg = Some(
                            "That queue entry cannot move further in that direction.".to_owned(),
                        );
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            } else {
                self.lifecycle_msg = Some("The queue is empty.".to_owned());
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc | KeyCode::Char('Q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.queue_cursor = self.queue_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.queue_cursor = (self.queue_cursor + 1).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => self.queue_cursor = self.queue_cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.queue_cursor = (self.queue_cursor + 10).min(len.saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.queue_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.queue_cursor = len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(entry) = self
                    .controller
                    .queue_entries()
                    .get(self.queue_cursor)
                    .copied()
                {
                    match self.controller.play_queue_entry(entry.id()) {
                        Ok(()) => {
                            self.lifecycle_msg =
                                Some(format!("Playing queue entry #{}.", entry.id()));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("The queue is empty.".to_owned());
                }
            }
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some(entry) = self
                    .controller
                    .queue_entries()
                    .get(self.queue_cursor)
                    .copied()
                {
                    match self.controller.remove_queue_entry(entry.id()) {
                        Ok(true) => {
                            self.queue_cursor = self
                                .queue_cursor
                                .min(self.controller.queue_len().saturating_sub(1));
                            self.lifecycle_msg =
                                Some(format!("Removed queue entry #{}.", entry.id()));
                        }
                        Ok(false) => {
                            self.lifecycle_msg =
                                Some("That queue entry was already removed.".to_owned());
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("The queue is empty.".to_owned());
                }
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
            KeyCode::Char('s') => self.controller.stop(),
            KeyCode::Char('n') => {
                self.controller.next();
                self.follow_current_queue();
            }
            KeyCode::Char('p') => {
                self.controller.previous();
                self.follow_current_queue();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.controller.adjust_volume(0.05),
            KeyCode::Char('-') => self.controller.adjust_volume(-0.05),
            _ => {}
        }
        false
    }

    /// Returns true when the user asked to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if self.mode == Mode::LyricsView {
            if ctrl_c || key.code == KeyCode::Char('q') {
                return true;
            }
            match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_add(1);
                }
                KeyCode::PageUp => self.lyrics_scroll = self.lyrics_scroll.saturating_sub(10),
                KeyCode::PageDown => self.lyrics_scroll = self.lyrics_scroll.saturating_add(10),
                KeyCode::Home | KeyCode::Char('g') => self.lyrics_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.lyrics_scroll = self.lyrics_lines.len().saturating_sub(1);
                }
                _ => {}
            }
            return false;
        }
        match self.mode {
            Mode::InfoView => return self.handle_info_key(key, ctrl_c),
            Mode::QueueView => return self.handle_queue_view_key(key, ctrl_c),
            _ => {}
        }
        if self.mode != Mode::Normal {
            if ctrl_c {
                return true;
            }
            return self.handle_input_key(key, ctrl_c);
        }
        if ctrl_c {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    if let Some(entry) = self.controller.current_queue_entry_id() {
                        let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                        let _ = self.controller.move_queue_entry_by(entry, delta);
                    } else {
                        self.lifecycle_msg = Some("The queue is empty.".to_owned());
                    }
                }
                _ => {}
            }
            return false;
        }
        let len = self.rows().len();
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.cursor = (self.cursor + 10).min(len.saturating_sub(1));
            }
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    let title = self
                        .controller
                        .track(id)
                        .map(display_title)
                        .unwrap_or_else(|| format!("Track {id}"));
                    self.controller.play_track(id);
                    self.lifecycle_msg = Some(format!("Playing {title}."));
                }
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
            KeyCode::Char('a') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    match self.controller.enqueue_track(id) {
                        Ok(entry) => {
                            self.lifecycle_msg = Some(format!("Added queue entry #{entry}."));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to add to the queue.".to_owned());
                }
            }
            KeyCode::Char('x') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    match self.controller.play_next_track(id) {
                        Ok(entry) => {
                            self.lifecycle_msg =
                                Some(format!("Queue entry #{entry} will play next."));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to play next.".to_owned());
                }
            }
            KeyCode::Char('s') => self.controller.stop(),
            KeyCode::Char('n') => {
                self.controller.next();
                self.follow_current();
            }
            KeyCode::Char('p') => {
                self.controller.previous();
                self.follow_current();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.controller.adjust_volume(0.05),
            KeyCode::Char('-') => self.controller.adjust_volume(-0.05),
            KeyCode::Char('R') => self.controller.cycle_repeat_mode(),
            KeyCode::Char('K') => self.toggle_crossfade(),
            KeyCode::Char(']') => self.adjust_crossfade_up(),
            KeyCode::Char('[') => self.adjust_crossfade_down(),
            KeyCode::Char('o') => {
                self.mode = Mode::ScanInput;
                self.input.clear();
            }
            KeyCode::Char('i') => {
                self.mode = Mode::ImportInput;
                self.import_copy = true;
                self.input.clear();
            }
            KeyCode::Char('I') => {
                self.mode = Mode::ImportInput;
                self.import_copy = true;
                self.input.clear();
            }
            KeyCode::Char('e') => {
                if let Some(id) = self.rows().get(self.cursor).copied()
                    && let Some(track) = self.controller.track(id)
                {
                    self.input = track_edit_form(&self.controller, id, track);
                    self.mode = Mode::TrackEditInput;
                } else {
                    self.lifecycle_msg = Some("Select a track to edit.".to_owned());
                }
            }
            KeyCode::Char('y') => {
                let id = self
                    .rows()
                    .get(self.cursor)
                    .copied()
                    .or_else(|| self.controller.current_id());
                let Some(id) = id else {
                    self.lifecycle_msg = Some("Select a track to inspect lyrics.".to_owned());
                    return false;
                };
                let Some(raw) = self.controller.track_lyrics(id) else {
                    self.lifecycle_msg = Some(format!("Track {id} has no managed lyrics."));
                    return false;
                };
                let document = match flux_app::LyricsDocument::parse(&raw) {
                    Ok(document) => document,
                    Err(error) => {
                        self.lifecycle_msg = Some(format!("Lyrics cannot be parsed: {error}"));
                        return false;
                    }
                };
                self.lyrics_title = self
                    .controller
                    .track(id)
                    .map(|track| {
                        format!(
                            "Lyrics · {} · {} · {}",
                            display_title(track),
                            credit_display(&self.controller, id),
                            match document.kind() {
                                flux_app::LyricsKind::Synced => "synchronized",
                                flux_app::LyricsKind::Plain => "plain text",
                            }
                        )
                    })
                    .unwrap_or_else(|| format!("Lyrics for track {id}"));
                self.lyrics_lines = document
                    .lines
                    .iter()
                    .map(|line| match line.timestamp_ms {
                        Some(timestamp) => {
                            format!("{}  {}", format_lyrics_timestamp(timestamp), line.text)
                        }
                        None => line.text.clone(),
                    })
                    .collect();
                if self.lyrics_lines.is_empty() {
                    self.lyrics_lines.push("(empty lyrics file)".to_owned());
                }
                self.lyrics_scroll = 0;
                self.mode = Mode::LyricsView;
            }
            KeyCode::Char('P') => {
                self.mode = Mode::ArtistCreateInput;
                self.input.clear();
            }
            KeyCode::Char('Y') => {
                self.mode = Mode::ArtistRenameInput;
                self.input.clear();
            }
            KeyCode::Char('V') => {
                let artists = self.controller.artists();
                if artists.is_empty() {
                    self.lifecycle_msg = Some(
                        "No stable artists yet (artist metadata requires a canonical library)."
                            .to_owned(),
                    );
                } else {
                    let mut lines = vec![
                        "Artist relationships are derived from stable ArtistId references. Renaming \
                         an artist updates every Album and Track view without rewriting audio."
                            .to_owned(),
                        String::new(),
                    ];
                    for artist in artists {
                        let owned = self.controller.albums_owned_by(artist.id());
                        let credited = self.controller.albums_credited_for(artist.id());
                        let track_count = self
                            .controller
                            .tracks()
                            .iter()
                            .filter(|(id, _)| {
                                self.controller.track_artist_id(*id) == Some(artist.id())
                                    || self.controller.track_credits(*id).contains(&artist.id())
                            })
                            .count();
                        lines.push(format!(
                            "ArtistId {} · {}{}",
                            artist.id(),
                            artist.name(),
                            if artist.aliases().is_empty() {
                                String::new()
                            } else {
                                format!(" · aliases: {}", artist.aliases().join(", "))
                            }
                        ));
                        lines.push(format!(
                            "  description: {} · image: {} · owned albums: {} · credited-only albums: {} · tracks: {}",
                            if self
                                .controller
                                .artist_description(artist.id())
                                .is_some_and(|text| !text.trim().is_empty())
                            {
                                "yes"
                            } else {
                                "no"
                            },
                            if self
                                .controller
                                .artist_image_path(artist.id())
                                .is_some_and(|path| path.is_file())
                            {
                                "yes"
                            } else {
                                "no"
                            },
                            owned.len(),
                            credited.len(),
                            track_count,
                        ));
                        for album_id in owned.iter().chain(credited.iter()) {
                            if let Some(album) = self.controller.album(*album_id) {
                                lines.push(format!(
                                    "  · AlbumId {} “{}” · {} track(s){}",
                                    album.id,
                                    album.title,
                                    album.track_order.len(),
                                    if owned.contains(album_id) {
                                        ""
                                    } else {
                                        " · credited only"
                                    }
                                ));
                            }
                        }
                        lines.push(String::new());
                    }
                    lines.push(
                        "TUI: Y renames an existing artist; P creates an artist. Description, \
                         image, ownership transfer and deletion require the GUI."
                            .to_owned(),
                    );
                    self.open_info("Artists", lines);
                }
            }
            KeyCode::Char('c') => {
                self.controller.dismiss_notice();
                self.lifecycle_msg = None;
            }
            KeyCode::Char('f') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    match self.controller.toggle_track_favorite(id) {
                        Ok(favorite) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some(format!(
                                "Track {id} {} favorite.",
                                if favorite { "marked" } else { "removed from" }
                            ));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to toggle favorite.".to_owned());
                }
            }
            KeyCode::Char(digit @ '0'..='5') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    let rating = (digit != '0').then(|| digit.to_digit(10).unwrap_or(0) as u8);
                    match self.controller.set_track_rating(id, rating) {
                        Ok(()) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some(match rating {
                                Some(stars) => format!("Rated track {id} {stars}★."),
                                None => format!("Cleared rating for track {id}."),
                            });
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to rate.".to_owned());
                }
            }
            KeyCode::Char('F') => {
                self.favorites_only = !self.favorites_only;
                self.cursor = 0;
                self.offset = 0;
                self.lifecycle_msg = Some(if self.favorites_only {
                    "Showing favorites only (F toggles).".to_owned()
                } else {
                    "Showing all tracks (F toggles favorites).".to_owned()
                });
            }
            KeyCode::Char('H') => {
                let history = self.controller.playback_history();
                if history.is_empty() {
                    self.lifecycle_msg = Some(
                        "No meaningful playback history yet; history starts when this feature is used."
                            .to_owned(),
                    );
                } else {
                    let mut entries = history.entries().collect::<Vec<_>>();
                    entries.sort_by(|(left_id, left), (right_id, right)| {
                        right
                            .play_count
                            .cmp(&left.play_count)
                            .then_with(|| right.last_played_unix_ms.cmp(&left.last_played_unix_ms))
                            .then_with(|| left_id.cmp(right_id))
                    });
                    let summary = entries
                        .into_iter()
                        .take(5)
                        .map(|(id, entry)| {
                            format!(
                                "{}={}",
                                self.controller
                                    .track(id)
                                    .map(display_title)
                                    .unwrap_or_else(|| format!("Track {id}")),
                                entry.play_count
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.lifecycle_msg = Some(format!(
                        "Most played: {summary}. Total meaningful plays: {}.",
                        history.total_plays()
                    ));
                }
            }
            KeyCode::Char('L') => self.relink_first_unique(),
            KeyCode::Char('r') => self.controller.refresh(),
            KeyCode::Char('O') => {
                self.mode = Mode::OpenInput;
                self.input.clear();
            }
            KeyCode::Char('C') => {
                self.mode = Mode::CreateInput;
                self.input.clear();
            }
            KeyCode::Char('M') => {
                self.mode = Mode::MigrateSourceInput;
                self.input.clear();
            }
            KeyCode::Char('E') => {
                let albums = self.controller.albums();
                let standalone = self
                    .controller
                    .tracks()
                    .iter()
                    .filter(|(id, _)| self.controller.track_album_id(*id).is_none())
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                if albums.is_empty() && standalone.is_empty() {
                    self.lifecycle_msg =
                        Some("No canonical albums or standalone tracks.".to_owned());
                } else {
                    let mut lines = vec![
                        "Album membership and order are canonical; Track.album_id and \
                         Album.track_order are validated together."
                            .to_owned(),
                        String::new(),
                    ];
                    for album in albums {
                        let owner = self
                            .controller
                            .artist(album.owner_artist_id)
                            .map(|artist| format!("{} (ArtistId {})", artist.name(), artist.id()))
                            .unwrap_or_else(|| format!("ArtistId {}", album.owner_artist_id));
                        lines.push(format!(
                            "AlbumId {} · “{}” · {} · owner {} · artwork {} · {} track(s)",
                            album.id,
                            album.title,
                            album.release_type.as_str(),
                            owner,
                            if album.cover.is_some() { "yes" } else { "no" },
                            album.track_order.len(),
                        ));
                        for (position, track_id) in album.track_order.iter().enumerate() {
                            let title = self
                                .controller
                                .track(*track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  {}. {} · TrackId {}{}{}",
                                position + 1,
                                title,
                                track_id,
                                if self.controller.track_artwork_path(*track_id).is_some() {
                                    " · Track artwork override"
                                } else {
                                    ""
                                },
                                if self.controller.track_lyrics_path(*track_id).is_some() {
                                    " · lyrics"
                                } else {
                                    ""
                                },
                            ));
                        }
                        lines.push(String::new());
                    }
                    if !standalone.is_empty() {
                        lines.push(format!(
                            "Standalone Tracks · no Album membership · {} track(s)",
                            standalone.len()
                        ));
                        for track_id in standalone {
                            let title = self
                                .controller
                                .track(track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  · {} · TrackId {}{}{}",
                                title,
                                track_id,
                                if self.controller.track_artwork_path(track_id).is_some() {
                                    " · Track artwork"
                                } else {
                                    ""
                                },
                                if self.controller.track_lyrics_path(track_id).is_some() {
                                    " · lyrics"
                                } else {
                                    ""
                                },
                            ));
                        }
                        lines.push(String::new());
                    }
                    lines.push(
                        "Empty albums are retained deliberately. TUI can inspect membership; \
                         rename, artwork, reorder, add/move/remove Tracks and ownership transfer \
                         require the GUI."
                            .to_owned(),
                    );
                    self.open_info("Albums", lines);
                }
            }
            KeyCode::Char('D') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    if let Some(album) = self
                        .controller
                        .track_album_id(id)
                        .and_then(|album_id| self.controller.album(album_id))
                    {
                        let owner = self
                            .controller
                            .artist(album.owner_artist_id)
                            .map(|artist| artist.name().to_owned())
                            .unwrap_or_else(|| format!("Artist {}", album.owner_artist_id));
                        let credited = self.controller.albums_credited_for(album.owner_artist_id);
                        let mut lines = vec![
                            format!("AlbumId {} · “{}”", album.id, album.title),
                            format!(
                                "{} · owner {} (ArtistId {}) · artwork {}",
                                album.release_type.as_str(),
                                owner,
                                album.owner_artist_id,
                                if album.cover.is_some() { "yes" } else { "no" },
                            ),
                            format!(
                                "{} ordered track(s) · credited-only albums for owner: {}",
                                album.track_order.len(),
                                credited.len()
                            ),
                            String::new(),
                        ];
                        for (position, track_id) in album.track_order.iter().enumerate() {
                            let title = self
                                .controller
                                .track(*track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  {}. {} · TrackId {} · artwork {} · lyrics {}",
                                position + 1,
                                title,
                                track_id,
                                if self.controller.track_artwork_path(*track_id).is_some() {
                                    "Track override"
                                } else if album.cover.is_some() {
                                    "inherited Album"
                                } else {
                                    "none"
                                },
                                if self.controller.track_lyrics_path(*track_id).is_some() {
                                    "yes"
                                } else {
                                    "no"
                                },
                            ));
                        }
                        lines.push(String::new());
                        lines.push(
                            "TUI is inspection-only here. Album metadata/artwork, order, \
                         membership and Track movement use the GUI transactional workflows."
                                .to_owned(),
                        );
                        self.open_info("Selected album", lines);
                    } else if let Some(track) = self.controller.track(id) {
                        let mut lines = vec![
                            format!("Standalone Track · TrackId {id}"),
                            format!(
                                "Title: {} · artist: {} · artwork: {} · lyrics: {}",
                                display_title(track),
                                credit_display(&self.controller, id),
                                if self.controller.track_artwork_path(id).is_some() {
                                    "Track override"
                                } else {
                                    "generated fallback (no Album inheritance)"
                                },
                                if self.controller.track_lyrics_path(id).is_some() {
                                    "yes"
                                } else {
                                    "no"
                                },
                            ),
                            String::new(),
                            "This Track has no Album membership. Album artwork cannot be inherited; \
                             playback, queue, history, ratings and playlists remain available."
                                .to_owned(),
                            "TUI is inspection-only here. Adding it to an Album or moving it use \
                             the GUI transactional membership workflow."
                                .to_owned(),
                        ];
                        lines.push(String::new());
                        self.open_info("Selected standalone Track", lines);
                    } else {
                        self.lifecycle_msg =
                            Some("The selected track is no longer present.".to_owned());
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                }
            }
            KeyCode::Char('T') => {
                self.lifecycle_msg = Some(
                    "Ownership transfer and artist deletion require the GUI review/confirm \
                     workflow; the TUI never bypasses canonical transaction validation."
                        .to_owned(),
                );
            }
            KeyCode::Char('B') => {
                self.lifecycle_msg = Some(
                    "Legacy runtime switching is disabled. Migrate an old index with M; \
                     the source is left untouched."
                        .to_owned(),
                );
            }
            KeyCode::Char('U') => {
                if !self.controller.legacy_index_write_protected() {
                    self.lifecycle_msg =
                        Some("The active library index is not write-protected.".to_owned());
                } else {
                    self.lifecycle_msg = Some(
                        "U will move the unreadable index to a preserved backup and enable \
                         scanning/saving. Press y to confirm, Esc to cancel."
                            .to_owned(),
                    );
                    self.mode = Mode::ConfirmPreserveIndex;
                }
            }
            KeyCode::Char('A') => {
                if self.controller.portable_recovery().is_none() {
                    self.lifecycle_msg =
                        Some("There is no portable recovery marker to acknowledge.".to_owned());
                } else {
                    self.lifecycle_msg = Some(
                        "A will replace the portable catalogue from its preserved backup and \
                         re-enable writes. Press y to confirm, Esc to cancel."
                            .to_owned(),
                    );
                    self.mode = Mode::ConfirmAcknowledgeRecovery;
                }
            }
            KeyCode::Char('?') => {
                let library = self.library_full_identity();
                let lines = help_lines(&self.controller, &library);
                self.open_info("Flux TUI help", lines);
            }
            KeyCode::Char('Q') => self.open_queue_view(),
            KeyCode::Char('X') => {
                if self.controller.storage_mode() == StorageMode::Canonical {
                    self.controller.refresh();
                    self.lifecycle_msg = Some(
                        "Refreshed the canonical library and validated its resources.".to_owned(),
                    );
                } else if let Ok(report) = self.controller.reconcile_portable_library() {
                    self.lifecycle_msg = Some(reconcile_summary(&report));
                    self.last_reconcile = Some(report);
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::SearchInput;
                self.input.clone_from(&self.search);
            }
            KeyCode::Esc => {
                if self.search.is_empty() {
                    self.lifecycle_msg = Some(
                        "No active search. Esc cancels an input or closes a view; / starts a search."
                            .to_owned(),
                    );
                } else {
                    self.search.clear();
                    self.cursor = 0;
                    self.lifecycle_msg = Some("Search cleared.".to_owned());
                }
            }
            KeyCode::Char(command) => {
                self.lifecycle_msg =
                    Some(format!("Unknown command '{command}'. Press ? for help."));
            }
            _ => {}
        }
        false
    }

    fn crossfade_state_message(&self) -> String {
        if self.controller.crossfade_enabled() {
            format!(
                "Crossfade on · {:.1} s · shared with the GUI.",
                self.controller.crossfade_duration_ms() as f64 / 1_000.0
            )
        } else {
            format!(
                "Crossfade off · remembered {:.1} s · natural transitions are gapless.",
                self.controller.crossfade_duration_ms() as f64 / 1_000.0
            )
        }
    }

    fn toggle_crossfade(&mut self) {
        let enabled = !self.controller.crossfade_enabled();
        self.lifecycle_msg = Some(match self.controller.set_crossfade_enabled(enabled) {
            Ok(()) => self.crossfade_state_message(),
            Err(error) => error,
        });
    }

    fn adjust_crossfade_up(&mut self) {
        if !self.controller.crossfade_enabled() {
            self.lifecycle_msg = Some(match self.controller.set_crossfade_enabled(true) {
                Ok(()) => self.crossfade_state_message(),
                Err(error) => error,
            });
            return;
        }
        let next = self
            .controller
            .crossfade_duration_ms()
            .saturating_add(250)
            .min(flux_app::CROSSFADE_MAX_MS);
        self.lifecycle_msg = Some(match self.controller.set_crossfade_duration_ms(next) {
            Ok(_) => self.crossfade_state_message(),
            Err(error) => error,
        });
    }

    fn adjust_crossfade_down(&mut self) {
        if !self.controller.crossfade_enabled() {
            self.lifecycle_msg = Some("Crossfade is already off.".to_owned());
            return;
        }
        let current = self.controller.crossfade_duration_ms();
        let next = current.saturating_sub(250);
        self.lifecycle_msg = Some(if next == 0 {
            match self.controller.set_crossfade_enabled(false) {
                Ok(()) => self.crossfade_state_message(),
                Err(error) => error,
            }
        } else {
            match self.controller.set_crossfade_duration_ms(next) {
                Ok(_) => self.crossfade_state_message(),
                Err(error) => error,
            }
        });
    }

    fn handle_input_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        // The two migration decision modes are single-key, not text.
        match self.mode {
            Mode::MigrateConfirm => {
                if key.code == KeyCode::Char('y') {
                    self.execute_pending_migration();
                } else {
                    self.pending_plan = None;
                    self.lifecycle_msg =
                        Some("Migration cancelled — nothing was written.".to_owned());
                    self.mode = Mode::Normal;
                }
                return false;
            }
            Mode::MigratedDone => {
                let mut activated = false;
                let mut activation_error = None;
                if key.code == KeyCode::Char('a')
                    && let Some(destination) = self.migrated_dest.clone()
                {
                    match self.controller.switch_to_canonical(&destination) {
                        Ok(()) => {
                            self.after_switch();
                            activated = true;
                        }
                        Err(error) => {
                            activation_error = Some(format!(
                                "Migrated library activation failed: {error}. The current \
                                 library remains active."
                            ));
                        }
                    }
                }
                if !activated {
                    self.lifecycle_msg = activation_error.or_else(|| {
                        Some(
                            "Migrated library was not activated; the current library remains \
                             active."
                                .to_owned(),
                        )
                    });
                }
                self.migrated_dest = None;
                self.mode = Mode::Normal;
                return false;
            }
            Mode::ConfirmPreserveIndex => {
                if key.code == KeyCode::Char('y') {
                    self.confirm_preserve_index();
                } else {
                    self.lifecycle_msg = Some(
                        "Index replacement cancelled; the protected file is unchanged.".to_owned(),
                    );
                    self.mode = Mode::Normal;
                }
                return false;
            }
            Mode::ConfirmAcknowledgeRecovery => {
                if key.code == KeyCode::Char('y') {
                    self.confirm_acknowledge_recovery();
                } else {
                    self.lifecycle_msg = Some(
                        "Recovery acknowledgement cancelled; the library remains read-only."
                            .to_owned(),
                    );
                    self.mode = Mode::Normal;
                }
                return false;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Esc => {
                // Cancelling an input never changes the active
                // library and abandons any in-flight plan.
                let cancelled = self.mode;
                self.pending_plan = None;
                self.mode = Mode::Normal;
                if let Some(message) = cancel_message(cancelled) {
                    self.lifecycle_msg = Some(message.to_owned());
                }
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                let mut next = Mode::Normal;
                match self.mode {
                    Mode::ScanInput => {
                        let path = text.trim();
                        if path.is_empty() {
                            self.lifecycle_msg =
                                Some("No folder path entered; nothing was scanned.".to_owned());
                        } else {
                            match self.controller.import_folder(Path::new(path), true) {
                                Ok(report) => {
                                    self.lifecycle_msg =
                                        Some(import_summary("Folder import", &report));
                                    if !report.issues.is_empty() {
                                        self.open_info(
                                            "Folder import results",
                                            import_detail_lines("Folder import", &report),
                                        );
                                        next = Mode::InfoView;
                                    }
                                }
                                Err(err) => {
                                    self.lifecycle_msg =
                                        Some(format!("Folder import failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::SearchInput => {
                        self.search = text;
                        self.cursor = 0;
                    }
                    Mode::ImportInput => {
                        let paths: Vec<PathBuf> = text
                            .split(';')
                            .map(str::trim)
                            .filter(|path| !path.is_empty())
                            .map(PathBuf::from)
                            .collect();
                        if paths.is_empty() {
                            self.lifecycle_msg =
                                Some("Enter at least one audio file path.".to_owned());
                        } else {
                            match self.controller.import_files(&paths, self.import_copy) {
                                Ok(report) => {
                                    self.lifecycle_msg =
                                        Some(import_summary("File import", &report));
                                    if !report.issues.is_empty() {
                                        self.open_info(
                                            "File import results",
                                            import_detail_lines("File import", &report),
                                        );
                                        next = Mode::InfoView;
                                    }
                                }
                                Err(err) => {
                                    self.lifecycle_msg = Some(format!("File import failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::TrackEditInput => match parse_track_edit_form(&text) {
                        Ok(edit) => {
                            let id = self.rows().get(self.cursor).copied();
                            if let Some(id) = id {
                                match self.controller.edit_track_metadata(id, edit) {
                                    Ok(()) => {
                                        self.lifecycle_msg =
                                            Some(format!("Saved metadata for track {id}."));
                                    }
                                    Err(err) => self.lifecycle_msg = Some(err),
                                }
                            } else {
                                self.lifecycle_msg = Some("The selected track is gone.".to_owned());
                            }
                        }
                        Err(err) => {
                            self.lifecycle_msg = Some(format!(
                                "{err} The edit form remains open so you can correct it."
                            ));
                            self.input = text;
                            next = Mode::TrackEditInput;
                        }
                    },
                    Mode::ArtistCreateInput => {
                        if let Err(err) = self.controller.create_artist(text.trim()) {
                            self.lifecycle_msg = Some(err);
                        } else {
                            self.lifecycle_msg = Some(format!("Created artist {}.", text.trim()));
                        }
                    }
                    Mode::ArtistRenameInput => {
                        let Some((id, name)) = text.split_once('|') else {
                            self.lifecycle_msg = Some(
                                "Use artist-id|new name for rename. The form remains open."
                                    .to_owned(),
                            );
                            self.input = text;
                            next = Mode::ArtistRenameInput;
                            self.mode = next;
                            return false;
                        };
                        match id.trim().parse::<u64>() {
                            Ok(raw) => {
                                if let Err(err) = self
                                    .controller
                                    .rename_artist(ArtistId::new(raw), name.trim())
                                {
                                    self.lifecycle_msg = Some(err);
                                } else {
                                    self.lifecycle_msg = Some(format!("Renamed artist {raw}."));
                                }
                            }
                            Err(_) => {
                                self.lifecycle_msg = Some(
                                    "Artist ID must be a whole number. The form remains open."
                                        .to_owned(),
                                );
                                self.input = text;
                                next = Mode::ArtistRenameInput;
                            }
                        }
                    }
                    Mode::OpenInput => {
                        if text.trim().is_empty() {
                            self.lifecycle_msg = Some(
                                "No library folder entered; active library unchanged.".to_owned(),
                            );
                        } else {
                            let path = PathBuf::from(text.trim());
                            match self.controller.switch_to_canonical(&path) {
                                Ok(()) => self.after_switch(),
                                Err(error) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Open failed: {error}. Active library unchanged."
                                    ));
                                }
                            }
                        }
                    }
                    Mode::CreateInput => {
                        if text.trim().is_empty() {
                            self.lifecycle_msg = Some(
                                "No destination folder entered; active library unchanged."
                                    .to_owned(),
                            );
                        } else {
                            let path = PathBuf::from(text.trim());
                            match self.controller.switch_to_new_canonical_library(&path) {
                                Ok(_) => self.after_switch(),
                                Err(error) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Create failed: {error}. Active library unchanged."
                                    ));
                                }
                            }
                        }
                    }
                    Mode::MigrateSourceInput => {
                        let source = text.trim();
                        if source.is_empty() {
                            self.lifecycle_msg =
                                Some("No legacy index path entered; nothing planned.".to_owned());
                        } else {
                            self.migrate_source = PathBuf::from(source);
                            next = Mode::MigrateDestInput;
                        }
                    }
                    Mode::MigrateDestInput => {
                        let destination = text.trim();
                        if destination.is_empty() {
                            self.lifecycle_msg =
                                Some("No destination folder entered; nothing planned.".to_owned());
                        } else {
                            let destination = PathBuf::from(destination);
                            match flux_app::plan_canonical_migration(
                                &self.migrate_source,
                                &destination,
                            ) {
                                Ok(plan) => {
                                    self.lifecycle_msg = Some(migration_plan_summary(&plan));
                                    self.pending_plan = Some(plan);
                                    self.migrated_dest = None;
                                    next = Mode::MigrateConfirm;
                                }
                                Err(err) => {
                                    self.lifecycle_msg =
                                        Some(format!("Migration planning failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::Normal
                    | Mode::MigrateConfirm
                    | Mode::MigratedDone
                    | Mode::LyricsView
                    | Mode::InfoView
                    | Mode::QueueView
                    | Mode::ConfirmPreserveIndex
                    | Mode::ConfirmAcknowledgeRecovery => {}
                }
                self.mode = next;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(_) if ctrl_c => self.mode = Mode::Normal,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(c);
            }
            _ => {}
        }
        false
    }

    fn confirm_preserve_index(&mut self) {
        self.mode = Mode::Normal;
        match self.controller.replace_unreadable_legacy_index() {
            Ok(backup) => {
                self.lifecycle_msg = Some(format!(
                    "Unreadable index preserved as {}; scanning and saving are enabled.",
                    backup.display()
                ));
            }
            Err(err) => self.lifecycle_msg = Some(format!("Index preservation failed: {err}")),
        }
    }

    fn confirm_acknowledge_recovery(&mut self) {
        self.mode = Mode::Normal;
        match self.controller.acknowledge_portable_recovery() {
            Ok(()) => {
                self.lifecycle_msg = Some(
                    "Portable recovery acknowledged; scanning and saving are enabled.".to_owned(),
                );
            }
            Err(err) => {
                self.lifecycle_msg = Some(format!("Recovery acknowledgement failed: {err}"));
            }
        }
    }

    /// Runs the migration the user confirmed with `y`. Execution
    /// never switches the active library by itself; success moves to
    /// [`Mode::MigratedDone`], where `a` activates the result.
    fn execute_pending_migration(&mut self) {
        let Some(plan) = self.pending_plan.take() else {
            self.mode = Mode::Normal;
            return;
        };
        if !plan.is_ready() {
            self.lifecycle_msg = Some(format!(
                "Migration blocked by {} issue(s); nothing was executed. Fix the source or \
                 destination and plan again.",
                plan.issues().len()
            ));
            self.mode = Mode::Normal;
            return;
        }
        let destination = plan.destination().to_path_buf();
        match flux_app::execute_canonical_migration(&plan) {
            Ok(report) => {
                self.lifecycle_msg = Some(format!(
                    "Canonical migration complete: {} track(s), {} artist(s), {} album(s), \
                     validated. The source is untouched. Press a to activate the \
                     migrated library, Esc to keep the current one.",
                    report.tracks(),
                    report.artists(),
                    report.albums(),
                ));
                self.migrated_dest = Some(destination);
                self.mode = Mode::MigratedDone;
            }
            Err(err) => {
                self.lifecycle_msg = Some(format!("Migration failed: {err}"));
                self.mode = Mode::Normal;
            }
        }
    }

    /// Applies the safe relink suggested by the last reconciliation when
    /// exactly one candidate exists for a missing track. Multiple
    /// candidates remain a manual choice (the GUI exposes each one).
    fn relink_first_unique(&mut self) {
        let Some(report) = self.last_reconcile.take() else {
            self.lifecycle_msg =
                Some("Reconcile with X before relinking a missing file.".to_owned());
            return;
        };
        let Some(candidate) = report
            .relink_candidates
            .iter()
            .find(|candidates| candidates.is_unique())
            .and_then(|candidates| candidates.candidates.first())
            .cloned()
        else {
            self.lifecycle_msg = Some(
                "No unique relink candidate is available; X reports ambiguous ones.".to_owned(),
            );
            return;
        };
        let track = report
            .relink_candidates
            .iter()
            .find(|candidates| candidates.is_unique())
            .map(|candidates| candidates.id);
        if let Some(id) = track {
            match self.controller.relink_portable_track(id, &candidate) {
                Ok(()) => {
                    self.lifecycle_msg = Some(format!(
                        "Relinked track {id} to {}.",
                        candidate.display_lossy()
                    ));
                }
                Err(err) => self.lifecycle_msg = Some(err),
            }
        }
    }

    /// Housekeeping after a successful library switch: cursor,
    /// search, and pending migration state referred to the old
    /// library. (The controller already stopped playback and rebuilt
    /// its queue from the new library.)
    fn after_switch(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.mode = Mode::Normal;
        self.input.clear();
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.last_reconcile = None;
        self.lyrics_lines.clear();
        self.lyrics_scroll = 0;
        self.lyrics_title.clear();
        self.queue_cursor = 0;
        self.queue_offset = 0;
        self.info_title.clear();
        self.info_lines.clear();
        self.info_scroll = 0;
        self.lifecycle_msg = Some(format!("Active library: {}", self.library_full_identity()));
    }

    /// After next/previous, put the cursor on the player's selection.
    fn follow_current(&mut self) {
        if let Some(current) = self.controller.current_id()
            && let Some(pos) = self.rows().iter().position(|id| *id == current)
        {
            self.cursor = pos;
        }
    }

    fn follow_current_queue(&mut self) {
        if let Some(entry) = self.controller.current_queue_entry_id()
            && let Some(index) = self.controller.queue_entry_index(entry)
        {
            self.queue_cursor = index;
        }
    }

    fn draw(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let (raw_width, raw_height) = terminal::size().unwrap_or((80, 24));
        if terminal_too_small(raw_width, raw_height) {
            queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
            if raw_width > 0 {
                self.line(
                    out,
                    0,
                    "Terminal too small — resize to at least 20x6.",
                    usize::from(raw_width),
                    true,
                )?;
            }
            if raw_height > 1 && raw_width > 0 {
                self.line(
                    out,
                    raw_height - 1,
                    "q/Ctrl-C quit",
                    usize::from(raw_width),
                    false,
                )?;
            }
            out.flush()?;
            return Ok(());
        }
        let (width, height) = clamp_terminal_size(raw_width, raw_height);
        // A fresh pty can report 0 columns; lay out for at least a
        // minimal sane width rather than dividing the void.
        let rows = self.rows();
        let current = self.controller.current_id();

        // Clamp cursor/window against the (possibly shrunken) list.
        let list_top = 3usize;
        let list_height = height.saturating_sub(list_top + 2).max(1);
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        if self.cursor >= self.offset + list_height {
            self.offset = self.cursor + 1 - list_height;
        }
        self.offset = self.offset.min(rows.len().saturating_sub(list_height));

        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

        if !detail_view_fits(height)
            && matches!(
                self.mode,
                Mode::InfoView | Mode::QueueView | Mode::LyricsView
            )
        {
            self.line(
                out,
                0,
                "Terminal too small for this view — resize to at least 8 rows or press Esc.",
                width,
                true,
            )?;
            self.line(out, (height - 1) as u16, "Esc back · q quit", width, false)?;
            out.flush()?;
            return Ok(());
        }

        // Header.
        let library_label = self.library_header_identity(width);
        let view_label = match self.mode {
            Mode::QueueView => "queue",
            Mode::InfoView => "help/info",
            Mode::LyricsView => "lyrics",
            Mode::Normal => "tracks",
            _ => "input",
        };
        let mut header = format!(
            "Flux {MILESTONE} [{library_label}] [view {view_label}] [queue {}] [playback {}]",
            self.controller.queue_len(),
            self.controller.playback_mode().label()
        );
        if self.favorites_only {
            header.push_str(" [favorites]");
        }
        if !self.search.trim().is_empty() {
            let search = truncate_to_width(&self.search, 24);
            header.push_str(&format!(" [search \"{search}\"]"));
        }
        if self.controller.portable_recovery().is_some() {
            header.push_str("   [recovered — read-only, A acknowledges]");
        }
        if self.controller.legacy_index_write_protected() {
            header.push_str("   [index protected — U preserves & replaces]");
        }
        if self.controller.is_scanning() {
            header.push_str("   [scanning…]");
        }
        self.line(out, 0, &header, width, true)?;

        // Now playing.
        self.line(out, 1, &self.now_playing_line(width), width, false)?;
        // Audio availability gets its own line so the reason remains
        // readable on narrow terminals.
        if let Some(reason) = self.controller.audio_status().reason() {
            let label = if matches!(self.controller.audio_status(), AudioStatus::Recovering(_)) {
                "Audio recovering"
            } else {
                "Audio unavailable"
            };
            queue!(out, SetForegroundColor(Color::DarkYellow))?;
            self.line(out, 2, &format!("{label} — {reason}"), width, false)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
        } else {
            self.line(out, 2, "", width, false)?;
        }

        let detail_height = height.saturating_sub(list_top + 4).max(1);
        if self.mode == Mode::InfoView {
            self.line(out, list_top as u16, &self.info_title, width, true)?;
            let max_scroll = self.info_lines.len().saturating_sub(detail_height);
            self.info_scroll = self.info_scroll.min(max_scroll);
            for (row, line) in self
                .info_lines
                .iter()
                .skip(self.info_scroll)
                .take(detail_height)
                .enumerate()
            {
                self.line(out, (list_top + 1 + row) as u16, line, width, false)?;
            }
        } else if self.mode == Mode::QueueView {
            let entries = self.controller.queue_entries();
            self.queue_cursor = self.queue_cursor.min(entries.len().saturating_sub(1));
            if self.queue_cursor < self.queue_offset {
                self.queue_offset = self.queue_cursor;
            }
            if self.queue_cursor >= self.queue_offset + detail_height {
                self.queue_offset = self.queue_cursor + 1 - detail_height;
            }
            self.queue_offset = self
                .queue_offset
                .min(entries.len().saturating_sub(detail_height));
            let title = format!(
                "QUEUE · {} entr{} · Enter play · Del remove · Ctrl+↑/↓ reorder · Esc back",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" }
            );
            self.line(out, list_top as u16, &title, width, true)?;
            if entries.is_empty() {
                self.line(
                    out,
                    (list_top + 1) as u16,
                    "Queue is empty — add a track with a or x from the track view.",
                    width,
                    false,
                )?;
            }
            for (row, entry) in entries
                .iter()
                .skip(self.queue_offset)
                .take(detail_height)
                .enumerate()
            {
                let index = self.queue_offset + row;
                let current = self.controller.current_queue_entry_id() == Some(entry.id());
                let track = self.controller.track(entry.track());
                let mut text = format!(
                    "{} {}. #{} {}",
                    if index == self.queue_cursor { ">" } else { " " },
                    index + 1,
                    entry.id(),
                    track.map_or_else(
                        || "Missing from library".to_owned(),
                        |track| format!(
                            "{} — {}",
                            display_title(track),
                            credit_display(&self.controller, entry.track())
                        ),
                    )
                );
                if current {
                    text.push_str("  ▶ current");
                }
                if let Some(label) =
                    resource_label(self.controller.track_audio_resource_state(entry.track()))
                {
                    text.push_str("  ");
                    text.push_str(label);
                }
                let duration = track
                    .and_then(Track::duration)
                    .map(format_duration)
                    .unwrap_or_default();
                let text = pad_with_right(&text, &duration, width);
                let y = (list_top + 1 + row) as u16;
                if index == self.queue_cursor {
                    queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                    self.line(out, y, &text, width, false)?;
                    queue!(out, SetAttribute(Attribute::Reset))?;
                } else {
                    self.line(out, y, &text, width, false)?;
                }
            }
        } else if self.mode == Mode::LyricsView {
            self.line(out, list_top as u16, &self.lyrics_title, width, true)?;
            let max_scroll = self.lyrics_lines.len().saturating_sub(detail_height);
            self.lyrics_scroll = self.lyrics_scroll.min(max_scroll);
            for (row, line) in self
                .lyrics_lines
                .iter()
                .skip(self.lyrics_scroll)
                .take(detail_height)
                .enumerate()
            {
                self.line(out, (list_top + 1 + row) as u16, line, width, false)?;
            }
        } else {
            // Track list.
            if rows.is_empty() {
                let message = self.empty_rows_message();
                self.line(out, list_top as u16, &message, width, false)?;
            }
            for (row, id) in rows.iter().skip(self.offset).take(list_height).enumerate() {
                let index = self.offset + row;
                let Some(track) = self.controller.track(*id) else {
                    continue;
                };
                let mut text = format!(
                    "{} {} {}  · {}",
                    if index == self.cursor { ">" } else { " " },
                    if Some(*id) == current { "▶" } else { " " },
                    display_title(track),
                    credit_display(&self.controller, *id),
                );
                if !is_track_playable(track) {
                    text.push_str("  [indexed — not playable yet]");
                }
                if let Some(label) = resource_label(self.controller.track_audio_resource_state(*id))
                {
                    text.push_str("  ");
                    text.push_str(label);
                }
                if self.controller.track_album_id(*id).is_none() {
                    text.push_str("  [standalone]");
                }
                if self.controller.track_artwork_path(*id).is_some() {
                    text.push_str("  [art]");
                }
                if self.controller.track_lyrics_path(*id).is_some() {
                    text.push_str("  [lyrics]");
                }
                let duration = track.duration().map(format_duration).unwrap_or_default();
                let text = pad_with_right(&text, &duration, width);
                let y = (list_top + row) as u16;
                if index == self.cursor {
                    queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                    self.line(out, y, &text, width, false)?;
                    queue!(out, SetAttribute(Attribute::Reset))?;
                } else {
                    self.line(out, y, &text, width, false)?;
                }
            }
        }

        // Status / error line.
        let status_y = (height - 2) as u16;
        if let Some(error) = self.controller.last_error() {
            queue!(out, SetForegroundColor(Color::Red))?;
            self.line(out, status_y, error, width, false)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
        } else if let Some(message) = &self.lifecycle_msg {
            self.line(out, status_y, message, width, false)?;
        } else if let Some(status) = self.controller.last_status() {
            self.line(out, status_y, status, width, false)?;
        }

        // Help / input line.
        let input_line = |prefix: &str| fit_input_line(prefix, &self.input, width);
        let help = match self.mode {
            Mode::Normal => normal_help().to_owned(),
            Mode::InfoView => format!(
                "Help/details — ↑/↓ scroll · PgUp/PgDn · Esc/Q back · {}-{} of {}",
                self.info_scroll + 1,
                (self.info_scroll + detail_height).min(self.info_lines.len()),
                self.info_lines.len()
            ),
            Mode::QueueView => {
                "Queue — ↑/↓ select · Enter play · Del/d remove · Ctrl+↑/↓ reorder · Esc/Q back"
                    .to_owned()
            }
            Mode::ScanInput => input_line("Scan folder (Enter scan, Esc cancel): "),
            Mode::SearchInput => input_line("Search (Enter apply, Esc cancel): "),
            Mode::ImportInput => {
                input_line("Copy import paths separated by ; (Enter import, Esc cancel): ")
            }
            Mode::TrackEditInput => input_line(
                "Edit track title|album|track|disc|artist-id|credit-ids (Enter save, Esc cancel): ",
            ),
            Mode::ArtistCreateInput => input_line("New artist name (Enter create, Esc cancel): "),
            Mode::ArtistRenameInput => {
                input_line("Rename artist-id|new-name (Enter rename, Esc cancel): ")
            }
            Mode::OpenInput => {
                input_line("Open canonical library folder (Enter switch, Esc cancel): ")
            }
            Mode::CreateInput => {
                input_line("Create canonical library folder (Enter create+switch, Esc cancel): ")
            }
            Mode::MigrateSourceInput => {
                input_line("Legacy index file to migrate (Enter next, Esc cancel): ")
            }
            Mode::MigrateDestInput => {
                input_line("Migration destination folder (Enter plan, Esc cancel): ")
            }
            Mode::MigrateConfirm => {
                "Migration planned — review the status line: y executes, Esc cancels.".to_owned()
            }
            Mode::MigratedDone => {
                "Migration done — a opens the migrated library, Esc keeps the current one."
                    .to_owned()
            }
            Mode::LyricsView => {
                "Lyrics read-only — ↑/↓ scroll · PgUp/PgDn · Esc back · q quits.".to_owned()
            }
            Mode::ConfirmPreserveIndex => {
                "Preserve and replace protected index? y confirms; any other key cancels."
                    .to_owned()
            }
            Mode::ConfirmAcknowledgeRecovery => {
                "Acknowledge portable recovery? y confirms; any other key cancels.".to_owned()
            }
        };
        self.line(out, (height - 1) as u16, &help, width, false)?;

        out.flush()
    }

    fn line(
        &self,
        out: &mut impl Write,
        y: u16,
        text: &str,
        width: usize,
        bold: bool,
    ) -> std::io::Result<()> {
        let text = truncate_to_width(text, width);
        queue!(out, MoveTo(0, y), Clear(ClearType::UntilNewLine))?;
        if bold {
            queue!(
                out,
                SetAttribute(Attribute::Bold),
                Print(text),
                SetAttribute(Attribute::Reset)
            )
        } else {
            queue!(out, Print(text))
        }
    }

    fn now_playing_line(&self, _width: usize) -> String {
        let state_icon = match self.controller.player_state() {
            PlaybackState::Playing => "▶",
            PlaybackState::Paused => "⏸",
            PlaybackState::Stopped => "■",
        };
        let volume = (self.controller.volume() * 100.0).round() as u32;
        let crossfade = if self.controller.crossfade_enabled() {
            format!(
                "Xfade {:.1}s",
                self.controller.crossfade_duration_ms() as f64 / 1_000.0
            )
        } else {
            "Xfade off".to_owned()
        };
        match self.controller.current_track() {
            Some(track) => {
                let total = track
                    .duration()
                    .or_else(|| self.controller.stream_info().and_then(|i| i.duration))
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".to_owned());
                format!(
                    "{state_icon} {}    {} / {total}    Vol {volume}%    {crossfade}",
                    display_title(track),
                    format_duration(self.controller.position()),
                )
            }
            None if self.controller.current_queue_entry_id().is_some() => {
                format!(
                    "{state_icon} Queue entry missing from the library    Vol {volume}%    {crossfade}"
                )
            }
            None => format!("{state_icon} Nothing selected    Vol {volume}%    {crossfade}"),
        }
    }
}

/// What the user reviews before pressing `y`: what will migrate,
/// what is excluded and why. (IDs/counter preservation and the
/// untouched legacy index/audio are guarantees of the shared
/// migration API.)
/// The reconciliation result in one line, including whether
/// possible new locations were found for missing tracks (never
/// applied automatically).
fn reconcile_summary(report: &flux_app::ReconcileReport) -> String {
    let mut summary = format!(
        "Reconciled {}: {} unchanged, {} changed, {} back, {} missing (kept), {} uninspectable.",
        report.inspected_count(),
        report.unchanged_count() + report.established_count(),
        report.changed_count(),
        report.reappeared_count(),
        report.missing_count(),
        report.inspection_error_count() + report.fingerprint_unavailable_count(),
    );
    let with_candidates = report.unique_candidate_count() + report.ambiguous_candidate_count();
    if with_candidates > 0 {
        summary.push_str(&format!(
            " {with_candidates} missing track(s) have possible new locations ({} ambiguous); nothing moved.",
            report.ambiguous_candidate_count()
        ));
    }
    summary
}

fn migration_plan_summary(plan: &CanonicalMigrationPlan) -> String {
    if plan.is_ready() {
        format!(
            "Canonical migration: {} track(s), {} artist(s), {} album(s), about {} MiB. \
             y = execute, Esc = cancel (source untouched).",
            plan.track_count(),
            plan.artist_count(),
            plan.album_count(),
            plan.estimated_bytes() / (1024 * 1024),
        )
    } else {
        format!(
            "Canonical migration blocked by {} issue(s): {}. Resolve them before executing.",
            plan.issues().len(),
            plan.issues()
                .iter()
                .take(3)
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

fn track_edit_form(controller: &Controller, id: TrackId, track: &Track) -> String {
    format!(
        "{} | {} | {} | {} | {}",
        track.title(),
        track.album().unwrap_or_default(),
        track
            .track_number()
            .map(|value| value.to_string())
            .unwrap_or_default(),
        track
            .disc_number()
            .map(|value| value.to_string())
            .unwrap_or_default(),
        controller
            .track_artist_id(id)
            .map(|id| id.to_string())
            .unwrap_or_default(),
    )
}

fn parse_track_edit_form(input: &str) -> Result<TrackMetadataEdit, String> {
    let fields: Vec<&str> = input.split('|').map(str::trim).collect();
    if !(5..=6).contains(&fields.len()) {
        return Err(
            "Track edit format: title|album|track-number|disc-number|artist-id|comma-separated-credit-ids (artist and credit IDs may be empty)."
                .to_owned(),
        );
    }
    let number = |value: &str, field: &str| -> Result<Option<u32>, String> {
        if value.is_empty() {
            Ok(None)
        } else {
            value
                .parse::<u32>()
                .map(Some)
                .map_err(|_| format!("{field} must be a whole number."))
        }
    };
    let artist_id = if fields[4].is_empty() {
        None
    } else {
        Some(ArtistId::new(fields[4].parse::<u64>().map_err(|_| {
            "artist ID must be a whole number.".to_owned()
        })?))
    };
    let credited_artist_ids = fields
        .get(5)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .split(',')
                .map(|id| {
                    id.trim()
                        .parse::<u64>()
                        .map(ArtistId::new)
                        .map_err(|_| "credit IDs must be comma-separated whole numbers.".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(TrackMetadataEdit {
        title: (!fields[0].is_empty()).then(|| fields[0].to_owned()),
        album: (!fields[1].is_empty()).then(|| fields[1].to_owned()),
        track_number: number(fields[2], "track number")?,
        disc_number: number(fields[3], "disc number")?,
        artist_id,
        credited_artist_ids,
    })
}

fn credit_display(controller: &Controller, id: TrackId) -> String {
    let credits = controller.track_credits(id);
    if credits.is_empty() {
        return controller
            .track(id)
            .and_then(Track::artist)
            .unwrap_or("Unknown artist")
            .to_owned();
    }
    credits
        .iter()
        .map(|artist| {
            controller
                .artist(*artist)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {artist}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn display_title(track: &Track) -> String {
    match track.artist() {
        Some(artist) => format!("{} — {artist}", track.title()),
        None => track.title().to_owned(),
    }
}

fn format_lyrics_timestamp(timestamp_ms: u64) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms / 1000) % 60;
    let millis = timestamp_ms % 1000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

fn normal_help() -> &'static str {
    "↑/↓ move · Enter play · / search · f favorite · 0–5 rate · Q queue · ? help · q quit"
}

fn resource_label(state: Option<flux_app::ResourceState>) -> Option<&'static str> {
    match state {
        Some(flux_app::ResourceState::Missing) => Some("[missing]"),
        Some(flux_app::ResourceState::Unavailable(_)) => Some("[inaccessible]"),
        Some(flux_app::ResourceState::Present) | None => None,
    }
}

fn terminal_too_small(width: u16, height: u16) -> bool {
    width < 20 || height < 6
}

fn clamp_terminal_size(width: u16, height: u16) -> (usize, usize) {
    (usize::from(width.max(20)), usize::from(height.max(6)))
}

fn detail_view_fits(height: usize) -> bool {
    height >= 8
}

fn char_width(character: char) -> usize {
    character.width().unwrap_or(0)
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let next = char_width(character);
        if used.saturating_add(next) > width {
            break;
        }
        output.push(character);
        used = used.saturating_add(next);
    }
    output
}

fn fit_input_line(prefix: &str, input: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let line = format!("{prefix}{input}_");
    if display_width(&line) <= width {
        return line;
    }
    let keep = width.saturating_sub(1);
    let mut used = 0usize;
    let mut tail = Vec::new();
    for character in line.chars().rev() {
        let next = char_width(character);
        if used.saturating_add(next) > keep {
            break;
        }
        used = used.saturating_add(next);
        tail.push(character);
    }
    let mut tail = tail.into_iter().rev().collect::<String>();
    tail.insert(0, '…');
    tail
}

fn import_summary(label: &str, report: &ImportReport) -> String {
    format!(
        "{label}: {} added, {} updated, {} already indexed, {} copied, {} issue(s).",
        report.added,
        report.updated,
        report.deduplicated,
        report.copied.len(),
        report.issues.len()
    )
}

fn import_detail_lines(label: &str, report: &ImportReport) -> Vec<String> {
    let mut lines = vec![import_summary(label, report)];
    if report.issues.is_empty() {
        lines.push("No per-path issues were reported.".to_owned());
        return lines;
    }
    lines.push(format!("Per-path issues ({}):", report.issues.len()));
    lines.extend(report.issues.iter().enumerate().map(|(index, issue)| {
        format!(
            "{}. {} — {}",
            index + 1,
            issue.path().display(),
            issue.reason()
        )
    }));
    lines
}

fn cancel_message(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::ScanInput => Some("Scan cancelled; no folder was scanned."),
        Mode::SearchInput => Some("Search input cancelled; the previous filter is unchanged."),
        Mode::ImportInput => Some("Import cancelled; no files were imported."),
        Mode::TrackEditInput => Some("Track edit cancelled; no changes were made."),
        Mode::ArtistCreateInput => Some("Artist creation cancelled."),
        Mode::ArtistRenameInput => Some("Artist rename cancelled; no changes were made."),
        Mode::OpenInput | Mode::CreateInput => {
            Some("Library switch cancelled; the active library is unchanged.")
        }
        Mode::MigrateSourceInput | Mode::MigrateDestInput => {
            Some("Migration cancelled before planning; nothing was written.")
        }
        Mode::Normal
        | Mode::MigrateConfirm
        | Mode::MigratedDone
        | Mode::LyricsView
        | Mode::InfoView
        | Mode::QueueView
        | Mode::ConfirmPreserveIndex
        | Mode::ConfirmAcknowledgeRecovery => None,
    }
}

fn help_lines(controller: &Controller, library: &str) -> Vec<String> {
    let audio = match controller.audio_status() {
        AudioStatus::Ready => "ready".to_owned(),
        AudioStatus::DeviceUnavailable(reason) => format!("unavailable — {reason}"),
        AudioStatus::Recovering(reason) => format!("recovering — {reason}"),
        other => other.reason().unwrap_or("unknown").to_owned(),
    };
    let active = controller.active_audio_device().map_or_else(
        || "unknown device".to_owned(),
        |device| {
            format!(
                "{}{}",
                device.name,
                if device.is_default { " (default)" } else { "" }
            )
        },
    );
    let policy = if controller.selected_audio_device().is_some() {
        "explicit selection"
    } else {
        "system default"
    };
    vec![
        "Flux TUI quick reference".to_owned(),
        format!("Active library: {library}"),
        format!(
            "Audio output: {audio} · {active} · policy {policy} · queue {} · playback mode {}",
            controller.queue_len(),
            controller.playback_mode().label()
        ),
        String::new(),
        "NAVIGATION".to_owned(),
        "↑/↓ or k/j move · PgUp/PgDn · g/G first/last".to_owned(),
        "/ search · Esc clears the active search · F favorites-only filter · c clears notice"
            .to_owned(),
        "f toggles the selected favorite · 0 clears rating · 1–5 sets stars".to_owned(),
        "Q queue view · ? help · q or Ctrl-C quits".to_owned(),
        String::new(),
        "PLAYBACK".to_owned(),
        "Enter play · Space play/pause · s stop · n/p skip".to_owned(),
        "+/- volume · R playback mode · K crossfade · [ ] crossfade duration".to_owned(),
        "Output loss triggers bounded automatic recovery; after exhaustion, Play starts a fresh retry."
            .to_owned(),
        String::new(),
        "QUEUE".to_owned(),
        "a add selected · x play selected next".to_owned(),
        "Q opens the stable queue-entry view".to_owned(),
        "Enter play selected entry · Del/d remove selected entry".to_owned(),
        "Ctrl+↑/↓ reorder selected entry; duplicate tracks remain distinct by #entry id".to_owned(),
        String::new(),
        "CATALOGUE".to_owned(),
        "o scan folder · i/I copy-import files · r refresh".to_owned(),
        "e edit selected metadata · y read managed lyrics".to_owned(),
        "H local history · D selected Track membership · E Album/standalone relationships"
            .to_owned(),
        "V artist/album relationships · P create artist · Y rename artist".to_owned(),
        "X refresh/reconcile · L relink a unique reported move candidate".to_owned(),
        String::new(),
        "LIBRARY LIFECYCLE".to_owned(),
        "O open canonical library · C create canonical library".to_owned(),
        "M migrate a legacy index/portable library".to_owned(),
        "U preserve and replace a protected legacy index (confirmation required)".to_owned(),
        "A acknowledge portable recovery from backup (confirmation required)".to_owned(),
        "B legacy runtime switching is disabled; use M instead".to_owned(),
        String::new(),
        "SAFETY AND LIMITS".to_owned(),
        "Imports are synchronous quick copy operations; there is no TUI cancel/review phase."
            .to_owned(),
        "All per-path import issues are shown in a scrollable result panel.".to_owned(),
        "Missing resources are retained; inaccessible paths are not treated as missing.".to_owned(),
        "The TUI never deletes external audio and never deletes library tracks.".to_owned(),
        "Track deletion, audio/artwork/lyrics repair, ownership/album/artist transfer,".to_owned(),
        "playlists, batch actions and global filters require the GUI review workflows."
            .to_owned(),
        "Audio-output enumeration and device selection are GUI-only; Play retries recovery here."
            .to_owned(),
    ]
}

/// Left text plus right-aligned `right`, space-padded to `width`.
/// Degenerate widths (a 0-column pty, `right` wider than the line)
/// degrade to just the truncated left text instead of panicking.
fn pad_with_right(left: &str, right: &str, width: usize) -> String {
    let right_width = display_width(right);
    if right.is_empty() || right_width >= width {
        return truncate_to_width(left, width);
    }
    let room = width.saturating_sub(right_width).saturating_sub(1);
    let left = truncate_to_width(left, room);
    let pad = width
        .saturating_sub(display_width(&left))
        .saturating_sub(right_width);
    format!("{left}{}{right}", " ".repeat(pad.max(1)))
}

fn run(initial_dir: Option<PathBuf>) -> std::io::Result<()> {
    let mut controller =
        Controller::open_default().map_err(|err| std::io::Error::other(err.to_string()))?;
    let (initial_import, initial_info) = initial_dir.map_or((None, None), |dir| {
        match controller.import_folder(&dir, true) {
            Ok(report) => {
                let summary = import_summary("Startup folder import", &report);
                let details = (!report.issues.is_empty())
                    .then(|| import_detail_lines("Startup folder import", &report));
                (Some(summary), details)
            }
            Err(error) => (Some(format!("Startup folder import failed: {error}")), None),
        }
    });

    let guard = TerminalGuard::enter()?;
    // Restore the terminal even if a panic unwinds through the loop.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut tui = Tui {
        controller,
        cursor: 0,
        offset: 0,
        mode: if initial_info.is_some() {
            Mode::InfoView
        } else {
            Mode::Normal
        },
        input: String::new(),
        search: String::new(),
        rows_cache: RefCell::new(None),
        favorites_only: false,
        migrate_source: PathBuf::new(),
        lyrics_lines: Vec::new(),
        lyrics_scroll: 0,
        lyrics_title: String::new(),
        queue_cursor: 0,
        queue_offset: 0,
        info_title: if initial_info.is_some() {
            "Startup folder import results".to_owned()
        } else {
            String::new()
        },
        info_lines: initial_info.unwrap_or_default(),
        info_scroll: 0,
        pending_plan: None,
        migrated_dest: None,
        lifecycle_msg: initial_import,
        import_copy: true,
        last_reconcile: None,
    };
    let mut stdout = std::io::stdout();
    let result = tui.event_loop(&mut stdout);

    drop(guard);
    tui.controller.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        Mode, StorageMode, Track, Tui, clamp_terminal_size, detail_view_fits, display_width,
        fit_input_line, help_lines, normal_help, pad_with_right, resource_label,
        terminal_too_small, truncate_to_width,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-tui-test-{}-{test}-{unique}",
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

    /// A Tui over a hermetic legacy controller — the key-handling
    /// wiring is what is under test; lifecycle semantics themselves
    /// live in `flux-app`.
    fn tui(dir: &TempDir) -> Tui {
        let (engine, audio) = flux_app::default_engine();
        Tui {
            controller: flux_app::Controller::new(
                engine,
                audio,
                Some(dir.path().join("library-index.json")),
            )
            .expect("controller opens the test library"),
            cursor: 0,
            offset: 0,
            mode: Mode::Normal,
            input: String::new(),
            search: String::new(),
            rows_cache: RefCell::new(None),
            favorites_only: false,
            migrate_source: std::path::PathBuf::new(),
            lyrics_lines: Vec::new(),
            lyrics_scroll: 0,
            lyrics_title: String::new(),
            queue_cursor: 0,
            queue_offset: 0,
            info_title: String::new(),
            info_lines: Vec::new(),
            info_scroll: 0,
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
            import_copy: true,
            last_reconcile: None,
        }
    }

    fn key(tui: &mut Tui, code: KeyCode) {
        assert!(
            !tui.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
            "test keys must not quit"
        );
    }

    fn type_path(tui: &mut Tui, path: &std::path::Path) {
        for c in path.display().to_string().chars() {
            key(tui, KeyCode::Char(c));
        }
        key(tui, KeyCode::Enter);
    }

    fn type_text(tui: &mut Tui, text: &str) {
        for c in text.chars() {
            key(tui, KeyCode::Char(c));
        }
        key(tui, KeyCode::Enter);
    }

    #[test]
    fn crossfade_keys_show_toggle_adjust_and_persist_shared_settings() {
        let dir = TempDir::new("crossfade-keys");
        let settings = dir.path().join("config").join("settings.json");
        {
            let mut app = tui(&dir);
            app.controller.load_settings_from_path(&settings);
            assert!(!app.controller.crossfade_enabled());
            assert!(app.now_playing_line(120).contains("Xfade off"));
            let help = help_lines(&app.controller, "test library").join("\n");
            assert!(help.contains("K crossfade"), "{help}");
            assert!(help.contains("[ ] crossfade duration"), "{help}");
            assert!(normal_help().contains("? help"));

            key(&mut app, KeyCode::Char('K'));
            assert!(app.controller.crossfade_enabled());
            assert_eq!(app.controller.crossfade_duration_ms(), 3_000);
            assert_eq!(app.controller.crossfade_ms(), 3_000);
            assert!(
                app.lifecycle_msg
                    .as_deref()
                    .is_some_and(|message| message.contains("Crossfade on"))
            );
            assert!(app.now_playing_line(120).contains("Xfade 3.0s"));

            key(&mut app, KeyCode::Char(']'));
            assert_eq!(app.controller.crossfade_duration_ms(), 3_250);
            assert_eq!(app.controller.crossfade_ms(), 3_250);
            key(&mut app, KeyCode::Char('['));
            assert_eq!(app.controller.crossfade_duration_ms(), 3_000);

            let saved = std::fs::read_to_string(&settings).expect("shared settings saved");
            assert!(saved.contains("\"enabled\": true"));
            assert!(saved.contains("\"duration_ms\": 3000"));
            app.controller.shutdown();
        }

        let restarted_dir = TempDir::new("crossfade-keys-restarted");
        let mut restarted = tui(&restarted_dir);
        restarted.controller.load_settings_from_path(&settings);
        assert!(restarted.controller.crossfade_enabled());
        assert_eq!(restarted.controller.crossfade_duration_ms(), 3_000);
        assert_eq!(restarted.controller.crossfade_ms(), 3_000);
        key(&mut restarted, KeyCode::Char('K'));
        assert!(!restarted.controller.crossfade_enabled());
        assert_eq!(restarted.controller.crossfade_ms(), 0);
        assert!(restarted.now_playing_line(120).contains("Xfade off"));
        restarted.controller.shutdown();
    }

    #[test]
    fn create_key_flow_creates_and_switches_and_esc_cancels() {
        use flux_app::StorageMode;

        let dir = TempDir::new("create-flow");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);

        // Esc during path entry cancels: nothing happens.
        key(&mut tui, KeyCode::Char('C'));
        assert_eq!(tui.mode, Mode::CreateInput);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert!(!root.join(".flux").exists());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );

        // A rejected destination preserves the active library.
        let occupied = dir.path().join("occupied");
        std::fs::write(&occupied, b"not a directory").expect("occupied file");
        tui.search = "preserve me".to_owned();
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &occupied);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(tui.search, "preserve me");

        // Full flow: C, type folder, Enter → created + switched. State tied
        // to the old library is cleared, while the global filter survives.
        let old_session = tui.controller.library_session();
        tui.search = "old library".to_owned();
        tui.input = "stale input".to_owned();
        tui.lyrics_lines = vec!["old lyric".to_owned()];
        tui.lyrics_title = "old title".to_owned();
        tui.favorites_only = true;
        key(&mut tui, KeyCode::Char('C'));
        tui.queue_cursor = 5;
        tui.queue_offset = 3;
        tui.info_lines = vec!["stale info".to_owned()];
        type_path(&mut tui, &root);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Canonical,
            "{:?}",
            tui.lifecycle_msg
        );
        assert_ne!(tui.controller.library_session(), old_session);
        assert!(tui.search.is_empty());
        assert!(tui.input.is_empty());
        assert!(tui.lyrics_lines.is_empty());
        assert!(tui.lyrics_title.is_empty());
        assert_eq!(tui.queue_cursor, 0);
        assert_eq!(tui.queue_offset, 0);
        assert!(tui.info_lines.is_empty());
        assert!(tui.info_title.is_empty());
        assert!(tui.favorites_only);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("Active library")),
            "{:?}",
            tui.lifecycle_msg
        );
        assert!(
            root.join("catalog.flux").exists(),
            "canonical catalogue created"
        );
        assert!(!root.join(".flux").exists(), "no portable runtime store");
        tui.controller.shutdown();
    }

    #[test]
    fn import_edit_and_artist_keys_use_the_shared_controller() {
        let dir = TempDir::new("tui-manage");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Tui Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);

        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Canonical,
            "{:?}",
            tui.lifecycle_msg
        );

        key(&mut tui, KeyCode::Char('i'));
        assert_eq!(tui.mode, Mode::ImportInput);
        assert!(tui.import_copy);
        type_path(&mut tui, &source);
        assert_eq!(tui.controller.tracks().len(), 1);
        let id = tui.controller.tracks()[0].0;
        let artist = tui.controller.track_artist_id(id).expect("inferred artist");
        let source_message = tui.lifecycle_msg.clone().expect("import summary");
        assert!(source_message.contains("1 added"), "{source_message}");

        key(&mut tui, KeyCode::Char('f'));
        assert!(
            tui.controller
                .rating(id)
                .is_some_and(|rating| rating.favorite)
        );
        key(&mut tui, KeyCode::Char('4'));
        assert_eq!(
            tui.controller.rating(id).and_then(|rating| rating.rating),
            Some(4)
        );
        key(&mut tui, KeyCode::Char('0'));
        assert_eq!(
            tui.controller.rating(id).and_then(|rating| rating.rating),
            None
        );

        key(&mut tui, KeyCode::Char('D'));
        assert_eq!(tui.mode, Mode::InfoView);
        let album_details = tui.info_lines.join("\n");
        assert!(
            album_details.contains("AlbumId") && album_details.contains("owner"),
            "{album_details}"
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('P'));
        type_text(&mut tui, "Extra Artist");
        assert_eq!(tui.controller.artists().len(), 2);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|m| m.contains("Extra Artist"))
        );

        key(&mut tui, KeyCode::Char('e'));
        assert_eq!(tui.mode, Mode::TrackEditInput);
        tui.input = format!("Tui Title | Tui Album | 3 | 1 | {artist} | {artist}");
        key(&mut tui, KeyCode::Enter);
        let track = tui.controller.track(id).expect("edited");
        assert_eq!(track.title(), "Tui Title");
        assert_eq!(track.album(), Some("Tui Album"));
        assert_eq!(track.artist(), Some("Tui Artist"));
        assert_eq!(tui.controller.track_artist_id(id), Some(artist));
        assert_eq!(tui.controller.track_credits(id), vec![artist]);

        key(&mut tui, KeyCode::Char('e'));
        let invalid = format!("Bad Title | Album | not-a-number | 1 | {artist} | {artist}");
        tui.input.clone_from(&invalid);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(
            tui.mode,
            Mode::TrackEditInput,
            "invalid input keeps the edit form open"
        );
        assert_eq!(
            tui.input, invalid,
            "typed fields survive a validation error"
        );
        assert_eq!(
            tui.controller.track(id).map(Track::title),
            Some("Tui Title")
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('V'));
        assert_eq!(tui.mode, Mode::InfoView);
        let artist_details = tui.info_lines.join("\n");
        assert!(
            artist_details.contains(&format!("ArtistId {artist} · Tui Artist")),
            "{artist_details}"
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('Y'));
        assert_eq!(tui.mode, Mode::ArtistRenameInput);
        tui.input = format!("{artist}|Renamed Tui Artist");
        key(&mut tui, KeyCode::Enter);
        assert_eq!(
            tui.controller.track(id).and_then(Track::artist),
            Some("Renamed Tui Artist")
        );

        tui.controller
            .set_lyrics(id, Some("[00:01.000]Tui lyric\nplain tail"))
            .expect("lyrics");
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::LyricsView);
        assert!(
            tui.lyrics_lines
                .iter()
                .any(|line| line.contains("Tui lyric"))
        );
        key(&mut tui, KeyCode::Down);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.set_favorites(&[id], true).expect("favorite");
        key(&mut tui, KeyCode::Char('F'));
        assert_eq!(tui.rows().len(), 1, "favorites-only filter");
        key(&mut tui, KeyCode::Char('F'));
        key(&mut tui, KeyCode::Char('H'));
        assert!(tui.lifecycle_msg.as_deref().is_some_and(
            |message| message.contains("Most played") || message.contains("No meaningful")
        ));

        let outside = dir.path().join("Other Artist/Album/outside.wav");
        std::fs::create_dir_all(outside.parent().expect("parent")).expect("outside dir");
        let mut different = minimal_wav();
        different.push(0);
        std::fs::write(&outside, different).expect("outside");
        key(&mut tui, KeyCode::Char('I'));
        type_path(&mut tui, &outside);
        assert_eq!(tui.controller.tracks().len(), 2);
        assert!(outside.is_file(), "source is never moved or deleted");
        tui.controller.shutdown();
    }

    #[test]
    fn canonical_refresh_keeps_missing_track_identity() {
        use flux_app::StorageMode;

        let dir = TempDir::new("tui-missing");
        let root = dir.path().join("library");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("missing.wav");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        key(&mut tui, KeyCode::Char('I'));
        type_path(&mut tui, &source);
        assert_eq!(tui.controller.tracks().len(), 1);
        let id = tui.controller.tracks()[0].0;
        let audio = tui
            .controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();
        std::fs::remove_file(audio).expect("remove managed audio");
        key(&mut tui, KeyCode::Char('X'));
        assert!(tui.controller.track(id).is_some(), "identity is retained");
        assert_eq!(
            tui.controller.track_availability(id),
            Some(flux_app::Availability::Missing)
        );
        assert!(source.is_file(), "the original source remains untouched");
        tui.controller.shutdown();
    }

    #[test]
    fn migration_key_flow_plans_executes_and_activates() {
        use flux_app::StorageMode;

        let dir = TempDir::new("migrate-flow");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("root");
        let song = root.join("song.wav");
        std::fs::write(&song, minimal_wav()).expect("song");
        let index = dir.path().join("legacy.json");
        std::fs::write(
            &index,
            format!(
                r#"{{"version":1,"next_id":2,"tracks":[{{"id":1,"title":"Song","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}]}}"#,
                song.display()
            ),
        )
        .expect("index");

        let destination = dir.path().join("CanonicalResult");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('M'));
        assert_eq!(tui.mode, Mode::MigrateSourceInput);
        type_path(&mut tui, &index);
        assert_eq!(tui.mode, Mode::MigrateDestInput);
        type_path(&mut tui, &destination);
        assert_eq!(tui.mode, Mode::MigrateConfirm);
        assert!(tui.pending_plan.is_some());
        assert!(!destination.exists(), "planning wrote nothing");
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);

        // Cancel at the confirmation step.
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.pending_plan.is_none());
        assert!(!destination.exists(), "cancel wrote nothing");

        // Again, this time confirm and activate.
        key(&mut tui, KeyCode::Char('M'));
        type_path(&mut tui, &index);
        type_path(&mut tui, &destination);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::MigratedDone);
        assert!(destination.join("catalog.flux").exists());
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Legacy,
            "execution alone never switches"
        );

        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        let ids: Vec<u64> = tui
            .controller
            .tracks()
            .iter()
            .map(|(id, _)| id.get())
            .collect();
        assert_eq!(ids, vec![1], "migrated ID preserved through activation");
        tui.controller.shutdown();
    }

    #[test]
    fn reconcile_key_reports_summary_and_never_switches() {
        use flux_app::StorageMode;

        let dir = TempDir::new("reconcile-key");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);

        // Legacy mode: X is refused visibly and changes nothing.
        key(&mut tui, KeyCode::Char('X'));
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert!(tui.controller.last_error().is_some());
        assert!(tui.lifecycle_msg.is_none(), "no false success summary");

        // Create + switch, then X reconciles and reports.
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        key(&mut tui, KeyCode::Char('X'));
        assert_eq!(tui.mode, Mode::Normal);
        let message = tui.lifecycle_msg.clone().expect("summary shown");
        assert!(
            message.contains("Refreshed the canonical library"),
            "{message}"
        );
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        tui.controller.shutdown();
    }

    /// The smallest valid PCM WAV header (no samples); nothing here
    /// plays it.
    fn minimal_wav() -> Vec<u8> {
        let mut wav = Vec::with_capacity(44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&88_200u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav
    }

    #[test]
    fn protected_index_key_preserves_and_reenables_writes() {
        let dir = TempDir::new("protected-index");
        let index = dir.path().join("library-index.json");
        std::fs::write(&index, b"not a valid index").expect("corrupt index");
        let mut tui = tui(&dir);
        assert!(tui.controller.legacy_index_write_protected());

        key(&mut tui, KeyCode::Char('U'));
        assert_eq!(tui.mode, Mode::ConfirmPreserveIndex);
        assert!(tui.controller.legacy_index_write_protected());
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.controller.legacy_index_write_protected());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );

        key(&mut tui, KeyCode::Char('U'));
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(!tui.controller.legacy_index_write_protected());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("preserved")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.save_index().expect("new index writes");
        key(&mut tui, KeyCode::Char('A'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("no portable recovery")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.mode = Mode::ConfirmAcknowledgeRecovery;
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("remains read-only")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.shutdown();
    }

    #[test]
    fn queue_keys_route_add_play_next_remove_and_reorder_through_controller() {
        let dir = TempDir::new("queue-keys");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("music dir");
        let one = root.join("one.wav");
        let two = root.join("two.wav");
        std::fs::write(&one, minimal_wav()).expect("one");
        std::fs::write(&two, minimal_wav()).expect("two");
        std::fs::write(
            dir.path().join("library-index.json"),
            format!(
                r#"{{"version":1,"next_id":3,"tracks":[{{"id":1,"title":"One","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}},{{"id":2,"title":"Two","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}]}}"#,
                one.display(),
                two.display()
            ),
        )
        .expect("index");
        let mut tui = tui(&dir);
        assert_eq!(tui.controller.queue_len(), 2);

        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(
            tui.controller.queue_len(),
            3,
            "a appends the selected track"
        );

        key(&mut tui, KeyCode::Char('Q'));
        key(&mut tui, KeyCode::Delete);
        assert_eq!(
            tui.controller.queue_len(),
            2,
            "Delete removes the selected queue entry from the queue view"
        );
        assert_eq!(
            tui.controller.player_state(),
            flux_core::PlaybackState::Stopped
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('x'));
        assert_eq!(
            tui.controller.queue_len(),
            3,
            "x inserts the selected track after the current entry"
        );
        let current = tui
            .controller
            .current_queue_entry_id()
            .expect("current after removal/reinsertion");
        let before = tui
            .controller
            .queue_entry_index(current)
            .expect("current index");
        assert!(
            !tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
            "Ctrl+Down is not quit"
        );
        assert_eq!(
            tui.controller.current_queue_entry_id(),
            Some(current),
            "reordering keeps the current entry identity"
        );
        assert_eq!(tui.controller.queue_entry_index(current), Some(before + 1));
        tui.controller.shutdown();
    }

    #[test]
    fn tiny_terminals_are_detected_before_clamping() {
        assert!(terminal_too_small(19, 24));
        assert!(terminal_too_small(80, 5));
        assert!(!terminal_too_small(20, 6));
        assert!(!terminal_too_small(120, 40));
    }

    #[test]
    fn escape_without_a_search_does_not_reset_the_cursor() {
        let dir = TempDir::new("escape-no-search");
        let mut tui = tui(&dir);
        tui.cursor = 3;
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.cursor, 3);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("No active search"))
        );
        tui.controller.shutdown();
    }

    #[test]
    fn pad_with_right_handles_degenerate_widths() {
        assert_eq!(pad_with_right("title", "3:45", 10), "title 3:45");
        assert_eq!(pad_with_right("title", "", 10), "title");
        assert_eq!(pad_with_right("title", "3:45", 4), "titl");
        assert_eq!(pad_with_right("title", "3:45", 0), "");
        assert_eq!(
            pad_with_right("a longer title here", "3:45", 10),
            "a lon 3:45"
        );
        assert_eq!(display_width("界"), 2);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(truncate_to_width("界界界", 5), "界界");
        assert_eq!(display_width(&pad_with_right("界界", "x", 5)), 5);
        let tail = fit_input_line("", "界界界", 4);
        assert!(display_width(&tail) <= 4, "{tail}");
        assert!(tail.starts_with('…'), "{tail}");
    }

    #[test]
    fn deletion_and_transfer_keys_refuse_to_bypass_gui_workflow() {
        let dir = TempDir::new("tui-delete-refusal");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('T'));
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("GUI"))
        );
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn help_info_and_input_lines_support_discovery_and_long_unicode_paths() {
        let dir = TempDir::new("tui-help-info");
        let mut tui = tui(&dir);
        assert!(normal_help().contains("? help"));
        assert!(
            tui.library_header_identity(40)
                .contains("library-index.json")
        );
        key(&mut tui, KeyCode::Char('?'));
        assert_eq!(tui.mode, Mode::InfoView);
        assert!(tui.info_title.contains("help"));
        let help = tui.info_lines.join("\n");
        assert!(help.contains("Q queue view"), "{help}");
        assert!(help.contains("Active library:"), "{help}");
        assert!(help.contains("policy system default"), "{help}");
        assert!(help.contains("bounded automatic recovery"), "{help}");
        assert!(help.contains("SAFETY AND LIMITS"), "{help}");
        assert!(help.contains("device selection are GUI-only"), "{help}");
        assert!(help.contains("GUI review workflows"), "{help}");
        assert!(help.contains("synchronous quick copy"), "{help}");
        key(&mut tui, KeyCode::Down);
        assert_eq!(tui.info_scroll, 1);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.lifecycle_msg = Some("transient notice".to_owned());
        key(&mut tui, KeyCode::Char('c'));
        assert!(tui.lifecycle_msg.is_none());

        assert_eq!(clamp_terminal_size(0, 0), (20, 6));
        assert_eq!(clamp_terminal_size(120, 40), (120, 40));
        assert!(!detail_view_fits(6));
        assert!(detail_view_fits(8));
        assert_eq!(
            resource_label(Some(flux_app::ResourceState::Missing)),
            Some("[missing]")
        );
        assert_eq!(
            resource_label(Some(flux_app::ResourceState::Unavailable(
                "permission denied".to_owned()
            ))),
            Some("[inaccessible]")
        );
        assert_eq!(resource_label(Some(flux_app::ResourceState::Present)), None);
        assert_eq!(resource_label(None), None);
        let long = format!("{}{}", "a".repeat(80), "é".repeat(20));
        let line = fit_input_line("Path: ", &long, 24);
        assert_eq!(line.chars().count(), 24);
        assert!(line.starts_with('…'), "{line}");
        assert!(line.ends_with('_'), "{line}");
        assert_eq!(fit_input_line("Path: ", "short", 40), "Path: short_");
        key(&mut tui, KeyCode::Char('?'));
        assert!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "Ctrl-C must quit from an info view"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn queue_view_preserves_entry_identity_and_supports_play_move_and_remove() {
        let dir = TempDir::new("tui-queue-view");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("music dir");
        let one = root.join("one.wav");
        let two = root.join("two.wav");
        std::fs::write(&one, minimal_wav()).expect("one");
        std::fs::write(&two, minimal_wav()).expect("two");
        std::fs::write(
            dir.path().join("library-index.json"),
            format!(
                r#"{{"version":1,"next_id":3,"tracks":[{{"id":1,"title":"One","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}},{{"id":2,"title":"Two","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}]}}"#,
                one.display(),
                two.display()
            ),
        )
        .expect("index");
        let mut tui = tui(&dir);
        let first = tui.controller.tracks()[0].0;
        tui.controller
            .enqueue_track(first)
            .expect("duplicate entry");
        let entries = tui.controller.queue_entries().to_vec();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.track() == first)
                .count(),
            2
        );
        assert_ne!(entries[0].id(), entries[2].id());

        key(&mut tui, KeyCode::Char('Q'));
        assert_eq!(tui.mode, Mode::QueueView);
        let selected = tui.controller.queue_entries()[tui.queue_cursor].id();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.controller.current_queue_entry_id(), Some(selected));
        let before_index = tui.queue_cursor;
        assert!(
            !tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
            "Ctrl+Down must not quit"
        );
        assert_eq!(tui.controller.current_queue_entry_id(), Some(selected));
        assert_eq!(tui.queue_cursor, before_index + 1);

        let mut rendered = Vec::new();
        tui.draw(&mut rendered).expect("queue view render");
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(rendered.contains("QUEUE"), "{rendered}");
        assert!(rendered.contains(&format!("#{selected}")), "{rendered}");

        let len_before = tui.controller.queue_len();
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.controller.queue_len(), len_before - 1);
        assert!(
            tui.controller
                .queue_entries()
                .iter()
                .all(|entry| entry.id() != selected)
        );
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_partial_success_preserves_every_issue_detail() {
        let dir = TempDir::new("tui-import-issues");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source = dir.path().join("Source/Album/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source dir");
        std::fs::write(&source, minimal_wav()).expect("source");
        let missing = dir.path().join("Source/Album/missing.wav");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_text(
            &mut tui,
            &format!("{};{}", source.display(), missing.display()),
        );

        assert_eq!(tui.controller.tracks().len(), 1);
        assert_eq!(tui.mode, Mode::InfoView);
        let summary = tui.lifecycle_msg.as_deref().expect("summary");
        assert!(summary.contains("1 added"), "{summary}");
        assert!(summary.contains("1 issue"), "{summary}");
        let details = tui.info_lines.join("\n");
        assert!(details.contains("Per-path issues (1)"), "{details}");
        assert!(
            details.contains(&missing.display().to_string()),
            "{details}"
        );
        assert_eq!(tui.info_title, "File import results");
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn tui_folder_import_reports_unsupported_files_and_missing_directories() {
        let dir = TempDir::new("tui-folder-issues");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let folder = dir.path().join("Incoming");
        std::fs::create_dir_all(&folder).expect("folder");
        std::fs::write(folder.join("good.wav"), minimal_wav()).expect("good wav");
        std::fs::write(folder.join("notes.txt"), b"not audio").expect("text file");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('o'));
        type_path(&mut tui, &folder);
        assert_eq!(tui.controller.tracks().len(), 1);
        assert_eq!(tui.mode, Mode::InfoView);
        let details = tui.info_lines.join("\n");
        assert!(details.contains("notes.txt"), "{details}");
        assert!(details.contains("Per-path issues"), "{details}");
        key(&mut tui, KeyCode::Esc);

        let missing = dir.path().join("does-not-exist");
        key(&mut tui, KeyCode::Char('o'));
        type_path(&mut tui, &missing);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("Folder import failed")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.shutdown();
    }

    #[test]
    fn empty_messages_distinguish_search_and_favorites() {
        let dir = TempDir::new("tui-empty-messages");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source = dir.path().join("Artist/Album/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, minimal_wav()).expect("song");
        let mut tui = tui(&dir);
        assert!(tui.empty_rows_message().contains("Library is empty"));
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        tui.search = "not present".to_owned();
        assert!(tui.empty_rows_message().contains("current search"));
        tui.favorites_only = true;
        assert!(
            tui.empty_rows_message()
                .contains("No favorite tracks match")
        );
        tui.search.clear();
        assert!(tui.empty_rows_message().contains("No favorite tracks"));
        tui.controller.shutdown();
    }

    #[test]
    fn tui_marks_and_inspects_standalone_tracks_without_album_membership() {
        let dir = TempDir::new("tui-standalone");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        let source = dir.path().join("source/standalone.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source");
        std::fs::write(&source, minimal_wav()).expect("wav");
        let artist = tui.controller.create_artist("Solo Artist").expect("artist");
        let id = tui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source,
                title: "Standalone Song".to_owned(),
                artist_id: Some(artist),
                artist_name: "Solo Artist".to_owned(),
                credited_artist_ids: vec![artist],
                album_title: None,
                release_type: flux_app::ReleaseType::Single,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("standalone import");
        assert_eq!(tui.controller.track_album_id(id), None);

        let mut rendered = Vec::new();
        tui.draw(&mut rendered).expect("draw");
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(rendered.contains("[standalone]"), "{rendered}");

        key(&mut tui, KeyCode::Char('E'));
        assert_eq!(tui.mode, Mode::InfoView);
        let albums = tui.info_lines.join("\n");
        assert!(albums.contains("Standalone Tracks"), "{albums}");
        assert!(albums.contains("Standalone Song"), "{albums}");
        key(&mut tui, KeyCode::Esc);
        key(&mut tui, KeyCode::Char('D'));
        assert_eq!(tui.mode, Mode::InfoView);
        assert_eq!(tui.info_title, "Selected standalone Track");
        assert!(
            tui.info_lines
                .iter()
                .any(|line| line.contains("no Album membership"))
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_lists_empty_albums_with_owner_and_count() {
        let dir = TempDir::new("tui-empty-albums");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        let artist = tui.controller.create_artist("Artist").expect("artist");
        tui.controller
            .create_album(artist, "Empty", flux_app::ReleaseType::Album)
            .expect("album");
        key(&mut tui, KeyCode::Char('E'));
        assert_eq!(tui.mode, Mode::InfoView);
        let message = tui.info_lines.join("\n");
        assert!(message.contains("Empty"), "{message}");
        assert!(message.contains("0 track(s)"), "{message}");
        assert!(message.contains("artwork no"), "{message}");
        assert!(message.contains("Empty albums are retained"), "{message}");
        tui.controller.shutdown();
    }
}
