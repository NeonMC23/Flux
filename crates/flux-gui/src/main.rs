//! Flux desktop GUI entry point (Iced).
//!
//! Milestone M0 scope: open a minimal static startup view that shares its
//! identity text with the TUI through `flux-core`. There is intentionally no
//! library, playback, or settings functionality yet.

use flux_core::{APP_NAME, startup_message};
use iced::widget::{Column, column, text};

pub fn main() -> iced::Result {
    iced::application(Flux::default, Flux::update, Flux::view).run()
}

#[derive(Default)]
struct Flux;

#[derive(Debug, Clone)]
enum Message {}

impl Flux {
    fn update(&mut self, message: Message) {
        match message {}
    }

    fn view(&self) -> Column<'_, Message> {
        column![text(APP_NAME).size(32), text(startup_message())]
            .spacing(12)
            .padding(24)
    }
}
