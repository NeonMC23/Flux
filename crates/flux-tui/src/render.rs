//! Terminal rendering and view composition.

use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use flux_app::ImportStage;

use crate::*;

impl Tui {
    pub(crate) fn draw(&mut self, out: &mut impl Write) -> std::io::Result<()> {
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
        queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

        if !detail_view_fits(height)
            && matches!(
                self.mode,
                Mode::InfoView
                    | Mode::QueueView
                    | Mode::LyricsView
                    | Mode::ImportScanning
                    | Mode::ImportReview
                    | Mode::ImportPreparing
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

        // Header, navigation tabs, playback summary and contextual title.
        let library_label = self.library_header_identity(width);
        let mode_label = match self.mode {
            Mode::QueueView => "queue",
            Mode::InfoView => "help",
            Mode::LyricsView => "lyrics",
            Mode::ImportScanning | Mode::ImportReview | Mode::ImportPreparing => "import",
            Mode::FormInput => "edit",
            Mode::CommandInput => "command",
            Mode::TrackMovePick => "move",
            Mode::ArtistDeletion => "delete",
            Mode::ImportAlbumPick => "import",
            Mode::ConfirmTrackMove
            | Mode::ConfirmTrackReplacement
            | Mode::ConfirmTrackDelete
            | Mode::ConfirmAlbumDelete
            | Mode::ConfirmArtistDelete
            | Mode::ConfirmPlaylist
            | Mode::ConfirmQueueClear => "confirm",
            Mode::Normal => self.view.label(),
            _ => "input",
        };
        let mut header = if width < 60 {
            format!(
                "Flux · {mode_label} · {}",
                self.controller.playback_mode().label()
            )
        } else {
            format!("Flux · {library_label} · {mode_label}")
        };
        if self.controller.queue_len() > 0 {
            header.push_str(&format!(" · queue {}", self.controller.queue_len()));
        }
        if width >= 60 {
            header.push_str(&format!(" · {}", self.controller.playback_mode().label()));
        }
        if !self.search.trim().is_empty() {
            header.push_str(&format!(" · “{}”", truncate_to_width(&self.search, 20)));
        }
        if self.controller.portable_recovery().is_some() {
            header.push_str(" · recovered");
        }
        if self.controller.legacy_index_write_protected() {
            header.push_str(" · index protected");
        }
        if self.controller.is_scanning() {
            header.push_str(" · scanning…");
        }
        self.line(out, 0, &header, width, true)?;

        let compact_layout = width < 70 || height < 16 || self.layout == LayoutMode::Compact;
        if !compact_layout {
            self.draw_tabs(out, 1, width)?;
        }
        let playing_y = if compact_layout { 1 } else { 2 };
        let mut playing = self.now_playing_line(width);
        if compact_layout {
            playing = truncate_to_width(&playing, width);
        }
        self.line(out, playing_y, &playing, width, false)?;

        let context_y = if compact_layout { 2 } else { 3 };
        let list_top = (context_y + 1) as usize;
        let detail_height = height.saturating_sub(list_top + 2).max(1);
        let context = match self.mode {
            Mode::InfoView => self.info_title.clone(),
            Mode::QueueView => "Queue · stable entry order".to_owned(),
            Mode::LyricsView => self.lyrics_title.clone(),
            Mode::ImportScanning | Mode::ImportReview | Mode::ImportPreparing => {
                "Import · Source → Destination → Verify → Import".to_owned()
            }
            Mode::FormInput => self
                .form
                .as_ref()
                .map(|form| form.title.clone())
                .unwrap_or_else(|| "Edit".to_owned()),
            Mode::CommandInput => "Command palette".to_owned(),
            Mode::TrackMovePick => "Move track · choose an album or standalone".to_owned(),
            Mode::TrackReplacementInput => "Replace or repair managed audio".to_owned(),
            Mode::ConfirmTrackMove => "Confirm track move".to_owned(),
            Mode::ConfirmTrackReplacement => "Confirm audio replacement".to_owned(),
            Mode::ConfirmTrackDelete => "Confirm track deletion".to_owned(),
            Mode::ConfirmAlbumDelete => "Confirm empty-album deletion".to_owned(),
            Mode::ImportAlbumPick => "Import into an existing album · choose a target".to_owned(),
            Mode::ArtistDeletion => self
                .artist_deletion
                .as_ref()
                .map(|plan| {
                    format!(
                        "Delete artist · {} · {} owned · {} credited · {} issue(s)",
                        plan.artist_name,
                        plan.owned_tracks.len(),
                        plan.credited_tracks.len(),
                        plan.issues.len()
                    )
                })
                .unwrap_or_else(|| "Delete artist".to_owned()),
            Mode::ConfirmArtistDelete => "Confirm artist deletion".to_owned(),
            Mode::LyricsExportInput => "Export managed lyrics".to_owned(),
            Mode::ImportAssetInput => "Attach import asset".to_owned(),
            _ => self.view_title(),
        };
        queue!(out, SetForegroundColor(self.accent()))?;
        self.line(out, context_y, &context, width, true)?;
        queue!(out, SetForegroundColor(Color::Reset))?;

        if self.mode == Mode::InfoView {
            let max_scroll = self.info_lines.len().saturating_sub(detail_height);
            self.info_scroll = self.info_scroll.min(max_scroll);
            for (row, line) in self
                .info_lines
                .iter()
                .skip(self.info_scroll)
                .take(detail_height)
                .enumerate()
            {
                self.line(out, (list_top + row) as u16, line, width, false)?;
            }
        } else if self.mode == Mode::QueueView {
            self.draw_queue_view(out, list_top, detail_height, width)?;
        } else if self.mode == Mode::LyricsView {
            self.update_lyrics_follow();
            let max_scroll = self.lyrics_lines.len().saturating_sub(detail_height);
            self.lyrics_scroll = self.lyrics_scroll.min(max_scroll);
            for (row, line) in self
                .lyrics_lines
                .iter()
                .skip(self.lyrics_scroll)
                .take(detail_height)
                .enumerate()
            {
                self.line(out, (list_top + row) as u16, line, width, false)?;
            }
        } else if matches!(
            self.mode,
            Mode::ImportScanning | Mode::ImportReview | Mode::ImportPreparing
        ) {
            self.draw_import_view(out, list_top, detail_height, width)?;
        } else if self.mode == Mode::ImportAlbumPick {
            self.draw_import_album_picker(out, list_top, detail_height, width)?;
        } else if self.mode == Mode::ArtistDeletion {
            self.draw_artist_deletion(out, list_top, detail_height, width)?;
        } else if self.mode == Mode::TrackMovePick {
            self.draw_move_picker(out, list_top, detail_height, width)?;
        } else if self.mode == Mode::FormInput {
            self.draw_form(out, list_top, detail_height, width)?;
        } else {
            self.draw_workspace(out, list_top, detail_height, width)?;
        }

        // Status / error line.
        let status_y = (height - 2) as u16;
        if let Some(error) = &self.audio_error {
            queue!(out, SetForegroundColor(Color::Red))?;
            self.line(out, status_y, error, width, false)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
        } else if let Some(error) = self.controller.last_error() {
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
            Mode::Normal => self.help_line(),
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
            Mode::FormInput => {
                "Form — Tab/↑/↓ field · type to edit · Enter save · Esc cancel".to_owned()
            }
            Mode::CommandInput => input_line("Command (:tracks :artists :albums :queue :viz :layout <name>): "),
            Mode::ConfirmPlaylist => {
                "Playlist change — y confirms · any other key cancels".to_owned()
            }
            Mode::ConfirmQueueClear => {
                "Clear queue — y confirms · any other key cancels".to_owned()
            }
            Mode::ImportAlbumPick => {
                "Fixed target — ↑/↓ choose album · Enter apply to every candidate · Esc cancel".to_owned()
            }
            Mode::ArtistDeletion => {
                "Artist deletion — ↑/↓ select · t transfer · r remove credit · d delete twice · u reset · a auto · y publish · Esc cancel".to_owned()
            }
            Mode::TrackMovePick => {
                "Move — ↑/↓ choose · Enter review · s standalone · Esc cancel".to_owned()
            }
            Mode::TrackReplacementInput => input_line("Replacement audio path (Enter review, Esc cancel): "),
            Mode::ConfirmTrackMove
            | Mode::ConfirmTrackReplacement
            | Mode::ConfirmTrackDelete
            | Mode::ConfirmAlbumDelete
            | Mode::ConfirmArtistDelete => {
                "Review the status line — y confirms · any other key cancels".to_owned()
            }
            Mode::LyricsExportInput => input_line("Export lyrics path (Enter write, Esc cancel): "),
            Mode::ImportAssetInput => input_line("Asset path (Enter apply, - clears, Esc cancel): "),
            Mode::ImportInput => {
                input_line("Copy import paths separated by ; (Enter import, Esc cancel): ")
            }
            Mode::TrackEditInput => input_line(
                "Edit track title|album|track|disc|artist-id|credit-ids (Enter save, Esc cancel): ",
            ),
            Mode::ArtistCreateInput => input_line("New artist name (Enter create, Esc cancel): "),
            Mode::ArtistRenameInput => {
                input_line("Edit artist-id|name|aliases;aliases|description|image-path (empty image preserves, - removes; Enter save, Esc cancel): ")
            }
            Mode::AlbumEditInput => input_line(
                "Edit album-id|title|release-type|cover-path (empty cover preserves, - removes; Enter save, Esc cancel): ",
            ),
            Mode::TrackArtworkInput => input_line(
                "Track artwork: track-id|image-path or track-id|- (Enter save, Esc cancel): ",
            ),
            Mode::TrackLyricsInput => input_line(
                "Managed lyrics: track-id|UTF-8-file or track-id|- (Enter save, Esc cancel): ",
            ),
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
                "Lyrics — ↑/↓ scroll · f follow · x export · Esc back · q quits.".to_owned()
            }
            Mode::ImportScanning | Mode::ImportPreparing => {
                "Import worker running — Esc cancels · q quits after the worker stops.".to_owned()
            }
            Mode::ImportReview => {
                "Review — ↑/↓ select · Space include/skip · e edit · A/T/L artwork/lyrics · a all · s skip all · Enter import · Esc cancel"
                    .to_owned()
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

    pub(crate) fn draw_tabs(
        &self,
        out: &mut impl Write,
        y: u16,
        width: usize,
    ) -> std::io::Result<()> {
        queue!(out, MoveTo(0, y), Clear(ClearType::UntilNewLine))?;
        if width < 70 {
            let label = format!("{} · Tab switches views · ? help", self.view.label());
            self.line_at(out, 0, y, &label, width, true)?;
            return Ok(());
        }
        let mut x = 0usize;
        for view in View::ALL {
            let active = view == self.view;
            let label = format!(" {} ", view.label());
            if active {
                queue!(
                    out,
                    SetForegroundColor(Color::Rgb {
                        r: 0xff,
                        g: 0x66,
                        b: 0xcc,
                    }),
                    SetAttribute(Attribute::Reverse)
                )?;
            } else {
                queue!(out, SetForegroundColor(Color::Grey))?;
            }
            queue!(out, Print(&label))?;
            if active {
                queue!(out, SetAttribute(Attribute::Reset))?;
            }
            x += display_width(&label);
            if x >= width {
                break;
            }
        }
        queue!(out, SetForegroundColor(Color::Reset))?;
        Ok(())
    }

    pub(crate) fn draw_visualizer(
        &mut self,
        out: &mut impl Write,
        x: usize,
        y: u16,
        width: usize,
        height: usize,
    ) -> std::io::Result<()> {
        if width < 4 || height == 0 {
            return Ok(());
        }
        if self.controller.player_state() == PlaybackState::Playing
            && self.controller.audio_status().is_ready()
        {
            let waveform = self.controller.waveform();
            self.scope.update(&waveform);
        } else {
            self.scope.reset();
        }
        let canvas = visualization_canvas(
            self.scope.samples(),
            self.viz_mode,
            self.viz_gain,
            width,
            height,
        );
        let accent = self.accent();
        for (row, line) in canvas.into_iter().enumerate() {
            queue!(
                out,
                MoveTo(x as u16, y + row as u16),
                SetForegroundColor(accent),
                Print(truncate_to_width(&line, width)),
                SetForegroundColor(Color::Reset)
            )?;
        }
        Ok(())
    }

    pub(crate) fn draw_workspace(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        if self.layout == LayoutMode::Visualizer {
            return self.draw_visualizer(out, 0, top as u16, width, height);
        }
        if self.layout == LayoutMode::Player {
            return self.draw_player_page(out, top, height, width);
        }
        let show_side = self.layout == LayoutMode::Standard && width >= 76 && height >= 8;
        let side = if show_side {
            (width / 3).clamp(22, 38)
        } else {
            0
        };
        let list_width = width.saturating_sub(side + usize::from(side > 0));
        self.draw_list(out, top, height, list_width)?;
        if side > 0 {
            self.draw_side_panel(out, top, height, width - side, side)?;
        }
        Ok(())
    }

    pub(crate) fn draw_list(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let rows = self.view_rows();
        let len = rows.len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
        self.offset = self.offset.min(len.saturating_sub(height));
        if rows.is_empty() {
            if self.view == View::Tracks
                && self.detail.is_none()
                && self.active_collection.is_none()
            {
                let message = self.empty_rows_message();
                self.line(out, top as u16, &message, width, false)?;
                return Ok(());
            }
            let message = if let Some(detail) = self.detail {
                match detail {
                    Detail::Artist(_) => "This artist has no tracks or albums in the library.",
                    Detail::Album(_) if self.search.trim().is_empty() => {
                        "This album has no tracks."
                    }
                    Detail::Album(_) => "No tracks in this album match the search.",
                    Detail::Playlist(_) if self.search.trim().is_empty() => {
                        "This playlist is empty. Add tracks with :playlist <name>."
                    }
                    Detail::Playlist(_) => "No tracks in this playlist match the search.",
                }
            } else {
                match self.view {
                    View::Tracks if self.active_collection.is_some() => {
                        "No tracks in this collection match the current search."
                    }
                    View::Tracks => {
                        "Library is empty — press o to scan a folder, i to import files, or ? for help."
                    }
                    View::Artists => "No artists yet. Import music or press P to create one.",
                    View::Albums if self.favorites_only => "No albums match the current search.",
                    View::Albums => "No albums yet. Import music or create one in Library setup.",
                    View::Playlists => "No playlists yet — press N to create one.",
                    View::Collections => "No collections match the search.",
                    View::Lyrics => {
                        "No managed lyrics yet. Open a track and press S to attach lyrics."
                    }
                    View::Settings => "No settings available.",
                }
            };
            self.line(out, top as u16, message, width, false)?;
            return Ok(());
        }
        for (row, item) in rows.iter().skip(self.offset).take(height).enumerate() {
            let index = self.offset + row;
            let text = format!(
                "{} {}",
                if index == self.cursor { ">" } else { " " },
                self.row_text(*item, width.saturating_sub(2))
            );
            let text = truncate_to_width(&text, width);
            let y = (top + row) as u16;
            if index == self.cursor {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_side_panel(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        x: usize,
        width: usize,
    ) -> std::io::Result<()> {
        if width < 8 {
            return Ok(());
        }
        let current = self.controller.current_track();
        let mut y = top;
        queue!(out, SetForegroundColor(self.accent()))?;
        self.line_at(out, x, y as u16, "NOW PLAYING", width, true)?;
        queue!(out, SetForegroundColor(Color::Reset))?;
        y += 1;
        if let Some(track) = current {
            self.line_at(out, x, y as u16, &display_title(track), width, true)?;
            y += 1;
            self.line_at(
                out,
                x,
                y as u16,
                &format!(
                    "{} · {}",
                    self.controller
                        .current_id()
                        .map(|id| credit_display(&self.controller, id))
                        .unwrap_or_default(),
                    display_album(track)
                ),
                width,
                false,
            )?;
            y += 1;
            let elapsed = format_duration(self.controller.position());
            let total = self
                .controller
                .current_duration()
                .map(format_duration)
                .unwrap_or_else(|| "--:--".to_owned());
            let bar_width =
                width.saturating_sub(display_width(&elapsed) + display_width(&total) + 2);
            self.line_at(
                out,
                x,
                y as u16,
                &format!("{elapsed} {} {total}", self.progress_text(bar_width)),
                width,
                false,
            )?;
            y += 1;
        } else {
            self.line_at(out, x, y as u16, "Nothing playing", width, false)?;
            y += 1;
            self.line_at(
                out,
                x,
                y as u16,
                "Select a track and press Enter.",
                width,
                false,
            )?;
            y += 1;
        }
        if self.viz_visible && y + 3 < top + height {
            queue!(out, SetForegroundColor(self.accent()))?;
            self.line_at(
                out,
                x,
                y as u16,
                &format!("VISUALIZER · {}", self.viz_mode.label()),
                width,
                true,
            )?;
            queue!(out, SetForegroundColor(Color::Reset))?;
            y += 1;
            let viz_height = (top + height).saturating_sub(y + 2).clamp(2, 7);
            self.draw_visualizer(out, x, y as u16, width, viz_height)?;
            y += viz_height;
        }
        let entries = self.controller.queue_entries();
        let current_index = self
            .controller
            .current_queue_entry_id()
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .map_or(0, |index| index + 1);
        let up_next = entries
            .iter()
            .skip(current_index)
            .take((top + height).saturating_sub(y + 1))
            .collect::<Vec<_>>();
        if !up_next.is_empty() {
            queue!(out, SetForegroundColor(self.accent()))?;
            self.line_at(out, x, y as u16, "UP NEXT", width, true)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
            y += 1;
            for entry in up_next {
                let label = self.controller.track(entry.track()).map_or_else(
                    || "Missing track".to_owned(),
                    |track| {
                        format!(
                            "{} — {}",
                            display_title(track),
                            credit_display(&self.controller, entry.track())
                        )
                    },
                );
                self.line_at(out, x, y as u16, &format!("· {label}"), width, false)?;
                y += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_player_page(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let mut y = top;
        let current = self.controller.current_track();
        if let Some(track) = current {
            queue!(out, SetForegroundColor(self.accent()))?;
            self.line(out, y as u16, &display_title(track), width, true)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
            y += 1;
            self.line(
                out,
                y as u16,
                &format!(
                    "{} · {}",
                    self.controller
                        .current_id()
                        .map(|id| credit_display(&self.controller, id))
                        .unwrap_or_default(),
                    display_album(track)
                ),
                width,
                false,
            )?;
            y += 1;
        } else {
            self.line(out, y as u16, "Nothing playing", width, true)?;
            y += 1;
            self.line(
                out,
                y as u16,
                "Select a track in Tracks and press Enter.",
                width,
                false,
            )?;
            y += 1;
        }
        let elapsed = format_duration(self.controller.position());
        let total = self
            .controller
            .current_duration()
            .map(format_duration)
            .unwrap_or_else(|| "--:--".to_owned());
        self.line(
            out,
            y as u16,
            &format!(
                "{elapsed} {} {total}",
                self.progress_text(width.saturating_sub(14))
            ),
            width,
            false,
        )?;
        y += 1;
        if self.viz_visible && y + 2 < top + height {
            queue!(out, SetForegroundColor(self.accent()))?;
            self.line(
                out,
                y as u16,
                &format!("VISUALIZER · {}", self.viz_mode.label()),
                width,
                true,
            )?;
            queue!(out, SetForegroundColor(Color::Reset))?;
            y += 1;
            let viz_height = (top + height).saturating_sub(y + 4).max(2);
            self.draw_visualizer(out, 0, y as u16, width, viz_height)?;
            y += viz_height;
        }
        let entries = self.controller.queue_entries();
        let current_index = self
            .controller
            .current_queue_entry_id()
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .map_or(0, |index| index + 1);
        let up_next = entries
            .iter()
            .skip(current_index)
            .take((top + height).saturating_sub(y + 1))
            .collect::<Vec<_>>();
        if !up_next.is_empty() {
            queue!(out, SetForegroundColor(self.accent()))?;
            self.line(out, y as u16, "UP NEXT", width, true)?;
            queue!(out, SetForegroundColor(Color::Reset))?;
            y += 1;
            for entry in up_next {
                let label = self.controller.track(entry.track()).map_or_else(
                    || "Missing track".to_owned(),
                    |track| {
                        format!(
                            "{} — {}",
                            display_title(track),
                            credit_display(&self.controller, entry.track())
                        )
                    },
                );
                self.line(out, y as u16, &format!("· {label}"), width, false)?;
                y += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_move_picker(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let targets = self.move_targets();
        if targets.is_empty() {
            self.line(
                out,
                top as u16,
                "No destination albums are available.",
                width,
                false,
            )?;
            return Ok(());
        }
        self.move_cursor = self.move_cursor.min(targets.len().saturating_sub(1));
        for (index, target) in targets.iter().take(height).enumerate() {
            let text = format!(
                "{} {}{}",
                if index == self.move_cursor { ">" } else { " " },
                target.label,
                if target.current { "  [current]" } else { "" }
            );
            let y = (top + index) as u16;
            if index == self.move_cursor {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_artist_deletion(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let Some(plan) = self.artist_deletion.as_ref() else {
            return Ok(());
        };
        let count = plan.owned_tracks.len() + plan.credited_tracks.len();
        self.artist_delete_cursor = self.artist_delete_cursor.min(count.saturating_sub(1));
        for (index, track) in plan
            .owned_tracks
            .iter()
            .chain(plan.credited_tracks.iter())
            .take(height)
            .enumerate()
        {
            let decision = match &track.decision {
                ArtistDeletionDecision::Unresolved => "unresolved".to_owned(),
                ArtistDeletionDecision::Transfer { new_owner, .. } => format!(
                    "transfer → {}",
                    self.controller
                        .artist(*new_owner)
                        .map(|artist| artist.name().to_owned())
                        .unwrap_or_else(|| format!("Artist {new_owner}"))
                ),
                ArtistDeletionDecision::RemoveCredit { .. } => "remove credit".to_owned(),
                ArtistDeletionDecision::Delete { confirmed: true } => {
                    "DELETE (confirmed)".to_owned()
                }
                ArtistDeletionDecision::Delete { confirmed: false } => {
                    "delete (unconfirmed)".to_owned()
                }
            };
            let ownership = if track.owner_artist_id == plan.artist_id {
                "owned"
            } else {
                "credited"
            };
            let text = format!(
                "{} {} · {} · {} · {}",
                if index == self.artist_delete_cursor {
                    ">"
                } else {
                    " "
                },
                track.title,
                ownership,
                track.album_title,
                decision
            );
            let y = (top + index) as u16;
            if index == self.artist_delete_cursor {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_import_album_picker(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let targets = self.import_album_targets();
        if targets.is_empty() {
            self.line(
                out,
                top as u16,
                "No existing albums are available for a fixed-target review.",
                width,
                false,
            )?;
            return Ok(());
        }
        self.import_album_cursor = self
            .import_album_cursor
            .min(targets.len().saturating_sub(1));
        for (index, target) in targets.iter().take(height).enumerate() {
            let text = format!(
                "{} {} · {} · {} · {}",
                if index == self.import_album_cursor {
                    ">"
                } else {
                    " "
                },
                target.title,
                target.owner_name,
                sentence_case(target.release_type.as_str()),
                count_label(target.track_order.len(), "track")
            );
            let y = (top + index) as u16;
            if index == self.import_album_cursor {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_form(
        &self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let Some(form) = self.form.as_ref() else {
            return Ok(());
        };
        let visible = height.saturating_sub(2).max(1);
        let start = form.focus.saturating_sub(visible.saturating_sub(1));
        let mut y = top;
        for (index, field) in form.fields.iter().enumerate().skip(start).take(visible) {
            let marker = if index == form.focus { ">" } else { " " };
            let line = format!(
                "{marker} {:<18} {}",
                format!("{}:", field.label),
                field.value
            );
            if index == form.focus {
                queue!(out, MoveTo(0, y as u16), SetAttribute(Attribute::Reverse))?;
                self.line(out, y as u16, &line, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y as u16, &line, width, false)?;
            }
            y += 1;
        }
        if y < top + height {
            if let Some(error) = &form.error {
                queue!(out, SetForegroundColor(Color::Red))?;
                self.line(out, y as u16, error, width, false)?;
                queue!(out, SetForegroundColor(Color::Reset))?;
            } else if let Some(field) = form.fields.get(form.focus) {
                self.line(out, y as u16, &format!("· {}", field.hint), width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_queue_view(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let entries = self.controller.queue_entries();
        if self.queue_follow
            && let Some(entry) = self.controller.current_queue_entry_id()
            && let Some(index) = self.controller.queue_entry_index(entry)
        {
            self.queue_cursor = index;
        }
        self.queue_cursor = self.queue_cursor.min(entries.len().saturating_sub(1));
        if self.queue_cursor < self.queue_offset {
            self.queue_offset = self.queue_cursor;
        }
        if self.queue_cursor >= self.queue_offset + height {
            self.queue_offset = self.queue_cursor + 1 - height;
        }
        self.queue_offset = self.queue_offset.min(entries.len().saturating_sub(height));
        if entries.is_empty() {
            self.line(
                out,
                top as u16,
                "Queue is empty — press a or x on a track to add it.",
                width,
                false,
            )?;
            return Ok(());
        }
        for (row, entry) in entries
            .iter()
            .skip(self.queue_offset)
            .take(height)
            .enumerate()
        {
            let index = self.queue_offset + row;
            let current = self.controller.current_queue_entry_id() == Some(entry.id());
            let track = self.controller.track(entry.track());
            let mut text = format!(
                "{} {}. {} — {}",
                if index == self.queue_cursor { ">" } else { " " },
                index + 1,
                track.map_or_else(|| "Missing from library".to_owned(), display_title,),
                track.map_or_else(
                    || "unavailable".to_owned(),
                    |_| credit_display(&self.controller, entry.track()),
                ),
            );
            if current {
                text.push_str("  ▶");
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
            let y = (top + row) as u16;
            if index == self.queue_cursor {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw_import_view(
        &mut self,
        out: &mut impl Write,
        top: usize,
        height: usize,
        width: usize,
    ) -> std::io::Result<()> {
        let progress = self.import_progress_snapshot();
        if matches!(self.mode, Mode::ImportScanning | Mode::ImportPreparing) {
            let stage = match progress.stage {
                ImportStage::Discovering => "Discovering files",
                ImportStage::ReadingMetadata => "Reading local metadata",
                ImportStage::DetectingDuplicates => "Checking duplicates",
                ImportStage::Copying => "Copying selected files",
                ImportStage::WritingCatalogue => "Publishing catalogue",
                ImportStage::Completed => "Completed",
                ImportStage::Cancelled => "Cancelled",
                ImportStage::Failed => "Failed",
                ImportStage::Idle => "Preparing",
            };
            let title = format!(
                "{stage} · {}/{} · Esc cancel · q quit",
                progress.done, progress.total
            );
            self.line(out, top as u16, &title, width, true)?;
            let bar_width = width.saturating_sub(12).max(1);
            let filled = (progress.done.min(progress.total) * bar_width)
                .checked_div(progress.total)
                .unwrap_or(0);
            let bar = format!(
                "[{}{}]",
                "#".repeat(filled),
                "-".repeat(bar_width.saturating_sub(filled))
            );
            self.line(out, (top + 1) as u16, &bar, width, false)?;
            if height > 1 {
                self.line(out, (top + 2) as u16, &progress.message, width, false)?;
            }
            if let Some(ui) = self.import_ui.as_ref()
                && let Some(error) = &ui.error
                && height > 2
            {
                self.line(out, (top + 3) as u16, error, width, false)?;
            }
            return Ok(());
        }
        let (count, issue_count, rows, error) = {
            let Some(ui) = self.import_ui.as_mut() else {
                return Ok(());
            };
            let Some(plan) = ui.plan.as_ref() else {
                return Ok(());
            };
            let count = plan.candidates.len();
            let issue_count = plan.issues.len();
            let rows_height = height.saturating_sub(1).max(1);
            ui.selected = ui.selected.min(count.saturating_sub(1));
            if ui.selected < ui.scroll {
                ui.scroll = ui.selected;
            }
            if ui.selected >= ui.scroll + rows_height {
                ui.scroll = ui.selected + 1 - rows_height;
            }
            ui.scroll = ui.scroll.min(count.saturating_sub(rows_height));
            let rows = plan.candidates[ui.scroll.min(count)..count.min(ui.scroll + rows_height)]
                .iter()
                .enumerate()
                .map(|(row, candidate)| {
                    let index = ui.scroll + row;
                    let marker = match candidate.decision() {
                        ImportDecision::ImportNew => "[import]",
                        ImportDecision::UpdateExisting => "[update]",
                        ImportDecision::Skip => "[ skip ]",
                    };
                    let album = candidate
                        .album()
                        .map(|album| format!(" · {album}"))
                        .unwrap_or_else(|| " · standalone".to_owned());
                    let status = match candidate.status() {
                        CandidateStatus::New => String::new(),
                        CandidateStatus::ExistingPath(_) => " · same path".to_owned(),
                        CandidateStatus::SameContent { .. } => " · same audio".to_owned(),
                        CandidateStatus::SimilarMetadata(_) => " · check details".to_owned(),
                        CandidateStatus::SameTitleDifferentAudio(_) => {
                            " · different audio".to_owned()
                        }
                        CandidateStatus::ProbableAudio(_) => " · possible duplicate".to_owned(),
                        CandidateStatus::AmbiguousAudio(_) => " · needs a decision".to_owned(),
                    };
                    (
                        index == ui.selected,
                        format!(
                            "{} {marker} {} — {}{album}{status}",
                            if index == ui.selected { ">" } else { " " },
                            candidate.title(),
                            candidate.artist_name().unwrap_or("Unknown Artist")
                        ),
                    )
                })
                .collect::<Vec<_>>();
            (count, issue_count, rows, ui.error.clone())
        };
        let title = if issue_count > 0 {
            format!(
                "Import review · {count} track(s) · {issue_count} unreadable · e edit · A/T/L assets · Enter import · Esc cancel"
            )
        } else {
            format!(
                "Import review · {count} track(s) · e edit · A/T/L assets · Enter import · Esc cancel"
            )
        };
        self.line(out, top as u16, &title, width, true)?;
        for (row, (selected, text)) in rows.into_iter().enumerate() {
            let y = (top + 1 + row) as u16;
            if selected {
                queue!(out, MoveTo(0, y), SetAttribute(Attribute::Reverse))?;
                self.line(out, y, &text, width, false)?;
                queue!(out, SetAttribute(Attribute::Reset))?;
            } else {
                self.line(out, y, &text, width, false)?;
            }
        }
        if let Some(error) = error
            && height > 0
        {
            self.line(out, (top + height) as u16, &error, width, false)?;
        }
        Ok(())
    }

    pub(crate) fn progress_text(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let position = self.controller.position();
        let total = self.controller.current_duration();
        let fraction = total
            .filter(|duration| !duration.is_zero())
            .map_or(0.0, |duration| {
                (position.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
            });
        let filled = (fraction * width as f32).round() as usize;
        format!(
            "{}{}",
            "█".repeat(filled.min(width)),
            "░".repeat(width.saturating_sub(filled))
        )
    }

    pub(crate) fn view_title(&self) -> String {
        if let Some(collection) = self.active_collection {
            return format!("{} · {}", collection.label(), collection.description());
        }
        if let Some(detail) = self.detail {
            match detail {
                Detail::Artist(id) => {
                    let name = self
                        .controller
                        .artist(id)
                        .map(|artist| artist.name().to_owned())
                        .unwrap_or_else(|| "Artist".to_owned());
                    let tracks = self
                        .controller
                        .tracks()
                        .iter()
                        .filter(|(track, _)| {
                            self.controller.track_artist_id(*track) == Some(id)
                                || self.controller.track_credits(*track).contains(&id)
                        })
                        .count();
                    let albums = self.controller.albums_owned_by(id).len()
                        + self.controller.albums_credited_for(id).len();
                    return format!(
                        "Artist · {name} · {} · {}",
                        count_label(albums, "album"),
                        count_label(tracks, "track")
                    );
                }
                Detail::Album(id) => {
                    if let Some(album) = self.controller.album(id) {
                        let owner = self
                            .controller
                            .artist(album.owner_artist_id)
                            .map(|artist| artist.name().to_owned())
                            .unwrap_or_else(|| "Unknown artist".to_owned());
                        return format!(
                            "Album · {} · {owner} · {} · {} track(s)",
                            album.title,
                            sentence_case(album.release_type.as_str()),
                            album.track_order.len()
                        );
                    }
                }
                Detail::Playlist(id) => {
                    if let Some(playlist) = self.controller.playlist(id) {
                        return format!(
                            "Playlist · {} · {} track(s) · Enter play · d remove · c clear · R rename · x delete",
                            playlist.name,
                            playlist.track_ids.len()
                        );
                    }
                }
            }
        }
        match self.view {
            View::Tracks => {
                let count = self.view_rows().len();
                if self.favorites_only {
                    format!("Tracks · favorites · {}", count_label(count, "track"))
                } else {
                    format!("Tracks · {}", count_label(count, "track"))
                }
            }
            View::Artists => format!(
                "Artists · {} · Enter opens albums and tracks",
                count_label(self.view_rows().len(), "artist")
            ),
            View::Albums => format!(
                "Albums · {} · Enter opens the track listing",
                count_label(self.view_rows().len(), "album")
            ),
            View::Playlists => "Playlists · N new · Enter open · R rename · x delete".to_owned(),
            View::Collections => "Collections · Enter filters Tracks".to_owned(),
            View::Lyrics => format!(
                "Lyrics library · {} track(s) · Enter reads",
                self.view_rows().len()
            ),
            View::Settings => "Settings · Enter toggles · d refreshes audio devices".to_owned(),
        }
    }

    pub(crate) fn row_text(&self, row: Row, width: usize) -> String {
        let current = self.controller.current_id();
        match row {
            Row::Track(id) | Row::LyricsTrack(id) => {
                let Some(track) = self.controller.track(id) else {
                    return "Missing track".to_owned();
                };
                let favorite = self
                    .controller
                    .rating(id)
                    .is_some_and(|rating| rating.favorite);
                let stars = self
                    .controller
                    .rating(id)
                    .and_then(|rating| rating.rating)
                    .map(|rating| format!("★{rating}"))
                    .unwrap_or_default();
                let marker = if current == Some(id) { "▶" } else { " " };
                let favorite_marker = if favorite { "♥" } else { " " };
                let mut text = format!(
                    "{marker}{favorite_marker} {}{}  · {}  · {}{}",
                    track.title(),
                    if self.controller.track_album_id(id).is_none() {
                        "  [standalone]"
                    } else {
                        ""
                    },
                    credit_display(&self.controller, id),
                    display_album(track),
                    if stars.is_empty() {
                        String::new()
                    } else {
                        format!("  {stars}")
                    }
                );
                if matches!(row, Row::LyricsTrack(_)) {
                    let kind = self
                        .controller
                        .track_lyrics(id)
                        .and_then(|raw| flux_app::LyricsDocument::parse(&raw).ok())
                        .map(|document| match document.kind() {
                            flux_app::LyricsKind::Synced => "synced",
                            flux_app::LyricsKind::Plain => "plain",
                        })
                        .unwrap_or("lyrics");
                    text.push_str(&format!("  [{kind}]"));
                }
                if !is_track_playable(track) {
                    text.push_str("  [not playable]");
                }
                if let Some(label) = resource_label(self.controller.track_audio_resource_state(id))
                {
                    text.push_str("  ");
                    text.push_str(label);
                }
                let duration = track.duration().map(format_duration).unwrap_or_default();
                pad_with_right(&text, &duration, width)
            }
            Row::Artist(id) => {
                let Some(artist) = self.controller.artist(id) else {
                    return "Missing artist".to_owned();
                };
                let tracks = self
                    .controller
                    .tracks()
                    .iter()
                    .filter(|(track, _)| {
                        self.controller.track_artist_id(*track) == Some(id)
                            || self.controller.track_credits(*track).contains(&id)
                    })
                    .count();
                let albums = self.controller.albums_owned_by(id).len()
                    + self.controller.albums_credited_for(id).len();
                pad_with_right(
                    &format!(
                        "{}  · {} album(s) · {} track(s)",
                        artist.name(),
                        albums,
                        tracks
                    ),
                    "",
                    width,
                )
            }
            Row::Album(id) => {
                let Some(album) = self.controller.album(id) else {
                    return "Missing album".to_owned();
                };
                let owner = self
                    .controller
                    .artist(album.owner_artist_id)
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_else(|| "Unknown artist".to_owned());
                let mut text = format!(
                    "{}  · {owner}  · {}  · {} track(s)",
                    album.title,
                    sentence_case(album.release_type.as_str()),
                    album.track_order.len()
                );
                if album.cover.is_some() {
                    text.push_str("  [cover]");
                }
                pad_with_right(&text, "", width)
            }
            Row::Playlist(id) => {
                let Some(playlist) = self.controller.playlist(id) else {
                    return "Missing playlist".to_owned();
                };
                pad_with_right(
                    &format!("{}  · {} track(s)", playlist.name, playlist.track_ids.len()),
                    "",
                    width,
                )
            }
            Row::Collection(collection) => pad_with_right(
                &format!("{}  · {}", collection.label(), collection.description()),
                &format!(
                    "{}",
                    self.controller.smart_collection_tracks(collection).len()
                ),
                width,
            ),
            Row::Setting(setting) => {
                let value = match setting {
                    SettingRow::Layout => self.layout.label().to_owned(),
                    SettingRow::Visualizer => {
                        if self.viz_visible {
                            "Shown".to_owned()
                        } else {
                            "Hidden".to_owned()
                        }
                    }
                    SettingRow::VisualizationMode => self.viz_mode.label().to_owned(),
                    SettingRow::FollowPlayback => {
                        if self.follow_lyrics {
                            "On".to_owned()
                        } else {
                            "Off".to_owned()
                        }
                    }
                    SettingRow::RefreshDevices => {
                        if self.audio_loading {
                            "Refreshing…".to_owned()
                        } else {
                            format!("{} found", self.audio_devices.len())
                        }
                    }
                    SettingRow::LibrarySetup => "Open".to_owned(),
                };
                pad_with_right(&format!("{}  · {}", setting.label(), value), "", width)
            }
            Row::AudioDevice(index) => {
                let (name, id, default) = match index {
                    None => ("System default output".to_owned(), None, true),
                    Some(index) => match self.audio_devices.get(index) {
                        Some(device) => (device.name.clone(), device.id.clone(), device.is_default),
                        None => return "Missing audio device".to_owned(),
                    },
                };
                let active = self.controller.selected_audio_device().map(str::to_owned) == id;
                let duplicate_name = self
                    .audio_devices
                    .iter()
                    .filter(|device| device.name == name)
                    .count()
                    > 1;
                let mut text = name;
                if default {
                    text.push_str("  · system default");
                }
                if active {
                    text.push_str("  · active");
                }
                if let Some(id) = id
                    && duplicate_name
                {
                    text.push_str(&format!("  [{id}]"));
                }
                pad_with_right(&text, "", width)
            }
        }
    }

    pub(crate) fn help_line(&self) -> String {
        if let Some(detail) = self.detail {
            return match detail {
                Detail::Artist(_) => "↑/↓ select · Enter opens album/track · Y edit · Delete removes an empty artist · Esc back".to_owned(),
                Detail::Album(_) => "↑/↓ select · Enter play · Z edit · Ctrl+↑/↓ reorder · Delete removes a track/empty album · Esc back".to_owned(),
                Detail::Playlist(_) => "↑/↓ select · Enter play · l play all · Alt+s shuffle · d remove · Ctrl+↑/↓ reorder · c clear · R rename · Esc back".to_owned(),
            };
        }
        match self.view {
            View::Tracks => "↑/↓ move · Enter play · a queue · x next · e edit · Alt+m move · Delete delete · f favorite · 0–5 rate · / search".to_owned(),
            View::Artists => "↑/↓ select · Enter opens artist · Y edit · P create · Esc back · Tab views".to_owned(),
            View::Albums => "↑/↓ select · Enter opens album · Z edit · Esc back · Tab views".to_owned(),
            View::Playlists => "↑/↓ select · Enter open · N new · R rename · x delete · Esc back".to_owned(),
            View::Collections => "↑/↓ select · Enter filters Tracks · Esc back · Tab views".to_owned(),
            View::Lyrics => "↑/↓ select · Enter reads lyrics · y current track · Esc back · Tab views".to_owned(),
            View::Settings => "↑/↓ select · Enter toggles · d refresh devices · Tab views · ? help".to_owned(),
        }
    }

    pub(crate) fn now_playing_line(&self, _width: usize) -> String {
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
                    "{state_icon} {} — {}    {} / {total}    {}    Vol {volume}%    {crossfade}",
                    track.title(),
                    self.controller
                        .current_id()
                        .map(|id| credit_display(&self.controller, id))
                        .unwrap_or_default(),
                    format_duration(self.controller.position()),
                    self.progress_text(12),
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

    pub(crate) fn line(
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

    pub(crate) fn line_at(
        &self,
        out: &mut impl Write,
        x: usize,
        y: u16,
        text: &str,
        width: usize,
        bold: bool,
    ) -> std::io::Result<()> {
        if width == 0 {
            return Ok(());
        }
        let text = truncate_to_width(text, width);
        queue!(out, MoveTo(x as u16, y), Clear(ClearType::UntilNewLine))?;
        if bold {
            queue!(
                out,
                SetAttribute(Attribute::Bold),
                Print(text),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(out, Print(text))?;
        }
        Ok(())
    }

    pub(crate) fn accent(&self) -> Color {
        Color::Rgb {
            r: 0xff,
            g: 0x66,
            b: 0xcc,
        }
    }

    pub(crate) fn update_lyrics_follow(&mut self) {
        if !self.follow_lyrics || !self.lyrics_follow_ready {
            return;
        }
        let Some(id) = self.lyrics_track else {
            return;
        };
        if self.controller.current_id() != Some(id) {
            return;
        }
        let position = self
            .controller
            .position()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let mut target = None;
        for (index, timestamp) in self.lyrics_timestamps.iter().enumerate() {
            if let Some(timestamp) = timestamp
                && *timestamp <= position
            {
                target = Some(index);
            }
        }
        if let Some(index) = target {
            self.lyrics_scroll = index.saturating_sub(2);
        }
    }
}
