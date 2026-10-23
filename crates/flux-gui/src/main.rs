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

use flux_app::{Controller, format_duration, is_track_playable};
use flux_core::{APP_NAME, MILESTONE, PlaybackState, TrackId};
use iced::widget::{button, column, container, row, scrollable, slider, text, text_input};
use iced::{Element, Length, Subscription, Task, window};

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
}

#[derive(Debug, Clone)]
enum Message {
    DirInputChanged(String),
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
        Self {
            controller: Controller::open_default(),
            dir_input: String::new(),
            search: String::new(),
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
                // The trailing counters are temporary playback-glitch
                // diagnostics (engine generation / callback
                // underruns): during uninterrupted playback neither
                // may move. Remove once that investigation closes.
                format!(
                    "{title}    {} / {total}    [diag: gen {}, underruns {}]",
                    format_duration(controller.position()),
                    controller.engine_generation(),
                    controller.underrun_count()
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
