//! Vendored SVG icon set.
//!
//! Flux uses a small curated subset of [Lucide](https://lucide.dev) icons
//! (ISC license, see `assets/icons/LICENSE`). The SVG files are compiled into
//! the binary with [`include_bytes!`], so icon rendering never touches the
//! network and works identically on Linux, Windows and macOS.
//!
//! Icons are decorative: callers must provide a text label or an accessible
//! name for every icon-only control.

use iced::widget::svg;
use iced::{Color, ContentFit, Element, Length};

/// One icon used by the Flux interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Home,
    Tracks,
    Artists,
    Albums,
    Folders,
    Playlists,
    Collections,
    Lyrics,
    Settings,
    Import,
    Search,
    Back,
    Forward,
    Up,
    Down,
    Play,
    Pause,
    Previous,
    Next,
    Stop,
    Repeat,
    RepeatOne,
    Shuffle,
    Favorite,
    More,
    Close,
    Add,
    Music,
    Edit,
    Volume,
    Grip,
    Remove,
    Filter,
    Sort,
    Check,
    Circle,
    Clock,
    Star,
    Image,
    Refresh,
    Download,
    External,
    CalendarAdded,
    History,
    Trending,
    LyricsMissing,
    SyncedLyrics,
    ArtworkMissing,
    Queue,
    Success,
}

impl Icon {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Home => include_bytes!("../assets/icons/house.svg"),
            Self::Tracks => include_bytes!("../assets/icons/audio-lines.svg"),
            Self::Artists => include_bytes!("../assets/icons/mic-vocal.svg"),
            Self::Albums => include_bytes!("../assets/icons/disc-3.svg"),
            Self::Folders => include_bytes!("../assets/icons/folder.svg"),
            Self::Playlists => include_bytes!("../assets/icons/list-music.svg"),
            Self::Collections => include_bytes!("../assets/icons/sparkles.svg"),
            Self::Lyrics => include_bytes!("../assets/icons/mic.svg"),
            Self::Settings => include_bytes!("../assets/icons/settings.svg"),
            Self::Import => include_bytes!("../assets/icons/import.svg"),
            Self::Search => include_bytes!("../assets/icons/search.svg"),
            Self::Back => include_bytes!("../assets/icons/chevron-left.svg"),
            Self::Forward => include_bytes!("../assets/icons/chevron-right.svg"),
            Self::Up => include_bytes!("../assets/icons/chevron-up.svg"),
            Self::Down => include_bytes!("../assets/icons/chevron-down.svg"),
            Self::Play => include_bytes!("../assets/icons/play.svg"),
            Self::Pause => include_bytes!("../assets/icons/pause.svg"),
            Self::Previous => include_bytes!("../assets/icons/skip-back.svg"),
            Self::Next => include_bytes!("../assets/icons/skip-forward.svg"),
            Self::Stop => include_bytes!("../assets/icons/square.svg"),
            Self::Repeat => include_bytes!("../assets/icons/repeat.svg"),
            Self::RepeatOne => include_bytes!("../assets/icons/repeat-1.svg"),
            Self::Shuffle => include_bytes!("../assets/icons/shuffle.svg"),
            Self::Favorite => include_bytes!("../assets/icons/heart.svg"),
            Self::More => include_bytes!("../assets/icons/ellipsis.svg"),
            Self::Close => include_bytes!("../assets/icons/x.svg"),
            Self::Add => include_bytes!("../assets/icons/plus.svg"),
            Self::Music => include_bytes!("../assets/icons/audio-lines.svg"),
            Self::Edit => include_bytes!("../assets/icons/pencil.svg"),
            Self::Volume => include_bytes!("../assets/icons/volume-2.svg"),
            Self::Grip => include_bytes!("../assets/icons/grip-vertical.svg"),
            Self::Remove => include_bytes!("../assets/icons/trash.svg"),
            Self::Filter => include_bytes!("../assets/icons/funnel.svg"),
            Self::Sort => include_bytes!("../assets/icons/arrow-down-up.svg"),
            Self::Check => include_bytes!("../assets/icons/check.svg"),
            Self::Circle => include_bytes!("../assets/icons/circle.svg"),
            Self::Clock => include_bytes!("../assets/icons/clock.svg"),
            Self::Star => include_bytes!("../assets/icons/star.svg"),
            Self::Image => include_bytes!("../assets/icons/image.svg"),
            Self::Refresh => include_bytes!("../assets/icons/refresh-cw.svg"),
            Self::Download => include_bytes!("../assets/icons/download.svg"),
            Self::External => include_bytes!("../assets/icons/external-link.svg"),
            Self::CalendarAdded => include_bytes!("../assets/icons/calendar-plus.svg"),
            Self::History => include_bytes!("../assets/icons/clock-arrow-down.svg"),
            Self::Trending => include_bytes!("../assets/icons/trending-up.svg"),
            Self::LyricsMissing => include_bytes!("../assets/icons/mic-off.svg"),
            Self::SyncedLyrics => include_bytes!("../assets/icons/mic-vocal.svg"),
            Self::ArtworkMissing => include_bytes!("../assets/icons/image-off.svg"),
            Self::Queue => include_bytes!("../assets/icons/list-ordered.svg"),
            Self::Success => include_bytes!("../assets/icons/circle-check.svg"),
        }
    }
}

/// Renders the canonical Flux application mark at a square size.
///
/// The mark carries its own identity colours, so it is not tinted.
pub fn brand<'a, Message: 'a>(size: f32) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(include_bytes!(
        "../../../assets/flux-icon.svg"
    )))
    .width(Length::Fixed(size))
    .height(Length::Fixed(size))
    .content_fit(ContentFit::Contain)
    .into()
}

/// Renders one icon at a square size with a solid tint.
///
/// The icon is decorative. Pair it with visible text or a tooltip/accessible
/// label at the control boundary.
pub fn icon<'a, Message: 'a>(kind: Icon, size: f32, color: Color) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(kind.bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .content_fit(ContentFit::Contain)
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into()
}
