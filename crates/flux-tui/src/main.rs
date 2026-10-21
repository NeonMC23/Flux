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
//! - `o` scan a folder (type a path), `r` refresh scanned folders
//! - `/` search/filter, `Esc` clear search or cancel input
//! - `q` or `Ctrl-C` quit (terminal state is always restored)

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use flux_app::{Controller, format_duration, is_track_playable};
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

#[derive(PartialEq, Eq)]
enum Mode {
    Normal,
    ScanInput,
    SearchInput,
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
}

impl Tui {
    /// The visible rows, owned so drawing/handling can mutate the UI
    /// state while iterating (the call happens at most ~10×/s).
    fn rows(&self) -> Vec<(TrackId, Track)> {
        self.controller
            .search(&self.search)
            .into_iter()
            .map(|(id, track)| (id, track.clone()))
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
                if let Some((id, _)) = self.rows().get(self.cursor) {
                    let id = *id;
                    self.controller.play_track(id);
                }
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
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
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
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
                    Mode::Normal => {}
                }
                self.mode = Mode::Normal;
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

    /// After next/previous, put the cursor on the player's selection.
    fn follow_current(&mut self) {
        if let Some(current) = self.controller.current_id()
            && let Some(pos) = self.rows().iter().position(|(id, _)| *id == current)
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
        let mut header = format!("Flux {MILESTONE} — terminal player");
        if self.controller.is_scanning() {
            header.push_str("   [scanning…]");
        }
        if let Some(reason) = self.controller.audio_status().reason() {
            header.push_str(&format!("   [audio: {reason}]"));
        }
        self.line(out, 0, &header, width, true)?;

        // Now playing.
        self.line(out, 1, &self.now_playing_line(width), width, false)?;
        self.line(out, 2, "", width, false)?;

        // Track list.
        if rows.is_empty() {
            let msg = if self.controller.tracks().is_empty() {
                "Library is empty — press o, type a music folder path, and press Enter to scan."
            } else {
                "No tracks match the current search — Esc clears it."
            };
            self.line(out, list_top as u16, msg, width, false)?;
        }
        for (row, (id, track)) in rows.iter().skip(self.offset).take(list_height).enumerate() {
            let index = self.offset + row;
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
        } else if let Some(status) = self.controller.last_status() {
            self.line(out, status_y, status, width, false)?;
        }

        // Help / input line.
        let help = match self.mode {
            Mode::Normal => "Enter play · Space pause · s stop · n/p next/prev · +/- volume \
                             · o scan · r refresh · / search · R repeat · q quit"
                .to_owned(),
            Mode::ScanInput => format!(
                "Scan folder (Enter to scan, Esc to cancel): {}_",
                self.input
            ),
            Mode::SearchInput => format!("Search (Enter to apply, Esc to cancel): {}_", self.input),
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
            None => format!("{state_icon} Nothing selected    Vol {volume}%"),
        }
    }
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
    let mut controller = Controller::open_default();
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
    };
    let mut stdout = std::io::stdout();
    let result = tui.event_loop(&mut stdout);

    drop(guard);
    tui.controller.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::pad_with_right;

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
