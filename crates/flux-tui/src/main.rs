//! Flux terminal interface (`flux-tui`).
//!
//! A minimal interactive player over the shared [`flux_app::Controller`]
//! — the same controller the GUI uses, so behavior matches exactly.
//! Rendering is hand-rolled on top of `crossterm` (no widget
//! framework): a header, a now-playing line, a scrollable track list,
//! a status/error line, and a help bar. Playback, scanning, and
//! persistence all happen on the controller/engine workers; this
//! thread only draws and reads keys.
//!
//! Usage: `flux-tui [MUSIC_DIR]` — with a directory argument, a scan
//! of it starts immediately. Real audio output requires a build with
//! `--features cpal`; otherwise the app says so and playback commands
//! report why (see the README).
//!
//! Keys:
//! - `↑`/`k`, `↓`/`j` move; `PgUp`/`PgDn`, `g`/`G` jump
//! - `Enter` play selected, `Space` play/pause, `s` stop
//! - `n` next, `p` previous, `+`/`-` volume, `R` cycle repeat
//! - `a` add selected track to the queue, `x` play it next,
//!   `Delete` remove the current queue entry, `Ctrl+↑`/`Ctrl+↓`
//!   reorder the current queue entry
//! - `o` scan a folder (type a path), `r` refresh scanned folders
//! - `/` search/filter, `Esc` clear search or cancel input
//! - `q` or `Ctrl-C` quit (terminal state is always restored)
//! - `U` preserve and replace an unreadable legacy app index (writes
//!   are otherwise disabled so it can never be overwritten)
//! - `O` open a portable library (type its folder), `C` create one,
//!   `M` migrate a legacy index (type index file, then destination
//!   folder; review the plan, `y` executes, `Esc` cancels; in the
//!   migration-complete screen, `a` opens the migrated library), `B`
//!   switch back to the app library index, `A` acknowledge a recovered
//!   library, `X` reconcile the portable catalogue with the filesystem
//!   (refresh
//!   availability/fingerprints; missing records are kept and
//!   possible new locations are only reported, never auto-applied)
//!
//! Startup always uses the legacy app index: opening, creating,
//! migrating, or switching libraries only ever happens through the
//! explicit keys above, and a failed/cancelled switch leaves the
//! active library untouched.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use flux_app::{
    Controller, MigrationPlan, StorageMode, default_index_path, format_duration, is_track_playable,
};
use flux_core::{MILESTONE, PlaybackState, Track, TrackId};

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

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    ScanInput,
    SearchInput,
    /// Typing a folder whose portable library to open (`O`).
    OpenInput,
    /// Typing a folder for a new portable library (`C`).
    CreateInput,
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
    /// Legacy index file chosen at migration step 1.
    migrate_source: PathBuf,
    /// Planned-but-unconfirmed migration (nothing written yet).
    pending_plan: Option<MigrationPlan>,
    /// Destination of a completed migration, offered for activation.
    migrated_dest: Option<PathBuf>,
    /// Latest library-lifecycle outcome (plan summary, errors, …).
    lifecycle_msg: Option<String>,
}

impl Tui {
    /// IDs of the visible rows. Only IDs are collected; track metadata
    /// is looked up for the handful of rows actually drawn. Cloning every
    /// `Track` on each redraw was unnecessary work for large libraries.
    fn rows(&self) -> Vec<TrackId> {
        self.controller
            .search(&self.search)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
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

    /// Returns true when the user asked to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if self.mode != Mode::Normal {
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
                    self.controller.play_track(id);
                }
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
            KeyCode::Char('a') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    let _ = self.controller.enqueue_track(id);
                }
            }
            KeyCode::Char('x') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    let _ = self.controller.play_next_track(id);
                }
            }
            KeyCode::Delete => {
                if let Some(entry) = self.controller.current_queue_entry_id() {
                    let _ = self.controller.remove_queue_entry(entry);
                } else {
                    self.lifecycle_msg = Some("The queue is empty.".to_owned());
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
            KeyCode::Char('o') => {
                self.mode = Mode::ScanInput;
                self.input.clear();
            }
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
            KeyCode::Char('B') => {
                if self
                    .controller
                    .switch_to_legacy_index(default_index_path())
                    .is_ok()
                {
                    self.after_switch();
                }
            }
            KeyCode::Char('U') => {
                if !self.controller.legacy_index_write_protected() {
                    self.lifecycle_msg =
                        Some("The active library index is not write-protected.".to_owned());
                } else {
                    match self.controller.replace_unreadable_legacy_index() {
                        Ok(backup) => {
                            self.lifecycle_msg = Some(format!(
                                "Unreadable index preserved as {}; scanning and saving are enabled.",
                                backup.display()
                            ));
                        }
                        Err(err) => self.lifecycle_msg = Some(err),
                    }
                }
            }
            KeyCode::Char('A') => {
                if self.controller.portable_recovery().is_some()
                    && let Err(err) = self.controller.acknowledge_portable_recovery()
                {
                    self.lifecycle_msg = Some(err);
                }
            }
            KeyCode::Char('X') => {
                // Failures land in the controller's error line; the
                // detailed summary replaces the lifecycle message.
                if let Ok(report) = self.controller.reconcile_portable_library() {
                    self.lifecycle_msg = Some(reconcile_summary(&report));
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::SearchInput;
                self.input.clone_from(&self.search);
            }
            KeyCode::Esc => {
                self.search.clear();
                self.cursor = 0;
            }
            _ => {}
        }
        false
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
                if key.code == KeyCode::Char('a')
                    && let Some(destination) = self.migrated_dest.clone()
                    && self.controller.switch_to_portable(&destination).is_ok()
                {
                    self.after_switch();
                }
                self.migrated_dest = None;
                self.mode = Mode::Normal;
                return false;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Esc => {
                // Cancelling an input never changes the active
                // library and abandons any in-flight plan.
                self.pending_plan = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                let mut next = Mode::Normal;
                match self.mode {
                    Mode::ScanInput => {
                        let path = text.trim();
                        if !path.is_empty() {
                            self.controller.begin_scan(PathBuf::from(path));
                        }
                    }
                    Mode::SearchInput => {
                        self.search = text;
                        self.cursor = 0;
                    }
                    Mode::OpenInput => {
                        let path = PathBuf::from(text.trim());
                        if self.controller.switch_to_portable(&path).is_ok() {
                            self.after_switch();
                        }
                    }
                    Mode::CreateInput => {
                        let path = PathBuf::from(text.trim());
                        if self
                            .controller
                            .switch_to_new_portable_library(&path)
                            .is_ok()
                        {
                            self.after_switch();
                        }
                    }
                    Mode::MigrateSourceInput => {
                        let source = text.trim();
                        if !source.is_empty() {
                            self.migrate_source = PathBuf::from(source);
                            next = Mode::MigrateDestInput;
                        }
                    }
                    Mode::MigrateDestInput => {
                        let destination = PathBuf::from(text.trim());
                        match flux_app::plan_legacy_migration(&self.migrate_source, &destination) {
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
                    Mode::Normal | Mode::MigrateConfirm | Mode::MigratedDone => {}
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

    /// Runs the migration the user confirmed with `y`. Execution
    /// never switches the active library by itself; success moves to
    /// [`Mode::MigratedDone`], where `a` activates the result.
    fn execute_pending_migration(&mut self) {
        let Some(plan) = self.pending_plan.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let destination = plan.destination_root().to_path_buf();
        match flux_app::execute_legacy_migration(&plan) {
            Ok(report) => {
                let done = report.plan();
                self.lifecycle_msg = Some(format!(
                    "Migration complete: {} track(s) migrated ({} present, {} missing), \
                     {} excluded. Active library unchanged — press a to open the \
                     migrated library, Esc to keep the current one.",
                    done.migrated_count(),
                    done.present_count(),
                    done.missing_count(),
                    done.records().len() - done.migrated_count(),
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

    /// Housekeeping after a successful library switch: cursor,
    /// search, and pending migration state referred to the old
    /// library. (The controller already stopped playback and rebuilt
    /// its queue from the new library.)
    fn after_switch(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.lifecycle_msg = None;
    }

    /// After next/previous, put the cursor on the player's selection.
    fn follow_current(&mut self) {
        if let Some(current) = self.controller.current_id()
            && let Some(pos) = self.rows().iter().position(|id| *id == current)
        {
            self.cursor = pos;
        }
    }

    fn draw(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let (width, height) = terminal::size().unwrap_or((80, 24));
        // A fresh pty can report 0 columns; lay out for at least a
        // minimal sane width rather than dividing the void.
        let width = usize::from(width.max(20));
        let height = usize::from(height.max(6));
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

        // Header.
        let library_label = match self.controller.storage_mode() {
            StorageMode::Legacy => "library: legacy index".to_owned(),
            StorageMode::Portable => match self.controller.portable_root() {
                Some(root) => format!("library: portable {}", root.display()),
                None => "library: portable".to_owned(),
            },
        };
        let mut header = format!(
            "Flux {MILESTONE} — terminal player   [{library_label}]   [queue {}]",
            self.controller.queue_len()
        );
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
            queue!(out, SetForegroundColor(Color::DarkYellow))?;
            self.line(
                out,
                2,
                &format!("Audio unavailable — {reason}"),
                width,
                false,
            )?;
            queue!(out, SetForegroundColor(Color::Reset))?;
        } else {
            self.line(out, 2, "", width, false)?;
        }

        // Track list.
        if rows.is_empty() {
            let msg = if self.controller.tracks().is_empty() {
                "Library is empty — press o, type a music folder path, and press Enter to scan."
            } else {
                "No tracks match the current search — Esc clears it."
            };
            self.line(out, list_top as u16, msg, width, false)?;
        }
        for (row, id) in rows.iter().skip(self.offset).take(list_height).enumerate() {
            let index = self.offset + row;
            let Some(track) = self.controller.track(*id) else {
                continue;
            };
            let mut text = format!(
                "{} {} {}",
                if index == self.cursor { ">" } else { " " },
                if Some(*id) == current { "▶" } else { " " },
                display_title(track),
            );
            if !is_track_playable(track) {
                text.push_str("  [indexed — not playable yet]");
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
        let help = match self.mode {
            Mode::Normal => "Enter play · Space pause · s stop · n/p skip · a add · x next \
                             · Del rm · Ctrl+↑/↓ reorder · +/- vol · o scan · r refresh \
                             · / search · R repeat · O open · C create · M migrate \
                             · B index · U fix · X reconcile · q quit"
                .to_owned(),
            Mode::ScanInput => format!(
                "Scan folder (Enter to scan, Esc to cancel): {}_",
                self.input
            ),
            Mode::SearchInput => format!("Search (Enter to apply, Esc to cancel): {}_", self.input),
            Mode::OpenInput => format!(
                "Open portable library folder (Enter to switch, Esc to cancel): {}_",
                self.input
            ),
            Mode::CreateInput => format!(
                "Create portable library in folder (Enter to create + switch, Esc to cancel): {}_",
                self.input
            ),
            Mode::MigrateSourceInput => format!(
                "Legacy index file to migrate (Enter for destination, Esc to cancel): {}_",
                self.input
            ),
            Mode::MigrateDestInput => format!(
                "Migration destination folder (Enter to plan, Esc to cancel): {}_",
                self.input
            ),
            Mode::MigrateConfirm => {
                "Migration planned — review the status line: y executes, Esc cancels.".to_owned()
            }
            Mode::MigratedDone => {
                "Migration done — a opens the migrated library, Esc keeps the current one."
                    .to_owned()
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
        let text: String = text.chars().take(width).collect();
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
        match self.controller.current_track() {
            Some(track) => {
                let total = track
                    .duration()
                    .or_else(|| self.controller.stream_info().and_then(|i| i.duration))
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".to_owned());
                format!(
                    "{state_icon} {}    {} / {total}    Vol {volume}%",
                    display_title(track),
                    format_duration(self.controller.position()),
                )
            }
            None if self.controller.current_queue_entry_id().is_some() => {
                format!("{state_icon} Queue entry missing from the library    Vol {volume}%")
            }
            None => format!("{state_icon} Nothing selected    Vol {volume}%"),
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

fn migration_plan_summary(plan: &MigrationPlan) -> String {
    format!(
        "Migrate {} track(s) ({} present, {} missing→unavailable); excluded: \
         {} external, {} symlinked, {} unrepresentable, {} ambiguous. \
         y = execute, Esc = cancel (writes nothing).",
        plan.migrated_count(),
        plan.present_count(),
        plan.missing_count(),
        plan.external_count(),
        plan.symlinked_count(),
        plan.unrepresentable_count(),
        plan.ambiguous_count(),
    )
}

fn display_title(track: &Track) -> String {
    match track.artist() {
        Some(artist) => format!("{} — {artist}", track.title()),
        None => track.title().to_owned(),
    }
}

/// Left text plus right-aligned `right`, space-padded to `width`.
/// Degenerate widths (a 0-column pty, `right` wider than the line)
/// degrade to just the truncated left text instead of panicking.
fn pad_with_right(left: &str, right: &str, width: usize) -> String {
    if right.is_empty() || right.chars().count() >= width {
        return left.chars().take(width).collect();
    }
    let room = width - right.chars().count() - 1;
    let left: String = left.chars().take(room).collect();
    let pad = width - left.chars().count() - right.chars().count();
    format!("{left}{}{right}", " ".repeat(pad.max(1)))
}

fn run(initial_dir: Option<PathBuf>) -> std::io::Result<()> {
    let mut controller =
        Controller::open_default().map_err(|err| std::io::Error::other(err.to_string()))?;
    if let Some(dir) = initial_dir {
        controller.begin_scan(dir);
    }

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
        mode: Mode::Normal,
        input: String::new(),
        search: String::new(),
        migrate_source: PathBuf::new(),
        pending_plan: None,
        migrated_dest: None,
        lifecycle_msg: None,
    };
    let mut stdout = std::io::stdout();
    let result = tui.event_loop(&mut stdout);

    drop(guard);
    tui.controller.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::{Mode, Tui, pad_with_right};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            migrate_source: std::path::PathBuf::new(),
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
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

        // Full flow: C, type folder, Enter → created + switched.
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Portable);
        assert!(root.join(".flux").exists(), "store created via flux-app");
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

        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('M'));
        assert_eq!(tui.mode, Mode::MigrateSourceInput);
        type_path(&mut tui, &index);
        assert_eq!(tui.mode, Mode::MigrateDestInput);
        type_path(&mut tui, &root);
        assert_eq!(tui.mode, Mode::MigrateConfirm);
        assert!(tui.pending_plan.is_some());
        assert!(!root.join(".flux").exists(), "planning wrote nothing");
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);

        // Cancel at the confirmation step.
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.pending_plan.is_none());
        assert!(!root.join(".flux").exists(), "cancel wrote nothing");

        // Again, this time confirm and activate.
        key(&mut tui, KeyCode::Char('M'));
        type_path(&mut tui, &index);
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::MigratedDone);
        assert!(root.join(".flux").exists());
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Legacy,
            "execution alone never switches"
        );

        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Portable);
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
        assert_eq!(tui.controller.storage_mode(), StorageMode::Portable);
        key(&mut tui, KeyCode::Char('X'));
        assert_eq!(tui.mode, Mode::Normal);
        let message = tui.lifecycle_msg.clone().expect("summary shown");
        assert!(message.starts_with("Reconciled 0:"), "{message}");
        assert_eq!(tui.controller.storage_mode(), StorageMode::Portable);
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
        assert!(!tui.controller.legacy_index_write_protected());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("preserved")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.save_index().expect("new index writes");
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

        key(&mut tui, KeyCode::Delete);
        assert_eq!(
            tui.controller.queue_len(),
            2,
            "Delete removes the current queue entry"
        );
        assert_eq!(
            tui.controller.player_state(),
            flux_core::PlaybackState::Stopped
        );

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
    fn pad_with_right_handles_degenerate_widths() {
        assert_eq!(pad_with_right("title", "3:45", 10), "title 3:45");
        assert_eq!(pad_with_right("title", "", 10), "title");
        assert_eq!(pad_with_right("title", "3:45", 4), "titl");
        assert_eq!(pad_with_right("title", "3:45", 0), "");
        assert_eq!(
            pad_with_right("a longer title here", "3:45", 10),
            "a lon 3:45"
        );
    }
}
