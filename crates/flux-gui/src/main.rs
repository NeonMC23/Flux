//! Flux desktop GUI (`flux`), built on Iced.
//!
//! A compact, dark desktop player over the shared
//! [`flux_app::Controller`] — the same controller the TUI uses. This file
//! owns presentation and view state only (navigation, pagination, setup
//! panels); every playback, library, and persistence decision stays in
//! the controller.
//!
//! Real audio output requires a build with `--features cpal`; otherwise
//! the player bar says so and playback attempts report why.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_app::{
    AudioStatus, Controller, MigrationPlan, ReconcileReport, StorageMode, WAVEFORM_WINDOW,
    default_index_path, format_duration, is_track_playable,
};
use flux_core::{APP_NAME, PlaybackState, QueueEntry, QueueEntryId, RepeatMode, Track, TrackId};
use iced::advanced::Widget;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as RendererTrait};
use iced::advanced::widget::Tree;
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::{
    button, column, container, mouse_area, progress_bar, row, scrollable, slider, text, text_input,
};
use iced::{
    Alignment, Border, Color, Element, Length, Rectangle, Shadow, Subscription, Task, Theme, mouse,
    window,
};

// ------------------------------------------------------------------ look ---

const BG: Color = Color::from_rgb8(0x0d, 0x0b, 0x12);
const SIDEBAR: Color = Color::from_rgb8(0x12, 0x10, 0x18);
const SURFACE: Color = Color::from_rgb8(0x18, 0x15, 0x20);
const SURFACE_HI: Color = Color::from_rgb8(0x22, 0x1d, 0x2b);
const BORDER: Color = Color::from_rgb8(0x2c, 0x25, 0x38);
const TEXT: Color = Color::from_rgb8(0xf5, 0xef, 0xf8);
const MUTED: Color = Color::from_rgb8(0x99, 0x8f, 0xa6);
const ACCENT: Color = Color::from_rgb8(0xff, 0x66, 0xcc);
const ACCENT_SOFT: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.13);
const ACCENT_BORDER: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.38);
const DANGER: Color = Color::from_rgb8(0xff, 0x78, 0x8c);
const DANGER_BG: Color = Color::from_rgba8(0x5a, 0x1c, 0x2a, 0.42);
const SUCCESS: Color = Color::from_rgb8(0x7d, 0xe2, 0xb4);

const TRACK_PAGE_SIZE: usize = 150;
const GROUP_PAGE_SIZE: usize = 100;
const QUEUE_PAGE_SIZE: usize = 8;
const QUEUE_ROW_HEIGHT: f32 = 38.0;
const WAVE_TICK: Duration = Duration::from_millis(16);
const OSC_HEIGHT: f32 = 48.0;
const OSC_RADIUS: f32 = 14.0;
const OSC_LINE_WIDTH: f32 = 1.0;
const OSC_BACKGROUND: Color = Color::from_rgba8(6, 8, 12, 0.85);
const OSC_COLOR: Color = Color::from_rgb8(0xff, 0x66, 0xcc);

fn flux_theme() -> Theme {
    Theme::custom(
        "Flux",
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            warning: Color::from_rgb8(0xff, 0xc2, 0x66),
            danger: DANGER,
        },
    )
}

fn app_style(_: &Theme) -> container::Style {
    container::Style::default().background(BG).color(TEXT)
}

fn sidebar_style(_: &Theme) -> container::Style {
    container::Style::default().background(SIDEBAR)
}

fn card_style(_: &Theme) -> container::Style {
    container::Style::default()
        .background(SURFACE)
        .border(Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        })
}

fn player_style(_: &Theme) -> container::Style {
    container::Style::default()
        .background(SIDEBAR)
        .border(Border {
            color: BORDER,
            width: 0.0,
            radius: 0.0.into(),
        })
}

fn rule_style(_: &Theme) -> container::Style {
    container::Style::default().background(BORDER)
}

fn error_banner_style(_: &Theme) -> container::Style {
    container::Style::default()
        .background(DANGER_BG)
        .border(Border {
            color: Color::from_rgba8(0xff, 0x78, 0x8c, 0.45),
            width: 1.0,
            radius: 10.0.into(),
        })
        .color(TEXT)
}

fn artwork_style(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        container::Style::default()
            .background(color)
            .border(Border {
                color: Color::from_rgba8(0xff, 0xff, 0xff, 0.08),
                width: 1.0,
                radius: 12.0.into(),
            })
    }
}

fn input_style(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(
        status,
        text_input::Status::Focused { .. } | text_input::Status::Hovered
    );
    text_input::Style {
        background: SURFACE_HI.into(),
        border: Border {
            color: if focused { ACCENT_BORDER } else { BORDER },
            width: 1.0,
            radius: 8.0.into(),
        },
        icon: MUTED,
        placeholder: MUTED,
        value: TEXT,
        selection: ACCENT_SOFT,
    }
}

fn nav_style(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if active {
            ACCENT_SOFT
        } else if hovered {
            SURFACE_HI
        } else {
            Color::TRANSPARENT
        };
        button::Style {
            background: Some(background.into()),
            text_color: if active {
                ACCENT
            } else if hovered {
                TEXT
            } else {
                MUTED
            },
            border: Border {
                color: if active {
                    ACCENT_BORDER
                } else {
                    Color::TRANSPARENT
                },
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

fn ghost_style(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(
            if hovered {
                SURFACE_HI
            } else {
                Color::TRANSPARENT
            }
            .into(),
        ),
        text_color: if hovered { TEXT } else { MUTED },
        border: Border {
            color: Color::TRANSPARENT,
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

fn accent_button_style(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Color::from_rgb8(0xff, 0x86, 0xd8),
        button::Status::Pressed => Color::from_rgb8(0xe6, 0x55, 0xb6),
        button::Status::Disabled => SURFACE_HI,
        button::Status::Active => ACCENT,
    };
    button::Style {
        background: Some(background.into()),
        text_color: if matches!(status, button::Status::Disabled) {
            MUTED
        } else {
            BG
        },
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

fn danger_button_style(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(
            if hovered {
                Color::from_rgba8(0xff, 0x78, 0x8c, 0.18)
            } else {
                Color::TRANSPARENT
            }
            .into(),
        ),
        text_color: DANGER,
        border: Border {
            color: Color::from_rgba8(0xff, 0x78, 0x8c, 0.45),
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

fn track_row_style(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if selected {
            ACCENT_SOFT
        } else if hovered {
            SURFACE_HI
        } else {
            Color::TRANSPARENT
        };
        button::Style {
            background: Some(background.into()),
            text_color: TEXT,
            border: Border {
                color: if selected {
                    Color::from_rgba8(0xff, 0x66, 0xcc, 0.16)
                } else {
                    Color::TRANSPARENT
                },
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

fn queue_row_style(selected: bool, dragging: bool) -> impl Fn(&Theme) -> container::Style {
    move |_theme| {
        let background = if dragging {
            SURFACE_HI
        } else if selected {
            ACCENT_SOFT
        } else {
            Color::TRANSPARENT
        };
        let border = if dragging {
            ACCENT
        } else if selected {
            ACCENT_BORDER
        } else {
            Color::TRANSPARENT
        };
        container::Style::default()
            .background(background)
            .border(Border {
                color: border,
                width: if dragging || selected { 1.0 } else { 0.0 },
                radius: 8.0.into(),
            })
    }
}

fn queue_gap_style(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_theme| {
        let color = if active { ACCENT } else { Color::TRANSPARENT };
        container::Style::default()
            .background(color)
            .border(Border {
                color,
                width: 0.0,
                radius: 1.0.into(),
            })
    }
}

fn queue_gap_line<'a>(active: bool) -> Element<'a, Message> {
    container(iced::widget::Space::new().width(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fixed(2.0))
        .style(queue_gap_style(active))
        .into()
}

fn group_style(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some((if hovered { SURFACE_HI } else { SURFACE }).into()),
        text_color: TEXT,
        border: Border {
            color: if hovered { ACCENT_BORDER } else { BORDER },
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Flow's oscilloscope processing, reproduced from `src/javascript/effect.js`.
///
/// Flow reads 4096 byte time-domain samples from a Web Audio
/// `AnalyserNode`, converts each byte back to a roughly -1..1 centred
/// value, computes RMS, smooths it with `0.85/0.15`, derives a gain
/// target of `0.45 / smoothRms` clamped to `1.0..=2.6`, and applies
/// different attack/release interpolations (`0.25`/`0.05`). This state
/// keeps the same per-frame calculations and the resulting normalized
/// waveform consumed by the renderer.
#[derive(Debug, Clone)]
struct FlowScope {
    samples: [f32; WAVEFORM_WINDOW],
    smooth_rms: f32,
    gain: f32,
}

impl Default for FlowScope {
    fn default() -> Self {
        Self {
            samples: [0.0; WAVEFORM_WINDOW],
            smooth_rms: 0.0,
            gain: 1.0,
        }
    }
}

impl FlowScope {
    fn update(&mut self, input: &[f32; WAVEFORM_WINDOW]) {
        let mut sum = 0.0;
        for (target, sample) in self.samples.iter_mut().zip(input) {
            let centered = flow_centered(*sample);
            *target = centered;
            sum += centered * centered;
        }

        let rms = (sum / WAVEFORM_WINDOW as f32).sqrt();
        self.smooth_rms = self.smooth_rms * 0.85 + rms * 0.15;
        let target = if self.smooth_rms > 0.0001 {
            0.45 / self.smooth_rms
        } else {
            1.0
        }
        .clamp(1.0, 2.6);
        let rate = if target > self.gain { 0.25 } else { 0.05 };
        self.gain += (target - self.gain) * rate;

        for sample in &mut self.samples {
            *sample *= self.gain;
        }
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.smooth_rms = 0.0;
        self.gain = 1.0;
    }

    fn samples(&self) -> &[f32] {
        &self.samples
    }
}

/// Converts a float sample into Flow's Web Audio byte-domain value and
/// back to the centred float Flow draws. Web Audio returns 8-bit time
/// samples where 128 is silence.
fn flow_centered(sample: f32) -> f32 {
    let byte = (128.0 * (sample + 1.0)).clamp(0.0, 255.0) as u8;
    byte as f32 / 128.0 - 1.0
}

struct FlowOscilloscope<'a> {
    samples: &'a [f32],
}

impl<Message, Theme, R> Widget<Message, Theme, R> for FlowOscilloscope<'_>
where
    R: RendererTrait,
{
    fn size(&self) -> iced::Size<Length> {
        iced::Size::new(Length::Fill, Length::Fixed(OSC_HEIGHT))
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &R, limits: &layout::Limits) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fixed(OSC_HEIGHT))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut R,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if bounds.width <= 0.0 || bounds.height <= 0.0 || self.samples.is_empty() {
            return;
        }
        renderer.fill_quad(
            Quad {
                bounds,
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: OSC_RADIUS.into(),
                },
                shadow: Shadow {
                    color: Color::from_rgba8(0xff, 0x66, 0xcc, 0.20),
                    offset: iced::Vector::ZERO,
                    blur_radius: 18.0,
                },
                snap: false,
            },
            OSC_BACKGROUND,
        );

        let columns = bounds.width.ceil().max(1.0) as usize;
        let center = bounds.y + bounds.height / 2.0;
        let mut previous_y = center;

        for column in 0..columns {
            let t = if columns <= 1 {
                0.0
            } else {
                column as f32 / (columns - 1) as f32
            };
            let index = (t * (self.samples.len() - 1) as f32).round() as usize;
            let value = self.samples[index].clamp(-1.0, 1.0);
            let y = center + value * (bounds.height / 2.0);
            let top = previous_y.min(y);
            let bottom = previous_y.max(y);
            let height = (bottom - top + OSC_LINE_WIDTH).max(OSC_LINE_WIDTH);
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle {
                        x: bounds.x + column as f32,
                        y: top,
                        width: OSC_LINE_WIDTH,
                        height,
                    },
                    snap: false,
                    ..Quad::default()
                },
                OSC_COLOR,
            );
            previous_y = y;
        }

        // Flow closes the path at the right edge with a final line back
        // to the vertical center.
        let x = bounds.x + bounds.width - OSC_LINE_WIDTH;
        let top = previous_y.min(center);
        let bottom = previous_y.max(center);
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x,
                    y: top,
                    width: OSC_LINE_WIDTH,
                    height: (bottom - top + OSC_LINE_WIDTH).max(OSC_LINE_WIDTH),
                },
                snap: false,
                ..Quad::default()
            },
            OSC_COLOR,
        );
    }
}

impl<'a, Message> From<FlowOscilloscope<'a>> for Element<'a, Message> {
    fn from(widget: FlowOscilloscope<'a>) -> Self {
        Self::new(widget)
    }
}

fn queue_drag_events(
    event: iced::Event,
    _status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::QueueDragRelease)
        }
        iced::Event::Mouse(mouse::Event::CursorLeft) => Some(Message::QueueDragCancel),
        _ => None,
    }
}

fn audio_summary(status: &AudioStatus) -> (&'static str, String, Color) {
    match status {
        AudioStatus::Ready => (
            "Ready",
            "An output device was initialized successfully for this session.".to_owned(),
            SUCCESS,
        ),
        AudioStatus::BuildWithoutBackend => (
            "Not available in this build",
            "This binary was compiled without an audio-output backend. Browsing and \
             library management still work; build with `--features cpal` for playback."
                .to_owned(),
            MUTED,
        ),
        AudioStatus::DeviceUnavailable(reason) => {
            ("Output device unavailable", reason.clone(), DANGER)
        }
    }
}

fn progress_style(_: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: SURFACE_HI.into(),
        bar: ACCENT.into(),
        border: iced::border::rounded(2.0),
    }
}

// --------------------------------------------------------------- helpers ---

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn display_artist(track: &Track) -> String {
    nonempty(track.artist()).unwrap_or_else(|| "Unknown artist".to_owned())
}

fn display_album(track: &Track) -> String {
    nonempty(track.album()).unwrap_or_else(|| "Unknown album".to_owned())
}

fn display_folder(track: &Track) -> String {
    track
        .source()
        .path()
        .parent()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unknown folder".to_owned())
}

fn artwork_color(seed: &str) -> Color {
    const COLORS: [Color; 6] = [
        Color::from_rgb8(0xd8, 0x5f, 0xb4),
        Color::from_rgb8(0x7d, 0x74, 0xc9),
        Color::from_rgb8(0x4c, 0x9b, 0xb8),
        Color::from_rgb8(0xb8, 0x7a, 0x55),
        Color::from_rgb8(0x62, 0xa8, 0x83),
        Color::from_rgb8(0xb2, 0x6a, 0x9e),
    ];
    let hash = seed.bytes().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    COLORS[hash as usize % COLORS.len()]
}

fn artwork<'a>(track: Option<&Track>, size: f32) -> Element<'a, Message> {
    let (glyph, color) = match track {
        Some(track) => {
            let glyph = track
                .title()
                .chars()
                .find(|value| value.is_alphanumeric())
                .map(|value| value.to_uppercase().collect::<String>())
                .unwrap_or_else(|| "♪".to_owned());
            (glyph, artwork_color(track.title()))
        }
        None => ("♪".to_owned(), SURFACE_HI),
    };

    container(text(glyph).size(size * 0.34).color(TEXT))
        .width(size)
        .height(size)
        .center(size)
        .style(artwork_style(color))
        .into()
}

fn rule<'a>() -> Element<'a, Message> {
    container(iced::widget::Space::new().width(1).height(Length::Fill))
        .style(rule_style)
        .into()
}

fn horizontal_rule<'a>() -> Element<'a, Message> {
    container(iced::widget::Space::new().width(Length::Fill).height(1))
        .style(rule_style)
        .into()
}

fn pagination_footer(
    page: usize,
    pages: usize,
    start: usize,
    end: usize,
    total: usize,
) -> Element<'static, Message> {
    row![
        text(format!("{}-{} of {total}", start + 1, end))
            .size(10)
            .color(MUTED),
        iced::widget::Space::new().width(Length::Fill),
        button(text("Prev").size(11))
            .style(ghost_style)
            .padding([3, 8])
            .on_press_maybe((page > 0).then_some(Message::PagePrev)),
        text(format!("{} / {}", page + 1, pages))
            .size(10)
            .color(MUTED),
        button(text("Next").size(11))
            .style(ghost_style)
            .padding([3, 8])
            .on_press_maybe((page + 1 < pages).then_some(Message::PageNext)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn group_key(view: LibraryView, track: &Track) -> Option<String> {
    match view {
        LibraryView::Tracks => None,
        LibraryView::Artists => Some(display_artist(track)),
        LibraryView::Albums => Some(display_album(track)),
        LibraryView::Folders => Some(display_folder(track)),
    }
}

fn group_title(view: LibraryView, key: &str) -> String {
    if view == LibraryView::Folders {
        Path::new(key)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_owned())
    } else {
        key.to_owned()
    }
}

// ----------------------------------------------------------------- state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryView {
    Tracks,
    Artists,
    Albums,
    Folders,
}

impl LibraryView {
    fn label(self) -> &'static str {
        match self {
            Self::Tracks => "Tracks",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Folders => "Folders",
        }
    }

    fn empty_group_label(self) -> &'static str {
        match self {
            Self::Tracks | Self::Artists => "No artists yet",
            Self::Albums => "No albums yet",
            Self::Folders => "No folders yet",
        }
    }
}

#[derive(Debug, Clone)]
struct QueueDrag {
    entry: QueueEntryId,
    target_gap: Option<usize>,
    moved: bool,
}

struct FluxReady {
    controller: Controller,
    dir_input: String,
    search: String,
    lib_input: String,
    migrate_source: String,
    pending_plan: Option<MigrationPlan>,
    migrated_dest: Option<PathBuf>,
    lifecycle_msg: Option<String>,
    view: LibraryView,
    selected_group: Option<String>,
    page: usize,
    queue_page: usize,
    queue_drag: Option<QueueDrag>,
    last_queue_current: Option<QueueEntryId>,
    scope: FlowScope,
    show_setup: bool,
    queue_visible: bool,
    window_width: f32,
}

#[derive(Debug, Clone)]
enum Message {
    DirInputChanged(String),
    LibInputChanged(String),
    OpenLibraryPressed,
    CreateLibraryPressed,
    UseLegacyIndexPressed,
    AcknowledgeRecoveryPressed,
    ReplaceLegacyIndexPressed,
    MigrateSourceChanged(String),
    PlanMigrationPressed,
    ExecuteMigrationPressed,
    CancelMigrationPressed,
    OpenMigratedPressed,
    ReconcilePressed,
    ScanPressed,
    RefreshPressed,
    SearchChanged(String),
    SelectView(LibraryView),
    SelectGroup(Option<String>),
    ToggleSetup,
    ToggleQueue,
    PagePrev,
    PageNext,
    QueuePagePrev,
    QueuePageNext,
    PlayTrack(TrackId),
    EnqueueTrack(TrackId),
    PlayNextTrack(TrackId),
    RemoveQueueEntry(QueueEntryId),
    QueueDragStart(QueueEntryId),
    QueueDragHover(usize),
    QueueDragLeave,
    QueueDragCancel,
    QueueDragRelease,
    TogglePlayPause,
    Stop,
    Next,
    Previous,
    CycleRepeat,
    VolumeChanged(f32),
    KeyPressed(Key, Modifiers),
    Tick,
    WaveTick,
    WindowResized(f32),
    WindowCloseRequested(window::Id),
}

impl FluxReady {
    fn new(controller: Controller) -> Self {
        let show_setup = controller.legacy_index_write_protected();
        let current = controller.current_queue_entry_id();
        let queue_page = current
            .and_then(|entry| controller.queue_entry_index(entry))
            .map_or(0, |index| index / QUEUE_PAGE_SIZE);
        Self {
            controller,
            dir_input: String::new(),
            search: String::new(),
            lib_input: String::new(),
            migrate_source: String::new(),
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
            view: LibraryView::Tracks,
            selected_group: None,
            page: 0,
            queue_page,
            queue_drag: None,
            last_queue_current: current,
            scope: FlowScope::default(),
            show_setup,
            queue_visible: true,
            window_width: 1_180.0,
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            iced::time::every(Duration::from_millis(150)).map(|_| Message::Tick),
            window::close_requests().map(Message::WindowCloseRequested),
            window::resize_events().map(|(_, size)| Message::WindowResized(size.width)),
            iced::keyboard::listen().filter_map(|event| match event {
                iced::keyboard::Event::KeyPressed { key, modifiers, .. } => {
                    Some(Message::KeyPressed(key, modifiers))
                }
                _ => None,
            }),
            iced::event::listen_with(queue_drag_events),
        ];
        if self.controller.player_state() == PlaybackState::Playing
            && self.controller.audio_status().is_ready()
        {
            subscriptions.push(iced::time::every(WAVE_TICK).map(|_| Message::WaveTick));
        }
        Subscription::batch(subscriptions)
    }

    fn after_switch(&mut self) {
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.lifecycle_msg = None;
        self.queue_drag = None;
        let current = self.controller.current_queue_entry_id();
        self.queue_page = current
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .map_or(0, |index| index / QUEUE_PAGE_SIZE);
        self.last_queue_current = current;
        self.scope.reset();
        self.select_view(LibraryView::Tracks);
    }

    fn follow_queue_current(&mut self) {
        let current = self.controller.current_queue_entry_id();
        if current != self.last_queue_current {
            self.last_queue_current = current;
            self.focus_queue_current();
        }
    }

    fn focus_queue_current(&mut self) {
        if let Some(entry) = self.controller.current_queue_entry_id() {
            self.focus_queue_entry(entry);
        }
    }

    fn focus_queue_entry(&mut self, entry: QueueEntryId) {
        if let Some(index) = self.controller.queue_entry_index(entry) {
            self.queue_page = index / QUEUE_PAGE_SIZE;
        }
    }

    fn cancel_queue_drag(&mut self) {
        self.queue_drag = None;
    }

    fn finish_queue_drag(&mut self) {
        let Some(drag) = self.queue_drag.take() else {
            return;
        };
        if self.controller.queue_entry_index(drag.entry).is_none() {
            return;
        }
        if !drag.moved {
            let _ = self.controller.play_queue_entry(drag.entry);
            self.follow_queue_current();
            return;
        }
        let Some(gap) = drag.target_gap else {
            return;
        };
        if self
            .controller
            .move_queue_entry_to_gap(drag.entry, gap)
            .unwrap_or(false)
        {
            self.focus_queue_entry(drag.entry);
        }
    }

    fn move_current_queue_entry(&mut self, delta: isize) {
        let Some(entry) = self.controller.current_queue_entry_id() else {
            return;
        };
        if self
            .controller
            .move_queue_entry_by(entry, delta)
            .unwrap_or(false)
        {
            self.focus_queue_entry(entry);
        }
    }

    fn select_view(&mut self, view: LibraryView) {
        self.view = view;
        self.selected_group = None;
        self.search.clear();
        self.page = 0;
        self.show_setup = false;
    }

    fn toggle_setup(&mut self) {
        self.show_setup = !self.show_setup;
        self.page = 0;
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::DirInputChanged(value) => self.dir_input = value,
            Message::LibInputChanged(value) => self.lib_input = value,
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
            Message::ReplaceLegacyIndexPressed => {
                if let Err(err) = self.controller.replace_unreadable_legacy_index() {
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
            Message::SearchChanged(value) => {
                self.search = value;
                self.page = 0;
                if !self.search.trim().is_empty()
                    && self.view != LibraryView::Tracks
                    && self.selected_group.is_none()
                {
                    self.view = LibraryView::Tracks;
                }
            }
            Message::SelectView(view) => self.select_view(view),
            Message::SelectGroup(group) => {
                if group.is_none() {
                    self.search.clear();
                }
                self.selected_group = group;
                self.page = 0;
                self.show_setup = false;
            }
            Message::ToggleSetup => self.toggle_setup(),
            Message::ToggleQueue => {
                self.cancel_queue_drag();
                self.queue_visible = !self.queue_visible;
            }
            Message::PagePrev => self.page = self.page.saturating_sub(1),
            Message::PageNext => self.page = self.page.saturating_add(1),
            Message::QueuePagePrev => {
                self.cancel_queue_drag();
                self.queue_page = self.queue_page.saturating_sub(1);
            }
            Message::QueuePageNext => {
                self.cancel_queue_drag();
                self.queue_page = self.queue_page.saturating_add(1);
            }
            Message::PlayTrack(id) => {
                self.controller.play_track(id);
                self.follow_queue_current();
            }
            Message::EnqueueTrack(id) => {
                let _ = self.controller.enqueue_track(id);
                self.follow_queue_current();
            }
            Message::PlayNextTrack(id) => {
                let _ = self.controller.play_next_track(id);
                self.follow_queue_current();
            }
            Message::RemoveQueueEntry(entry) => {
                if self.queue_drag.is_some() {
                    self.cancel_queue_drag();
                } else {
                    let _ = self.controller.remove_queue_entry(entry);
                    self.follow_queue_current();
                    self.focus_queue_current();
                }
            }
            Message::QueueDragStart(entry) => {
                if self.controller.queue_entry_index(entry).is_some() {
                    self.queue_drag = Some(QueueDrag {
                        entry,
                        target_gap: None,
                        moved: false,
                    });
                }
            }
            Message::QueueDragHover(gap) => {
                if let Some(drag) = &mut self.queue_drag {
                    drag.target_gap = (gap <= self.controller.queue_len()).then_some(gap);
                    drag.moved = true;
                }
            }
            Message::QueueDragLeave => {
                if let Some(drag) = &mut self.queue_drag {
                    drag.target_gap = None;
                    drag.moved = true;
                }
            }
            Message::QueueDragCancel => self.cancel_queue_drag(),
            Message::QueueDragRelease => self.finish_queue_drag(),
            Message::TogglePlayPause => self.controller.toggle_play_pause(),
            Message::Stop => self.controller.stop(),
            Message::Next => {
                self.controller.next();
                self.follow_queue_current();
            }
            Message::Previous => {
                self.controller.previous();
                self.follow_queue_current();
            }
            Message::CycleRepeat => self.controller.cycle_repeat_mode(),
            Message::VolumeChanged(volume) => self.controller.set_volume(volume),
            Message::KeyPressed(key, modifiers) => {
                if modifiers.contains(Modifiers::CTRL) {
                    match key.as_ref() {
                        Key::Named(Named::ArrowUp) => {
                            self.cancel_queue_drag();
                            self.move_current_queue_entry(-1);
                        }
                        Key::Named(Named::ArrowDown) => {
                            self.cancel_queue_drag();
                            self.move_current_queue_entry(1);
                        }
                        _ => {}
                    }
                    return Task::none();
                }
                match key.as_ref() {
                    Key::Named(Named::Space) => self.controller.toggle_play_pause(),
                    Key::Character("n") => self.controller.next(),
                    Key::Character("p") => self.controller.previous(),
                    Key::Character("s") => self.controller.stop(),
                    Key::Character("/") => {
                        return iced::widget::operation::focus(iced::widget::Id::new(
                            "flux-search",
                        ));
                    }
                    Key::Character("1") => self.select_view(LibraryView::Tracks),
                    Key::Character("2") => self.select_view(LibraryView::Artists),
                    Key::Character("3") => self.select_view(LibraryView::Albums),
                    Key::Character("4") => self.select_view(LibraryView::Folders),
                    Key::Character("l") => self.toggle_setup(),
                    Key::Named(Named::Escape) => {
                        self.cancel_queue_drag();
                        self.search.clear();
                        self.selected_group = None;
                        self.show_setup = false;
                        self.page = 0;
                    }
                    _ => {}
                }
            }
            Message::Tick => {
                self.controller.poll();
                self.follow_queue_current();
                if let Some(entry) = self.queue_drag.as_ref().map(|drag| drag.entry)
                    && self.controller.queue_entry_index(entry).is_none()
                {
                    self.cancel_queue_drag();
                }
                if self.controller.player_state() != PlaybackState::Playing
                    || !self.controller.audio_status().is_ready()
                {
                    self.scope.reset();
                }
            }
            Message::WaveTick => {
                if self.controller.player_state() == PlaybackState::Playing
                    && self.controller.audio_status().is_ready()
                {
                    let waveform = self.controller.waveform();
                    self.scope.update(&waveform);
                }
            }
            Message::WindowResized(width) => {
                self.cancel_queue_drag();
                self.window_width = width;
            }
            Message::WindowCloseRequested(id) => {
                let _ = self.controller.save_index();
                return window::close(id);
            }
        }
        Task::none()
    }

    // -------------------------------------------------------------- view ---

    fn view(&self) -> Element<'_, Message> {
        let queue_visible = self.queue_visible && self.window_width >= 1_080.0;
        let mut body = row![self.sidebar(), rule(), self.central()]
            .height(Length::Fill)
            .width(Length::Fill);
        if queue_visible {
            body = body.push(rule()).push(self.queue_panel());
        }

        container(column![body, horizontal_rule(), self.player_bar()].height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(app_style)
            .into()
    }

    fn nav_counts(&self) -> (usize, usize, usize, usize) {
        let mut artists: HashSet<Option<&str>> = HashSet::new();
        let mut albums: HashSet<Option<&str>> = HashSet::new();
        let mut folders: HashSet<Option<&Path>> = HashSet::new();
        for (_, track) in self.controller.tracks() {
            let artist = track
                .artist()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let album = track
                .album()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            artists.insert(artist);
            albums.insert(album);
            folders.insert(track.source().path().parent());
        }
        (
            self.controller.tracks().len(),
            artists.len(),
            albums.len(),
            folders.len(),
        )
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let counts = self.nav_counts();
        let brand = row![
            container(iced::widget::Space::new().width(4).height(28))
                .style(|_| container::Style::default().background(ACCENT)),
            column![
                text("Flux").size(22).color(TEXT),
                text("LOCAL MUSIC").size(9).color(MUTED),
            ]
            .spacing(1),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let mut nav = column![
            brand,
            iced::widget::Space::new().height(20),
            text("LIBRARY").size(9).color(MUTED),
        ]
        .spacing(6);

        for view in [
            LibraryView::Tracks,
            LibraryView::Artists,
            LibraryView::Albums,
            LibraryView::Folders,
        ] {
            nav = nav.push(
                button(
                    row![
                        text(view.label()).size(13),
                        iced::widget::Space::new().width(Length::Fill),
                        text(
                            match view {
                                LibraryView::Tracks => counts.0,
                                LibraryView::Artists => counts.1,
                                LibraryView::Albums => counts.2,
                                LibraryView::Folders => counts.3,
                            }
                            .to_string()
                        )
                        .size(9)
                        .color(MUTED),
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([8, 10])
                .style(nav_style(self.view == view && !self.show_setup))
                .on_press(Message::SelectView(view)),
            );
        }

        nav = nav.push(iced::widget::Space::new().height(12));
        nav = nav.push(
            button(text("Library setup").size(12))
                .width(Length::Fill)
                .padding([8, 10])
                .style(nav_style(self.show_setup))
                .on_press(Message::ToggleSetup),
        );
        nav = nav.push(iced::widget::Space::new().height(Length::Fill));
        nav = nav.push(text("SPACE PLAY · N/P SKIP").size(8).color(MUTED));
        nav = nav.push(text("1-4 VIEWS · L SETUP · / SEARCH").size(8).color(MUTED));
        nav = nav.push(iced::widget::Space::new().height(10));

        let library_state = match self.controller.storage_mode() {
            StorageMode::Legacy => "App index",
            StorageMode::Portable => "Portable library",
        };
        nav = nav.push(text("LIBRARY SOURCE").size(8).color(MUTED));
        nav = nav.push(text(library_state).size(10).color(TEXT));

        container(nav)
            .width(Length::Fixed(190.0))
            .height(Length::Fill)
            .padding(14)
            .style(sidebar_style)
            .into()
    }

    fn central(&self) -> Element<'_, Message> {
        let mut content = column![self.top_bar()].spacing(14).padding(20);
        if let Some(banner) = self.error_banner() {
            content = content.push(banner);
        }
        content = content.push(
            container(if self.show_setup {
                self.setup_view()
            } else {
                self.library_view()
            })
            .height(Length::Fill)
            .width(Length::Fill),
        );
        if let Some(status) = self.controller.last_status()
            && self.controller.last_error().is_none()
        {
            content = content.push(text(status).size(10).color(MUTED));
        }

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let (title, subtitle) = if self.show_setup {
            (
                "Library setup".to_owned(),
                "Choose what Flux scans and where it keeps the catalogue.".to_owned(),
            )
        } else if let Some(group) = &self.selected_group {
            let rows = self.filtered_tracks();
            (
                group_title(self.view, group),
                format!(
                    "{} track(s) · {}",
                    rows.len(),
                    self.view.label().to_lowercase()
                ),
            )
        } else if self.view == LibraryView::Tracks {
            (
                "All tracks".to_owned(),
                format!(
                    "{} indexed track(s){}",
                    self.controller.tracks().len(),
                    if self.search.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" · {} match(es)", self.filtered_tracks().len())
                    }
                ),
            )
        } else {
            (
                self.view.label().to_owned(),
                format!(
                    "{} {}",
                    self.groups_for(self.view).len(),
                    self.view.label().to_lowercase()
                ),
            )
        };

        let mut bar = row![].spacing(10).align_y(Alignment::Center);
        if self.selected_group.is_some() && !self.show_setup {
            bar = bar.push(
                button(text("‹ Back").size(11))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::SelectGroup(None)),
            );
        }
        bar = bar.push(
            column![
                text(title).size(24).color(TEXT),
                text(subtitle).size(11).color(MUTED)
            ]
            .spacing(2),
        );
        bar = bar.push(iced::widget::Space::new().width(Length::Fill));

        if !self.show_setup {
            bar = bar.push(
                text_input("Search title, artist, album…", &self.search)
                    .id(iced::widget::Id::new("flux-search"))
                    .on_input(Message::SearchChanged)
                    .style(input_style)
                    .width(Length::Fixed(250.0)),
            );
        }
        if self.window_width >= 1_080.0 {
            bar = bar.push(
                button(text("Queue").size(11))
                    .style(nav_style(self.queue_visible))
                    .padding([6, 10])
                    .on_press(Message::ToggleQueue),
            );
        }
        bar = bar.push(
            button(text(if self.show_setup { "Close" } else { "Setup" }).size(11))
                .style(nav_style(self.show_setup))
                .padding([6, 10])
                .on_press(Message::ToggleSetup),
        );

        bar.into()
    }

    fn error_banner(&self) -> Option<Element<'_, Message>> {
        let error = self.controller.last_error()?;
        let mut line = row![
            text("!").size(14).color(DANGER),
            text(error).size(11).color(TEXT).width(Length::Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        if self.controller.legacy_index_write_protected() {
            line = line.push(
                button(text("Preserve & replace index").size(10))
                    .style(danger_button_style)
                    .padding([4, 8])
                    .on_press(Message::ReplaceLegacyIndexPressed),
            );
        }
        if self.controller.portable_recovery().is_some() {
            line = line.push(
                button(text("Acknowledge recovery").size(10))
                    .style(danger_button_style)
                    .padding([4, 8])
                    .on_press(Message::AcknowledgeRecoveryPressed),
            );
        }

        Some(
            container(line)
                .padding([10, 12])
                .width(Length::Fill)
                .style(error_banner_style)
                .into(),
        )
    }

    fn library_view(&self) -> Element<'_, Message> {
        if self.controller.tracks().is_empty() {
            return self.empty_library();
        }

        if self.view != LibraryView::Tracks && self.selected_group.is_none() {
            self.group_list()
        } else {
            self.track_list(self.filtered_tracks())
        }
    }

    fn empty_library(&self) -> Element<'_, Message> {
        let card = column![
            text("♪").size(46).color(ACCENT),
            text("Your library is empty").size(22).color(TEXT),
            text(
                "Scan a folder of local music to begin. Flux reads embedded tags \
                 and never writes to your audio files."
            )
            .size(12)
            .color(MUTED)
            .width(Length::Fixed(420.0)),
            button(text("Scan your music").size(12))
                .style(accent_button_style)
                .padding([8, 14])
                .on_press(Message::ToggleSetup),
        ]
        .spacing(12)
        .align_x(Alignment::Center);

        container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into()
    }

    fn groups_for(&self, view: LibraryView) -> Vec<(String, usize)> {
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        for (_, track) in self.controller.tracks() {
            if let Some(key) = group_key(view, track) {
                *groups.entry(key).or_default() += 1;
            }
        }
        groups.into_iter().collect()
    }

    fn group_list(&self) -> Element<'_, Message> {
        let groups = self.groups_for(self.view);
        if groups.is_empty() {
            return container(text(self.view.empty_group_label()).size(14).color(MUTED))
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .into();
        }

        let total = groups.len();
        let pages = total.div_ceil(GROUP_PAGE_SIZE).max(1);
        let page = self.page.min(pages - 1);
        let start = page * GROUP_PAGE_SIZE;
        let end = (start + GROUP_PAGE_SIZE).min(total);

        let mut list = column![].spacing(7);
        for (key, count) in &groups[start..end] {
            let title = group_title(self.view, key);
            let subtitle = if self.view == LibraryView::Folders {
                format!("{count} track(s) · {key}")
            } else {
                format!("{count} track(s)")
            };
            let initial = title
                .chars()
                .find(|value| value.is_alphanumeric())
                .map(|value| value.to_uppercase().collect::<String>())
                .unwrap_or_else(|| "•".to_owned());

            let row = row![
                container(text(initial).size(15).color(TEXT))
                    .width(38)
                    .height(38)
                    .center(38)
                    .style(artwork_style(artwork_color(key))),
                column![
                    text(title.clone()).size(14).color(TEXT),
                    text(subtitle).size(10).color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                text("›").size(20).color(MUTED),
            ]
            .spacing(12)
            .align_y(Alignment::Center);

            list = list.push(
                button(row)
                    .width(Length::Fill)
                    .padding(9)
                    .style(group_style)
                    .on_press(Message::SelectGroup(Some(key.clone()))),
            );
        }

        column![
            scrollable(list).height(Length::Fill),
            pagination_footer(page, pages, start, end, total),
        ]
        .spacing(8)
        .height(Length::Fill)
        .into()
    }

    fn filtered_tracks(&self) -> Vec<(TrackId, &Track)> {
        let mut rows = self.controller.search(&self.search);
        if let Some(group) = &self.selected_group {
            rows.retain(|(_, track)| {
                group_key(self.view, track).as_deref() == Some(group.as_str())
            });
        }
        rows
    }

    fn track_list<'a>(&self, rows: Vec<(TrackId, &'a Track)>) -> Element<'a, Message> {
        let total = rows.len();
        if total == 0 {
            let message = if self.search.trim().is_empty() {
                "No tracks in this collection."
            } else {
                "No tracks match the current search."
            };
            return container(
                column![
                    text(message).size(15).color(TEXT),
                    text("Clear the search or choose another view.")
                        .size(11)
                        .color(MUTED),
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into();
        }

        let pages = total.div_ceil(TRACK_PAGE_SIZE).max(1);
        let page = self.page.min(pages - 1);
        let start = page * TRACK_PAGE_SIZE;
        let end = (start + TRACK_PAGE_SIZE).min(total);
        let current = self.controller.current_id();

        let header = row![
            text("#").size(9).color(MUTED).width(Length::Fixed(28.0)),
            text("TITLE")
                .size(9)
                .color(MUTED)
                .width(Length::FillPortion(5)),
            text("ALBUM")
                .size(9)
                .color(MUTED)
                .width(Length::FillPortion(4)),
            text("TIME").size(9).color(MUTED).width(Length::Fixed(48.0)),
            text("+").size(9).color(MUTED).width(Length::Fixed(26.0)),
            text("NEXT").size(9).color(MUTED).width(Length::Fixed(34.0)),
        ]
        .spacing(8)
        .padding([2, 10]);

        let mut list = column![].spacing(1);
        for (offset, (id, track)) in rows[start..end].iter().enumerate() {
            list = list.push(track_row(start + offset + 1, *id, track, current));
        }

        column![
            header,
            horizontal_rule(),
            scrollable(list).height(Length::Fill),
            pagination_footer(page, pages, start, end, total),
        ]
        .spacing(8)
        .height(Length::Fill)
        .into()
    }

    fn setup_view(&self) -> Element<'_, Message> {
        let scanning = self.controller.is_scanning();
        let protected = self.controller.legacy_index_write_protected();
        let scan_ready = !scanning && !protected && !self.dir_input.trim().is_empty();
        let lib_ready = !scanning && !self.lib_input.trim().is_empty();

        let mut setup = column![].spacing(12);

        let (audio_title, audio_detail, audio_color) =
            audio_summary(self.controller.audio_status());
        setup = setup.push(setup_card(
            "AUDIO OUTPUT",
            "The actual result of initializing this session's output backend. This is \
             information about the build or device, not a library error.",
            column![
                text(audio_title).size(12).color(audio_color),
                text(audio_detail).size(10).color(MUTED),
            ]
            .spacing(4),
        ));

        setup = setup.push(setup_card(
            "SCAN LOCAL MUSIC",
            "Add a folder of local audio to the active library. Embedded tags are read; \
             audio files are never modified.",
            column![
                row![
                    text_input("Music folder path…", &self.dir_input)
                        .on_input(Message::DirInputChanged)
                        .on_submit(Message::ScanPressed)
                        .style(input_style)
                        .width(Length::Fill),
                    button(text("Scan folder").size(11))
                        .style(accent_button_style)
                        .padding([7, 12])
                        .on_press_maybe(scan_ready.then_some(Message::ScanPressed)),
                    button(text("Refresh").size(11))
                        .style(ghost_style)
                        .padding([7, 12])
                        .on_press_maybe(
                            (!scanning && !protected).then_some(Message::RefreshPressed)
                        ),
                ]
                .spacing(8),
                text(if protected {
                    "Scanning is disabled until the unreadable app index is preserved and replaced."
                } else if scanning {
                    "Scanning… the library updates when the worker finishes."
                } else {
                    "Tags and durations come from the files; filenames are the fallback."
                })
                .size(10)
                .color(if protected { DANGER } else { MUTED }),
            ]
            .spacing(8),
        ));

        let library_label = match self.controller.storage_mode() {
            StorageMode::Legacy => match self.controller.legacy_index_path() {
                Some(path) => format!("Active: app index — {}", path.display()),
                None => "Active: session-only legacy library".to_owned(),
            },
            StorageMode::Portable => match self.controller.portable_root() {
                Some(root) => format!("Active: portable library — {}", root.display()),
                None => "Active: portable library".to_owned(),
            },
        };

        let mut portable_body = column![text(library_label).size(10).color(MUTED)].spacing(8);
        portable_body = portable_body.push(
            row![
                text_input("Portable library folder…", &self.lib_input)
                    .on_input(Message::LibInputChanged)
                    .on_submit(Message::OpenLibraryPressed)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Open").size(11))
                    .style(ghost_style)
                    .padding([7, 12])
                    .on_press_maybe(lib_ready.then_some(Message::OpenLibraryPressed)),
                button(text("Create").size(11))
                    .style(accent_button_style)
                    .padding([7, 12])
                    .on_press_maybe(lib_ready.then_some(Message::CreateLibraryPressed)),
            ]
            .spacing(8),
        );

        if self.controller.storage_mode() == StorageMode::Portable {
            portable_body = portable_body.push(
                row![
                    button(text("Use app index").size(11))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press_maybe((!scanning).then_some(Message::UseLegacyIndexPressed)),
                    button(text("Reconcile").size(11))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press_maybe((!scanning).then_some(Message::ReconcilePressed)),
                ]
                .spacing(8),
            );
        }
        if self.controller.portable_recovery().is_some() {
            portable_body = portable_body.push(
                button(text("Acknowledge recovered library").size(11))
                    .style(danger_button_style)
                    .padding([6, 10])
                    .on_press_maybe((!scanning).then_some(Message::AcknowledgeRecoveryPressed)),
            );
        }

        setup = setup.push(setup_card(
            "PORTABLE LIBRARY",
            "A portable library keeps its catalogue in a .flux folder beside your music, \
             so the whole library can move with the folder.",
            portable_body,
        ));

        let mut migration_body = column![
            row![
                text_input("Legacy index file to migrate…", &self.migrate_source)
                    .on_input(Message::MigrateSourceChanged)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Plan migration").size(11))
                    .style(ghost_style)
                    .padding([7, 12])
                    .on_press_maybe(
                        (lib_ready && !self.migrate_source.trim().is_empty())
                            .then_some(Message::PlanMigrationPressed)
                    ),
            ]
            .spacing(8),
            text(
                "Planning is read-only. Nothing is written or activated until you \
                     review the plan and execute it."
            )
            .size(10)
            .color(MUTED),
        ]
        .spacing(8);

        if self.pending_plan.is_some() {
            migration_body = migration_body.push(
                row![
                    button(text("Execute migration").size(11))
                        .style(accent_button_style)
                        .padding([7, 12])
                        .on_press(Message::ExecuteMigrationPressed),
                    button(text("Cancel").size(11))
                        .style(ghost_style)
                        .padding([7, 12])
                        .on_press(Message::CancelMigrationPressed),
                ]
                .spacing(8),
            );
        }
        if self.migrated_dest.is_some() {
            migration_body = migration_body.push(
                button(text("Open migrated library").size(11))
                    .style(ghost_style)
                    .padding([7, 12])
                    .on_press(Message::OpenMigratedPressed),
            );
        }
        setup = setup.push(setup_card(
            "MIGRATE A LEGACY INDEX",
            "Carry an existing app-data index into a portable library, preserving IDs and \
             metadata while reporting excluded paths.",
            migration_body,
        ));

        if let Some(message) = &self.lifecycle_msg {
            setup = setup.push(
                container(text(message).size(11).color(MUTED))
                    .padding(12)
                    .width(Length::Fill)
                    .style(card_style),
            );
        }

        scrollable(setup).height(Length::Fill).into()
    }

    fn queue_panel(&self) -> Element<'_, Message> {
        let current_entry = self.controller.current_queue_entry();
        let current = current_entry.and_then(|entry| self.controller.track(entry.track()));
        let (title, subtitle) = match (current_entry, current) {
            (_, Some(track)) => (
                track.title().to_owned(),
                format!("{} · {}", display_artist(track), display_album(track)),
            ),
            (Some(_), None) => (
                "Missing from library".to_owned(),
                "This queue entry's track is no longer in the library.".to_owned(),
            ),
            (None, _) => (
                "Nothing playing".to_owned(),
                "The queue is empty.".to_owned(),
            ),
        };

        let mut panel = column![
            text("NOW PLAYING").size(9).color(ACCENT),
            container(artwork(current, 176.0))
                .width(Length::Fill)
                .center_x(Length::Fill),
            text(title).size(16).color(TEXT),
            text(subtitle).size(11).color(MUTED),
            horizontal_rule(),
        ]
        .spacing(9);

        let entries = self.controller.queue_entries();
        let total = entries.len();
        let pages = total.div_ceil(QUEUE_PAGE_SIZE).max(1);
        let page = self.queue_page.min(pages - 1);
        let start = page * QUEUE_PAGE_SIZE;
        let end = (start + QUEUE_PAGE_SIZE).min(total);
        let current_entry_id = self.controller.current_queue_entry_id();
        let dragged = self.queue_drag.as_ref().map(|drag| drag.entry);
        let target_gap = self.queue_drag.as_ref().and_then(|drag| drag.target_gap);

        panel = panel.push(
            row![
                text("QUEUE").size(9).color(MUTED),
                iced::widget::Space::new().width(Length::Fill),
                text(format!("{total}")).size(9).color(MUTED),
            ]
            .align_y(Alignment::Center),
        );
        panel = panel.push(
            text("DRAG TO REORDER · CTRL+↑/↓ CURRENT")
                .size(8)
                .color(MUTED),
        );

        if total == 0 {
            panel = panel.push(
                column![
                    text("The queue is empty.").size(11).color(TEXT),
                    text("Play a library track, or use + to add one.")
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(3),
            );
        } else {
            let mut rows = column![].spacing(0);
            for (offset, entry) in entries[start..end].iter().enumerate() {
                let index = start + offset;
                rows = rows.push(queue_gap_line(target_gap == Some(index)));
                rows = rows.push(queue_row(
                    *entry,
                    index,
                    current_entry_id,
                    dragged == Some(entry.id()),
                    self.controller.track(entry.track()),
                ));
            }
            rows = rows.push(queue_gap_line(target_gap == Some(end)));
            panel = panel.push(
                mouse_area(scrollable(rows).height(Length::Fill)).on_exit(Message::QueueDragLeave),
            );
            panel = panel.push(
                row![
                    text(format!("{}-{} of {total}", start + 1, end))
                        .size(9)
                        .color(MUTED),
                    iced::widget::Space::new().width(Length::Fill),
                    button(text("Prev").size(10))
                        .style(ghost_style)
                        .padding([2, 6])
                        .on_press_maybe((page > 0).then_some(Message::QueuePagePrev)),
                    text(format!("{} / {}", page + 1, pages))
                        .size(9)
                        .color(MUTED),
                    button(text("Next").size(10))
                        .style(ghost_style)
                        .padding([2, 6])
                        .on_press_maybe((page + 1 < pages).then_some(Message::QueuePageNext)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }

        container(panel)
            .width(Length::Fixed(268.0))
            .height(Length::Fill)
            .padding(16)
            .style(sidebar_style)
            .into()
    }

    fn player_bar(&self) -> Element<'_, Message> {
        let current = self.controller.current_track();
        let current_entry = self.controller.current_queue_entry();
        let state = self.controller.player_state();
        let (title, subtitle) = match (current_entry, current) {
            (_, Some(track)) => (
                track.title().to_owned(),
                format!("{} · {}", display_artist(track), display_album(track)),
            ),
            (Some(_), None) => (
                "Missing queue entry".to_owned(),
                "This track is no longer in the library.".to_owned(),
            ),
            (None, _) => (
                "Nothing playing".to_owned(),
                "Select a track to begin".to_owned(),
            ),
        };
        let total = current
            .and_then(|track| {
                track
                    .duration()
                    .or_else(|| self.controller.stream_info().and_then(|info| info.duration))
            })
            .unwrap_or(Duration::ZERO);
        let position = self.controller.position();
        let progress = if total.is_zero() {
            0.0
        } else {
            (position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
        };

        let transport = row![
            button(text("Prev").size(10))
                .style(ghost_style)
                .padding([7, 8])
                .on_press(Message::Previous),
            button(
                text(if state == PlaybackState::Playing {
                    "Pause"
                } else {
                    "Play"
                })
                .size(11)
            )
            .style(accent_button_style)
            .padding([7, 13])
            .on_press(Message::TogglePlayPause),
            button(text("Stop").size(10))
                .style(ghost_style)
                .padding([7, 8])
                .on_press(Message::Stop),
            button(text("Next").size(10))
                .style(ghost_style)
                .padding([7, 8])
                .on_press(Message::Next),
            button(text(repeat_label(self.controller.repeat_mode())).size(10))
                .style(nav_style(self.controller.repeat_mode() != RepeatMode::Off))
                .padding([6, 8])
                .on_press(Message::CycleRepeat),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let volume = row![
            text("VOL").size(9).color(MUTED),
            slider(0.0..=1.0, self.controller.volume(), Message::VolumeChanged)
                .step(0.01_f32)
                .width(105),
            text(format!(
                "{:>3}%",
                (self.controller.volume() * 100.0).round() as u32
            ))
            .size(9)
            .color(MUTED),
        ]
        .spacing(7)
        .align_y(Alignment::Center);

        let mut info = column![
            text(title)
                .size(15)
                .color(if current.is_some() { TEXT } else { MUTED }),
            text(subtitle).size(10).color(MUTED),
        ]
        .spacing(2)
        .width(Length::Fill);
        if !self.controller.audio_status().is_ready() {
            let (_, _, color) = audio_summary(self.controller.audio_status());
            info = info.push(
                text("Audio unavailable · see Library setup")
                    .size(9)
                    .color(color),
            );
        }

        let now = row![
            artwork(current, 54.0),
            info,
            transport,
            iced::widget::Space::new().width(16),
            volume,
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        let progress_row = row![
            text(format_duration(position)).size(9).color(MUTED),
            progress_bar(0.0..=1.0, progress)
                .girth(4)
                .style(progress_style)
                .length(Length::Fill),
            text(if total.is_zero() {
                "--:--".to_owned()
            } else {
                format_duration(total)
            })
            .size(9)
            .color(MUTED),
        ]
        .spacing(9)
        .align_y(Alignment::Center);

        container(
            column![now, progress_row, self.oscilloscope()]
                .spacing(6)
                .padding([8, 16]),
        )
        .width(Length::Fill)
        .style(player_style)
        .into()
    }

    /// Flow's real-time oscilloscope, fed by post-gain output samples
    /// captured through the engine's lock-free waveform tap.
    fn oscilloscope(&self) -> Element<'_, Message> {
        FlowOscilloscope {
            samples: self.scope.samples(),
        }
        .into()
    }
}

/// Application shell. A failed lock acquisition does not fall back to an
/// empty or writable library: it renders an explicit blocking screen and
/// leaves the catalogue untouched.
struct Flux {
    state: FluxState,
}

enum FluxState {
    Ready(Box<FluxReady>),
    Blocked(String),
}

impl Flux {
    fn boot() -> Self {
        match Controller::open_default() {
            Ok(controller) => Self {
                state: FluxState::Ready(Box::new(FluxReady::new(controller))),
            },
            Err(error) => Self {
                state: FluxState::Blocked(error.to_string()),
            },
        }
    }

    fn theme(&self) -> Theme {
        flux_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.state {
            FluxState::Ready(app) => app.subscription(),
            FluxState::Blocked(_) => window::close_requests().map(Message::WindowCloseRequested),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match &mut self.state {
            FluxState::Ready(app) => app.update(message),
            FluxState::Blocked(_) => match message {
                Message::WindowCloseRequested(id) => window::close(id),
                _ => Task::none(),
            },
        }
    }

    fn view(&self) -> Element<'_, Message> {
        match &self.state {
            FluxState::Ready(app) => app.view(),
            FluxState::Blocked(message) => blocked_library_view(message),
        }
    }
}

fn blocked_library_view(message: &str) -> Element<'_, Message> {
    let card = column![
        text("Flux").size(28).color(ACCENT),
        text("This library is already in use").size(22).color(TEXT),
        text(message)
            .size(12)
            .color(MUTED)
            .width(Length::Fixed(560.0)),
        text(
            "The other Flux window keeps its catalogue safe while it is open. \
             Close that window (GUI or TUI), then start Flux again."
        )
        .size(11)
        .color(MUTED)
        .width(Length::Fixed(560.0)),
    ]
    .spacing(12)
    .align_x(Alignment::Center);

    container(card)
        .padding(28)
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .style(app_style)
        .into()
}

fn setup_card<'a>(
    heading: &'a str,
    description: &'a str,
    body: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(
        column![
            text(heading).size(9).color(ACCENT),
            text(description).size(10).color(MUTED),
            body.into(),
        ]
        .spacing(8),
    )
    .padding(14)
    .width(Length::Fill)
    .style(card_style)
    .into()
}

fn track_row<'a>(
    index: usize,
    id: TrackId,
    track: &'a Track,
    current: Option<TrackId>,
) -> Element<'a, Message> {
    let selected = current == Some(id);
    let marker = if selected {
        "▶".to_owned()
    } else {
        index.to_string()
    };
    let title_color = if selected { ACCENT } else { TEXT };
    let duration = track.duration().map(format_duration).unwrap_or_default();
    let playable = is_track_playable(track);

    let title = row![
        text(track.title()).size(13).color(title_color),
        if playable {
            Element::from(iced::widget::Space::new().width(0))
        } else {
            text("indexed, not playable").size(9).color(MUTED).into()
        },
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let content = row![
        text(marker)
            .size(10)
            .color(if selected { ACCENT } else { MUTED })
            .width(Length::Fixed(28.0)),
        column![title, text(display_artist(track)).size(10).color(MUTED),]
            .spacing(2)
            .width(Length::FillPortion(5)),
        text(display_album(track))
            .size(10)
            .color(MUTED)
            .width(Length::FillPortion(4)),
        text(duration)
            .size(10)
            .color(MUTED)
            .width(Length::Fixed(48.0)),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    let main = button(content)
        .width(Length::Fill)
        .padding([7, 10])
        .style(track_row_style(selected))
        .on_press(Message::PlayTrack(id));
    let add = button(text("+").size(12))
        .style(ghost_style)
        .padding([5, 7])
        .on_press(Message::EnqueueTrack(id));
    let next = button(text("→").size(12))
        .style(ghost_style)
        .padding([5, 7])
        .on_press(Message::PlayNextTrack(id));

    row![main, add, next]
        .spacing(2)
        .align_y(Alignment::Center)
        .into()
}

fn queue_row<'a>(
    entry: QueueEntry,
    index: usize,
    current: Option<QueueEntryId>,
    dragging: bool,
    track: Option<&'a Track>,
) -> Element<'a, Message> {
    let is_current = current == Some(entry.id());
    let playable = track.is_some_and(is_track_playable);
    let (title, subtitle, duration) = match track {
        Some(track) => (
            track.title().to_owned(),
            if playable {
                display_artist(track)
            } else {
                "Indexed, not playable".to_owned()
            },
            track.duration().map(format_duration).unwrap_or_default(),
        ),
        None => (
            "Missing from library".to_owned(),
            format!("track {}", entry.track()),
            String::new(),
        ),
    };

    let marker = if is_current {
        "▶".to_owned()
    } else {
        (index + 1).to_string()
    };
    let content = row![
        text(marker)
            .size(9)
            .color(if is_current { ACCENT } else { MUTED })
            .width(Length::Fixed(20.0)),
        column![
            text(title)
                .size(11)
                .color(if is_current || dragging { ACCENT } else { TEXT }),
            text(subtitle).size(9).color(MUTED),
        ]
        .spacing(1)
        .width(Length::Fill),
        text(duration).size(9).color(MUTED),
        text("⠿")
            .size(12)
            .color(if dragging { ACCENT } else { MUTED }),
    ]
    .spacing(5)
    .align_y(Alignment::Center);

    let main = container(content)
        .width(Length::Fill)
        .height(Length::Fixed(QUEUE_ROW_HEIGHT))
        .padding([3, 5])
        .style(queue_row_style(is_current, dragging));
    let remove = button(text("×").size(11))
        .style(ghost_style)
        .padding([3, 4])
        .on_press(Message::RemoveQueueEntry(entry.id()));

    let on_move = move |point: iced::Point| {
        Message::QueueDragHover(if point.y < QUEUE_ROW_HEIGHT / 2.0 {
            index
        } else {
            index + 1
        })
    };

    mouse_area(
        row![main, remove]
            .spacing(1)
            .height(Length::Fixed(QUEUE_ROW_HEIGHT))
            .align_y(Alignment::Center),
    )
    .on_press(Message::QueueDragStart(entry.id()))
    .on_move(on_move)
    .interaction(if dragging {
        mouse::Interaction::Grabbing
    } else {
        mouse::Interaction::Grab
    })
    .into()
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "Repeat: off",
        RepeatMode::All => "Repeat: all",
        RepeatMode::One => "Repeat: one",
    }
}

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
        .theme(Flux::theme)
        .subscription(Flux::subscription)
        .window(window::Settings {
            min_size: Some(iced::Size::new(920.0, 620.0)),
            ..window::Settings::default()
        })
        .window_size((1_180.0, 760.0))
        .exit_on_close_request(false)
        .run()
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

    fn gui(dir: &TempDir) -> FluxReady {
        let (engine, audio) = flux_app::default_engine();
        let controller =
            Controller::new(engine, audio, Some(dir.path().join("library-index.json")))
                .expect("controller opens the test library");
        let last_queue_current = controller.current_queue_entry_id();
        FluxReady {
            controller,
            dir_input: String::new(),
            search: String::new(),
            lib_input: String::new(),
            migrate_source: String::new(),
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
            view: LibraryView::Tracks,
            selected_group: None,
            page: 0,
            queue_page: 0,
            queue_drag: None,
            last_queue_current,
            scope: FlowScope::default(),
            show_setup: false,
            queue_visible: true,
            window_width: 1_180.0,
        }
    }

    fn send(gui: &mut FluxReady, message: Message) {
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

        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(
            &mut gui,
            Message::MigrateSourceChanged(index.display().to_string()),
        );

        send(&mut gui, Message::PlanMigrationPressed);
        assert!(gui.pending_plan.is_some(), "plan awaits confirmation");
        assert!(gui.lifecycle_msg.is_some(), "plan summary is shown");
        send(&mut gui, Message::CancelMigrationPressed);
        assert!(gui.pending_plan.is_none());
        assert!(!root.join(".flux").exists(), "cancel wrote nothing");
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        send(&mut gui, Message::PlanMigrationPressed);
        send(&mut gui, Message::ExecuteMigrationPressed);
        assert!(gui.pending_plan.is_none());
        assert_eq!(gui.migrated_dest.as_deref(), Some(root.as_path()));
        assert!(root.join(".flux").exists());
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

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

        send(&mut gui, Message::ReconcilePressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);
        assert!(gui.controller.last_error().is_some());
        assert!(gui.lifecycle_msg.is_none(), "no false success summary");

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

    #[test]
    fn replace_protected_index_message_reenables_saving() {
        let dir = TempDir::new("replace-protected");
        let index = dir.path().join("library-index.json");
        std::fs::write(&index, b"not a valid index").expect("corrupt index");
        let mut gui = gui(&dir);
        assert!(gui.controller.legacy_index_write_protected());

        send(&mut gui, Message::ReplaceLegacyIndexPressed);
        assert!(!gui.controller.legacy_index_write_protected());
        gui.controller.save_index().expect("new index writes");
        assert!(index.exists());
        gui.controller.shutdown();
    }

    #[test]
    fn queue_drag_messages_reorder_without_changing_the_current_entry() {
        let dir = TempDir::new("queue-drag");
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
        let mut gui = gui(&dir);
        assert_eq!(gui.controller.queue_len(), 2);
        let current = gui.controller.current_queue_entry_id().expect("current");
        let other = gui
            .controller
            .queue_entries()
            .iter()
            .find(|entry| entry.id() != current)
            .map(|entry| entry.id())
            .expect("second entry");
        let state = gui.controller.player_state();
        let order = |gui: &FluxReady| {
            gui.controller
                .queue_entries()
                .iter()
                .map(|entry| entry.id())
                .collect::<Vec<_>>()
        };

        send(&mut gui, Message::QueueDragStart(current));
        assert!(gui.queue_drag.is_some());
        send(&mut gui, Message::QueueDragHover(2));
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(order(&gui), vec![other, current]);
        assert_eq!(gui.controller.current_queue_entry_id(), Some(current));
        assert_eq!(gui.controller.player_state(), state);
        assert!(gui.queue_drag.is_none());
        assert_eq!(
            gui.controller.queue_entry_index(current),
            Some(1),
            "dropping at the end moved the entry, not the playback selection"
        );

        send(&mut gui, Message::EnqueueTrack(TrackId::new(1)));
        let duplicate = *order(&gui).last().expect("duplicate at end");
        send(&mut gui, Message::QueueDragStart(duplicate));
        send(&mut gui, Message::QueueDragHover(0));
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(order(&gui), vec![duplicate, other, current]);
        assert_eq!(gui.controller.current_queue_entry_id(), Some(current));
        assert_eq!(gui.controller.player_state(), state);

        let before_outside_drop = order(&gui);
        send(&mut gui, Message::QueueDragStart(duplicate));
        send(&mut gui, Message::QueueDragHover(2));
        send(&mut gui, Message::QueueDragLeave);
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(order(&gui), before_outside_drop);
        assert_eq!(gui.controller.current_queue_entry_id(), Some(current));
        assert!(gui.queue_drag.is_none());

        let before_invalid_drop = order(&gui);
        send(&mut gui, Message::QueueDragStart(duplicate));
        send(&mut gui, Message::QueueDragHover(99));
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(order(&gui), before_invalid_drop);
        assert_eq!(gui.controller.current_queue_entry_id(), Some(current));
        assert!(gui.queue_drag.is_none());

        send(&mut gui, Message::QueueDragStart(current));
        send(&mut gui, Message::QueuePageNext);
        assert!(gui.queue_drag.is_none());
        gui.controller.shutdown();
    }

    #[test]
    fn queue_drag_on_empty_queue_is_a_safe_noop() {
        let dir = TempDir::new("queue-drag-empty");
        let mut gui = gui(&dir);
        assert_eq!(gui.controller.queue_len(), 0);
        send(&mut gui, Message::QueueDragStart(QueueEntryId::new(1)));
        assert!(gui.queue_drag.is_none());
        send(&mut gui, Message::QueueDragHover(0));
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(gui.controller.queue_len(), 0);
        gui.controller.shutdown();
    }

    #[test]
    fn flow_scope_uses_web_audio_byte_quantization() {
        assert!((flow_centered(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((flow_centered(0.5) - 0.5).abs() < f32::EPSILON);
        assert!((flow_centered(-0.5) + 0.5).abs() < f32::EPSILON);
        assert!((flow_centered(-1.0) + 1.0).abs() < f32::EPSILON);
        assert!((flow_centered(1.0) - 127.0 / 128.0).abs() < f32::EPSILON);
    }

    #[test]
    fn flow_scope_reproduces_rms_smoothing_gain_and_reset() {
        let mut scope = FlowScope::default();
        let loud = [0.5f32; WAVEFORM_WINDOW];
        scope.update(&loud);

        assert!((scope.smooth_rms - 0.075).abs() < 1e-6);
        assert!((scope.gain - 1.4).abs() < 1e-6);
        assert!(
            scope
                .samples()
                .iter()
                .all(|sample| (*sample - 0.7).abs() < 1e-6)
        );

        let silence = [0.0f32; WAVEFORM_WINDOW];
        scope.update(&silence);
        assert!((scope.smooth_rms - 0.06375).abs() < 1e-6);
        assert!((scope.gain - 1.7).abs() < 1e-6);
        assert!(scope.samples().iter().all(|sample| *sample == 0.0));

        scope.reset();
        assert_eq!(scope.gain, 1.0);
        assert_eq!(scope.smooth_rms, 0.0);
        assert!(scope.samples().iter().all(|sample| *sample == 0.0));
    }

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
}
