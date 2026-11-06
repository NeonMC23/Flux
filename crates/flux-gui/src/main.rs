//! Flux desktop GUI (`flux`), built on Iced.
//!
//! A compact, dark desktop player over the shared
//! [`flux_app::Controller`] — the same controller the TUI uses. This file
//! owns presentation and view state only (navigation, pagination, setup
//! panels); every playback, library, and persistence decision stays in
//! the controller.
//!
//! The review-first Import Manager owns candidate selection/review state but
//! delegates all catalogue, duplicate, copy, and publication policy to
//! [`flux_app`]. Real audio output requires a build with `--features cpal`;
//! otherwise the player bar says so and playback attempts report why.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use flux_app::{
    AlbumId, Artist, AudioStatus, CROSSFADE_MAX_MS, CandidateStatus, CanonicalMigrationPlan,
    Controller, ImportAlbumTarget, ImportApplyReport, ImportDecision, ImportItemState, ImportPlan,
    ImportProgress, ImportRequest, ImportStage, ImportStorage, LyricLine, LyricsDocument,
    LyricsKind, PlaybackMode, PlaylistId, PreparedImport, ReconcileReport, RelPath, ReleaseType,
    ResourceState, StorageMode, TrackAudioReplacementPlan, TrackDeletionPlan, TrackMetadataEdit,
    TrackMetadataUpdatePlan, TrackMovePlan, WAVEFORM_WINDOW, format_duration, is_track_playable,
    prepare_import_files, prepare_import_plan,
};
use flux_core::{APP_NAME, ArtistId, PlaybackState, QueueEntry, QueueEntryId, Track, TrackId};
use iced::ContentFit;
use iced::advanced::Widget;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as RendererTrait};
use iced::advanced::widget::Tree;
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::image::Handle as ImageHandle;
use iced::widget::{
    button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, slider, stack,
    text, text_input, toggler, tooltip,
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
const SEARCH_RESULT_LIMIT: usize = 20;
const PLAYLIST_PAGE_SIZE: usize = 150;
const QUEUE_PAGE_SIZE: usize = 8;
const QUEUE_INLINE_MIN_WIDTH: f32 = 1_080.0;
const QUEUE_ROW_HEIGHT: f32 = 38.0;
const PLAYLIST_ROW_HEIGHT: f32 = 40.0;
const WAVE_TICK: Duration = Duration::from_millis(16);
const OSC_HEIGHT: f32 = 48.0;
const OSC_RADIUS: f32 = 14.0;
const OSC_LINE_WIDTH: f32 = 1.0;
const OSC_BACKGROUND: Color = Color::from_rgba8(6, 8, 12, 0.85);
const OSC_COLOR: Color = Color::from_rgb8(0xff, 0x66, 0xcc);
const OSC_GLOW: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.16);
const OSC_GUIDE: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.12);
const ARTWORK_CACHE_LIMIT: usize = 128;

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

fn modal_backdrop_style(_: &Theme) -> container::Style {
    container::Style::default().background(Color::from_rgba8(0, 0, 0, 0.70))
}

fn import_panel_style(_: &Theme) -> container::Style {
    container::Style::default()
        .background(SIDEBAR)
        .border(Border {
            color: ACCENT_BORDER,
            width: 1.0,
            radius: 14.0.into(),
        })
        .shadow(Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.55),
            offset: iced::Vector::new(0.0, 10.0),
            blur_radius: 28.0,
        })
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

fn transport_button_style(
    primary: bool,
    enabled: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if !enabled {
            return button::Style {
                background: Some(
                    (if primary {
                        SURFACE_HI
                    } else {
                        Color::TRANSPARENT
                    })
                    .into(),
                ),
                text_color: Color::from_rgba8(0x99, 0x8f, 0xa6, 0.35),
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 1.0,
                    radius: 9.0.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            };
        }
        if primary {
            accent_button_style(theme, status)
        } else {
            ghost_style(theme, status)
        }
    }
}

fn tooltip_style(_: &Theme) -> container::Style {
    container::Style::default()
        .background(SURFACE_HI)
        .border(Border {
            color: BORDER,
            width: 1.0,
            radius: 7.0.into(),
        })
        .shadow(Shadow::default())
}

fn seek_style(_: &Theme, status: slider::Status) -> slider::Style {
    let active = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    let played = if active {
        Color::from_rgb8(0xff, 0x86, 0xd8)
    } else {
        ACCENT
    };
    let remaining = if active { BORDER } else { SURFACE_HI };
    let radius = match status {
        slider::Status::Dragged => 7.0,
        slider::Status::Hovered => 6.5,
        slider::Status::Active => 6.0,
    };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (played.into(), remaining.into()),
            width: 4.0,
            border: iced::border::rounded(2.0),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius },
            background: ACCENT.into(),
            border_width: 2.0,
            border_color: BG,
        },
    }
}

fn volume_style(_: &Theme, status: slider::Status) -> slider::Style {
    let active = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    let played = if active {
        Color::from_rgb8(0xff, 0x86, 0xd8)
    } else {
        ACCENT
    };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (played.into(), SURFACE_HI.into()),
            width: 3.0,
            border: iced::border::rounded(1.5),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: if active { 5.0 } else { 4.0 },
            },
            background: ACCENT.into(),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

fn crossfade_toggler_style(_: &Theme, status: toggler::Status) -> toggler::Style {
    let toggled = match status {
        toggler::Status::Active { is_toggled }
        | toggler::Status::Hovered { is_toggled }
        | toggler::Status::Disabled { is_toggled } => is_toggled,
    };
    toggler::Style {
        background: if toggled { ACCENT_SOFT } else { SURFACE_HI }.into(),
        background_border_width: 1.0,
        background_border_color: if toggled { ACCENT_BORDER } else { BORDER },
        foreground: if toggled { ACCENT } else { MUTED }.into(),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: Some(TEXT),
        border_radius: Some(12.0.into()),
        padding_ratio: 0.16,
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

/// Samples the real waveform at a normalized horizontal position.
///
/// The oscilloscope has fewer horizontal pixels than the 4096-sample
/// window on most layouts. Linear interpolation avoids the visible
/// stair-stepping of nearest-neighbour sampling while preserving the
/// measured peaks at the window edges.
fn scope_sample(samples: &[f32], t: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    if samples.len() == 1 {
        return samples[0];
    }
    let position = t.clamp(0.0, 1.0) * (samples.len() - 1) as f32;
    let low = position.floor() as usize;
    let high = (low + 1).min(samples.len() - 1);
    let fraction = position - low as f32;
    samples[low] * (1.0 - fraction) + samples[high] * fraction
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

        // A restrained zero line makes quiet passages and the waveform
        // envelope easier to read without competing with the signal.
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x: bounds.x + 8.0,
                    y: center - 0.5,
                    width: (bounds.width - 16.0).max(0.0),
                    height: 1.0,
                },
                snap: false,
                ..Quad::default()
            },
            OSC_GUIDE,
        );

        let mut draw_trace = |color: Color, width: f32, halo: f32| {
            let mut previous_y = center;
            for column in 0..columns {
                let t = if columns <= 1 {
                    0.0
                } else {
                    column as f32 / (columns - 1) as f32
                };
                let value = scope_sample(self.samples, t).clamp(-1.0, 1.0);
                let y = center + value * (bounds.height / 2.0);
                let top = previous_y.min(y);
                let bottom = previous_y.max(y);
                let height = (bottom - top + width + halo * 2.0).max(width);
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle {
                            x: bounds.x + column as f32 - (width - 1.0).max(0.0) / 2.0,
                            y: top - halo,
                            width,
                            height,
                        },
                        snap: false,
                        ..Quad::default()
                    },
                    color,
                );
                previous_y = y;
            }

            // Flow closes the path at the right edge with a final line
            // back to the vertical center.
            let x = bounds.x + bounds.width - width;
            let top = previous_y.min(center);
            let bottom = previous_y.max(center);
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle {
                        x,
                        y: top - halo,
                        width,
                        height: (bottom - top + width + halo * 2.0).max(width),
                    },
                    snap: false,
                    ..Quad::default()
                },
                color,
            );
        };

        // A wider translucent pass gives the one-pixel Flow line a soft
        // neon halo without changing the underlying measured samples.
        draw_trace(OSC_GLOW, 3.0, 1.0);
        draw_trace(OSC_COLOR, OSC_LINE_WIDTH, 0.0);
    }
}

impl<'a, Message> From<FlowOscilloscope<'a>> for Element<'a, Message> {
    fn from(widget: FlowOscilloscope<'a>) -> Self {
        Self::new(widget)
    }
}

fn global_input_events(
    event: iced::Event,
    _status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::QueueDragRelease)
        }
        iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
            key:
                Key::Named(Named::ArrowLeft | Named::ArrowRight | Named::ArrowUp | Named::ArrowDown),
            ..
        }) => Some(Message::SeekCommit),
        iced::Event::Window(window::Event::FileHovered(_)) => Some(Message::ImportFileHovered),
        iced::Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::ImportDropPath(path))
        }
        iced::Event::Window(window::Event::FilesHoveredLeft) => Some(Message::ImportFileHoverLeft),
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
        AudioStatus::Recovering(reason) => (
            "Recovering audio output",
            format!("{reason} Flux is attempting to reopen the current default device."),
            ACCENT,
        ),
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

fn nonempty_string(value: &str) -> Option<String> {
    nonempty(Some(value))
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn new_browser(mode: BrowserMode, start: PathBuf) -> FileBrowser {
    let start = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(Path::new(".")).to_path_buf()
    };
    let mut browser = FileBrowser {
        mode,
        current: start,
        entries: Vec::new(),
        selected: HashSet::new(),
        path_input: String::new(),
        error: None,
        omitted: 0,
        symlinks_skipped: 0,
    };
    refresh_import_browser_data(&mut browser);
    browser
}

fn refresh_browser(browser: &mut FileBrowser) {
    refresh_import_browser_data(browser);
}

fn refresh_import_browser_data(browser: &mut FileBrowser) {
    browser.path_input = browser.current.display().to_string();
    browser.error = None;
    browser.omitted = 0;
    browser.symlinks_skipped = 0;
    let entries = match std::fs::read_dir(&browser.current) {
        Ok(entries) => entries,
        Err(err) => {
            browser.entries.clear();
            browser.error = Some(format!("Could not read this folder: {err}"));
            return;
        }
    };
    let mut rows = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                browser.omitted += 1;
                continue;
            }
        };
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                browser.omitted += 1;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            browser.symlinks_skipped += 1;
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if metadata.is_dir() {
            if name == ".flux" {
                continue;
            }
            rows.push(BrowserEntry {
                path,
                name,
                is_dir: true,
                supported: true,
                size: 0,
            });
        } else if metadata.is_file() && browser.mode == BrowserMode::Files {
            rows.push(BrowserEntry {
                path: path.clone(),
                name,
                is_dir: false,
                supported: flux_app::is_supported_audio_path(&path),
                size: metadata.len(),
            });
        }
    }
    rows.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    browser.entries = rows;
}

fn browser_enter(browser: &mut FileBrowser, path: PathBuf) {
    if path.is_dir() {
        browser.current = path;
        browser.selected.clear();
        refresh_import_browser_data(browser);
    }
}

fn browser_up(browser: &mut FileBrowser) {
    if let Some(parent) = browser.current.parent() {
        browser.current = parent.to_path_buf();
        browser.selected.clear();
        refresh_import_browser_data(browser);
    }
}

fn parse_editor_timestamp(value: &str) -> Result<Option<u64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let document =
        LyricsDocument::parse(&format!("[{value}]x")).map_err(|error| error.to_string())?;
    document
        .lines
        .first()
        .and_then(|line| line.timestamp_ms)
        .map(Some)
        .ok_or_else(|| "timestamp must use mm:ss.xxx".to_owned())
}

fn format_timestamp_ms(timestamp_ms: u64) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms / 1000) % 60;
    let millis = timestamp_ms % 1000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn status_description(status: &CandidateStatus) -> &'static str {
    match status {
        CandidateStatus::New => "No path, content, or metadata conflict was found.",
        CandidateStatus::ExistingPath(_) => {
            "The exact source path is already registered. Updating keeps its stable TrackId."
        }
        CandidateStatus::SameContent { .. } => {
            "A different path appears to hold identical content. Skipping avoids a duplicate; importing separately is an explicit choice."
        }
        CandidateStatus::SimilarMetadata(_) => {
            "A different audio file shares title/artist/album metadata. That alone is not treated as the same track; importing separately is an explicit choice."
        }
        CandidateStatus::SameTitleDifferentAudio(_) => {
            "The title matches an existing track, but the audio fingerprint differs. This is a metadata collision, not an audio duplicate."
        }
        CandidateStatus::ProbableAudio(_) => {
            "Size, duration and title agree, but the audio fingerprint differs. Treat this as uncertain; import separately only after checking the files."
        }
        CandidateStatus::AmbiguousAudio(_) => {
            "Several existing tracks match the evidence. No identity is selected automatically; import separately or skip."
        }
    }
}

fn parse_optional_number(value: &str, field: &str) -> Result<Option<u32>, String> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        value
            .parse::<u32>()
            .map(Some)
            .map_err(|_| format!("The {field} must be a non-negative whole number."))
    }
}

fn release_type_label(kind: ReleaseType) -> &'static str {
    match kind {
        ReleaseType::Album => "Album",
        ReleaseType::Ep => "EP",
        ReleaseType::Single => "Single",
        ReleaseType::Compilation => "Compilation",
        ReleaseType::Other => "Other",
    }
}

fn release_type_buttons<'a>(
    selected: ReleaseType,
    message: fn(ReleaseType) -> Message,
) -> Element<'a, Message> {
    let kinds = [
        ReleaseType::Album,
        ReleaseType::Ep,
        ReleaseType::Single,
        ReleaseType::Compilation,
        ReleaseType::Other,
    ];
    let mut row = row![].spacing(4);
    for kind in kinds {
        row = row.push(
            button(text(release_type_label(kind)).size(9))
                .style(nav_style(kind == selected))
                .padding([4, 7])
                .on_press(message(kind)),
        );
    }
    row.into()
}

fn deletion_decision_label(decision: &flux_app::ArtistDeletionDecision) -> String {
    match decision {
        flux_app::ArtistDeletionDecision::Unresolved => "Unresolved".to_owned(),
        flux_app::ArtistDeletionDecision::Transfer { new_owner, .. } => {
            format!("Transfer to {new_owner}")
        }
        flux_app::ArtistDeletionDecision::RemoveCredit { .. } => "Remove credit only".to_owned(),
        flux_app::ArtistDeletionDecision::Delete { confirmed: true } => {
            "PERMANENT DELETE".to_owned()
        }
        flux_app::ArtistDeletionDecision::Delete { confirmed: false } => {
            "Delete needs confirmation".to_owned()
        }
    }
}

fn display_artist(track: &Track) -> String {
    nonempty(track.artist()).unwrap_or_else(|| "Unknown artist".to_owned())
}

fn display_album(track: &Track) -> String {
    nonempty(track.album()).unwrap_or_else(|| "Standalone tracks".to_owned())
}

type HistoryEntityTotals = (Vec<(String, u64)>, Vec<(String, u64)>);

/// Aggregates meaningful plays by stable artist/album identity for the compact
/// dashboard breakdown. Display names are labels only; separate same-named
/// entities are never merged. TrackIds not present in the active catalogue are
/// ignored, so a switch/catalogue change cannot resurrect stale names.
fn history_entity_breakdown(controller: &Controller) -> HistoryEntityTotals {
    let mut artist_ids: BTreeMap<ArtistId, u64> = BTreeMap::new();
    let mut album_ids: BTreeMap<AlbumId, u64> = BTreeMap::new();
    let mut artist_names: BTreeMap<String, u64> = BTreeMap::new();
    let mut album_names: BTreeMap<String, u64> = BTreeMap::new();
    for (id, entry) in controller.playback_history().entries() {
        let Some(track) = controller.track(id) else {
            continue;
        };
        if let Some(artist) = controller.track_artist_id(id) {
            let total = artist_ids.entry(artist).or_default();
            *total = total.saturating_add(entry.play_count);
        } else {
            let total = artist_names.entry(display_artist(track)).or_default();
            *total = total.saturating_add(entry.play_count);
        }
        if let Some(album) = controller.track_album_id(id) {
            let total = album_ids.entry(album).or_default();
            *total = total.saturating_add(entry.play_count);
        } else {
            let total = album_names.entry(display_album(track)).or_default();
            *total = total.saturating_add(entry.play_count);
        }
    }
    let mut artists = artist_names.into_iter().collect::<Vec<_>>();
    artists.extend(artist_ids.into_iter().map(|(id, count)| {
        let name = controller
            .artist(id)
            .map_or_else(|| format!("Artist {id}"), |artist| artist.name().to_owned());
        (name, count)
    }));
    let mut albums = album_names.into_iter().collect::<Vec<_>>();
    albums.extend(album_ids.into_iter().map(|(id, count)| {
        let name = controller
            .album(id)
            .map_or_else(|| format!("Album {id}"), |album| album.title.clone());
        (name, count)
    }));
    artists.sort_by(|(left_name, left), (right_name, right)| {
        right.cmp(left).then_with(|| left_name.cmp(right_name))
    });
    albums.sort_by(|(left_name, left), (right_name, right)| {
        right.cmp(left).then_with(|| left_name.cmp(right_name))
    });
    (artists, albums)
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

#[derive(Debug, Clone)]
struct PlaylistDrag {
    playlist_id: PlaylistId,
    from: usize,
    expected_track_ids: Vec<TrackId>,
    target_gap: Option<usize>,
    moved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ArtworkOwner {
    Album(AlbumId),
    Artist(ArtistId),
    Track(TrackId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ArtworkKey {
    owner: ArtworkOwner,
    path: PathBuf,
    modified_ns: u128,
    len: u64,
}

#[derive(Debug, Clone)]
struct ArtworkData {
    handle: ImageHandle,
}

#[derive(Debug, Clone)]
struct ArtworkEntry {
    key: ArtworkKey,
    handle: Option<ImageHandle>,
    error: Option<String>,
}

fn artwork_key(owner: ArtworkOwner, path: PathBuf) -> ArtworkKey {
    let metadata = std::fs::metadata(&path).ok();
    let modified_ns = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    let len = metadata.map_or(0, |metadata| metadata.len());
    ArtworkKey {
        owner,
        path,
        modified_ns,
        len,
    }
}

fn decode_artwork(key: &ArtworkKey) -> Result<ArtworkData, String> {
    const MAX_BYTES: u64 = 32 * 1024 * 1024;
    const MAX_DIMENSION: u32 = 4096;
    const MAX_PIXELS: u64 = 16_000_000;
    const THUMB: u32 = 512;

    let metadata = std::fs::metadata(&key.path)
        .map_err(|_| "cover file is missing or unreadable".to_owned())?;
    if !metadata.is_file() {
        return Err("cover path is not a file".to_owned());
    }
    if metadata.len() == 0 {
        return Err("cover file is empty".to_owned());
    }
    if metadata.len() > MAX_BYTES {
        return Err("cover file is too large (limit 32 MiB)".to_owned());
    }
    let reader = image::ImageReader::open(&key.path)
        .map_err(|_| "cover file cannot be opened".to_owned())?
        .with_guessed_format()
        .map_err(|_| "cover file format cannot be identified".to_owned())?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
    ) {
        return Err("cover must be PNG, JPEG or WebP".to_owned());
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "cover dimensions cannot be read".to_owned())?;
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS
    {
        return Err(format!(
            "cover dimensions are unreasonable ({width}×{height})"
        ));
    }
    let decoded = image::open(&key.path).map_err(|_| "cover image cannot be decoded".to_owned())?;
    let thumb = decoded.thumbnail(THUMB, THUMB);
    let rgba = thumb.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(ArtworkData {
        handle: ImageHandle::from_rgba(width, height, rgba.into_raw()),
    })
}

fn fallback_artwork<'a>(seed: Option<&str>, size: f32) -> Element<'a, Message> {
    let (glyph, color) = match seed {
        Some(seed) => {
            let glyph = seed
                .chars()
                .find(|value| value.is_alphanumeric())
                .map(|value| value.to_uppercase().collect::<String>())
                .unwrap_or_else(|| "♪".to_owned());
            (glyph, artwork_color(seed))
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

fn artwork_image<'a>(
    handle: &ImageHandle,
    size: f32,
    content_fit: ContentFit,
) -> Element<'a, Message> {
    iced::widget::image(handle.clone())
        .width(size)
        .height(size)
        .content_fit(content_fit)
        .border_radius(8.0)
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

const ARTIST_KEY_SEPARATOR: char = '\u{1f}';
const ALBUM_KEY_SEPARATOR: char = '\u{1e}';

fn artist_group_key(artist_id: Option<ArtistId>, track: &Track) -> String {
    format!(
        "{}{ARTIST_KEY_SEPARATOR}{}",
        display_artist(track),
        artist_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_owned())
    )
}

fn artist_key_parts(key: &str) -> (&str, &str) {
    key.split_once(ARTIST_KEY_SEPARATOR)
        .unwrap_or((key, "none"))
}

fn album_key_for_title(album_id: AlbumId, title: &str) -> String {
    format!("{title}{ALBUM_KEY_SEPARATOR}{album_id}")
}

fn album_group_key(album_id: Option<flux_app::AlbumId>, track: &Track) -> String {
    format!(
        "{}{ALBUM_KEY_SEPARATOR}{}",
        display_album(track),
        album_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_owned())
    )
}

fn album_key_parts(key: &str) -> (&str, &str) {
    key.split_once(ALBUM_KEY_SEPARATOR).unwrap_or((key, "none"))
}

fn group_key(view: LibraryView, track: &Track) -> Option<String> {
    match view {
        LibraryView::Tracks => None,
        LibraryView::Artists => Some(artist_group_key(None, track)),
        LibraryView::Albums => Some(album_group_key(None, track)),
        LibraryView::Folders => Some(display_folder(track)),
        LibraryView::Playlists
        | LibraryView::Lyrics
        | LibraryView::Dashboard
        | LibraryView::Collections
        | LibraryView::RecentlyAdded
        | LibraryView::RecentlyPlayed
        | LibraryView::MostPlayed
        | LibraryView::Settings => None,
    }
}

fn group_title(view: LibraryView, key: &str) -> String {
    match view {
        LibraryView::Artists => artist_key_parts(key).0.to_owned(),
        LibraryView::Folders => Path::new(key)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_owned()),
        LibraryView::Albums => album_key_parts(key).0.to_owned(),
        LibraryView::Tracks
        | LibraryView::Playlists
        | LibraryView::Lyrics
        | LibraryView::Dashboard
        | LibraryView::Collections
        | LibraryView::RecentlyAdded
        | LibraryView::RecentlyPlayed
        | LibraryView::MostPlayed
        | LibraryView::Settings => key.to_owned(),
    }
}

// ----------------------------------------------------------------- state ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryView {
    Tracks,
    Artists,
    Albums,
    Folders,
    Playlists,
    Lyrics,
    Dashboard,
    Collections,
    RecentlyAdded,
    RecentlyPlayed,
    MostPlayed,
    Settings,
}

impl LibraryView {
    fn label(self) -> &'static str {
        match self {
            Self::Tracks => "Tracks",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Folders => "Folders",
            Self::Playlists => "Playlists",
            Self::Lyrics => "Lyrics",
            Self::Dashboard => "Dashboard",
            Self::Collections => "Smart Collections",
            Self::RecentlyAdded => "Recently Added",
            Self::RecentlyPlayed => "Recently Played",
            Self::MostPlayed => "Most Played",
            Self::Settings => "Settings",
        }
    }

    fn empty_group_label(self) -> &'static str {
        match self {
            Self::Tracks | Self::Artists => "No artists yet",
            Self::Albums => "No albums yet",
            Self::Folders => "No folders yet",
            Self::Playlists => "No playlists yet",
            Self::Lyrics => "No lyrics yet",
            Self::Dashboard => "Library is empty",
            Self::Collections => "No smart collections",
            Self::RecentlyAdded | Self::RecentlyPlayed | Self::MostPlayed => "No tracks yet",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePlacement {
    Hidden,
    Inline,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackResourceBadge {
    Missing,
    Inaccessible,
}

fn resource_badge(state: Option<ResourceState>) -> Option<TrackResourceBadge> {
    match state {
        Some(ResourceState::Missing) => Some(TrackResourceBadge::Missing),
        Some(ResourceState::Unavailable(_)) => Some(TrackResourceBadge::Inaccessible),
        Some(ResourceState::Present) | None => None,
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut shortened = value.chars().take(max_chars - 1).collect::<String>();
    shortened.push('…');
    shortened
}

fn track_list_empty_copy(
    view: LibraryView,
    filters_active: bool,
    grouped: bool,
) -> (&'static str, &'static str) {
    if filters_active {
        return (
            "No tracks match the current filters.",
            "Reset filters or choose another view.",
        );
    }
    match view {
        LibraryView::RecentlyAdded => (
            "No recently added tracks yet.",
            "Imported tracks with a recorded creation time appear here.",
        ),
        LibraryView::RecentlyPlayed => (
            "No listening history yet.",
            "Play a track through the shared controller to build local history.",
        ),
        LibraryView::MostPlayed => (
            "No listening history yet.",
            "Play a track to establish its local play count.",
        ),
        LibraryView::Collections => (
            "No tracks match this Smart Collection.",
            "Try another collection or adjust ratings, lyrics and artwork.",
        ),
        _ if grouped => (
            "No tracks in this group.",
            "Choose another group or return to the library.",
        ),
        _ => (
            "No tracks in this collection.",
            "Choose another view or import music.",
        ),
    }
}

fn import_has_committed_success(report: &ImportApplyReport) -> bool {
    report.imported > 0 || report.updated > 0
}

fn browser_omission_notice(omitted: usize, symlinks_skipped: usize) -> Option<String> {
    let mut parts = Vec::new();
    if omitted > 0 {
        parts.push(format!(
            "{omitted} entr{} disappeared or could not be inspected and {} omitted; refresh before selecting.",
            if omitted == 1 { "y" } else { "ies" },
            if omitted == 1 { "was" } else { "were" },
        ));
    }
    if symlinks_skipped > 0 {
        parts.push(format!(
            "{symlinks_skipped} symbolic link{} not followed.",
            if symlinks_skipped == 1 {
                " was"
            } else {
                "s were"
            },
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[derive(Debug, Clone, Default)]
struct SearchCache {
    query: String,
    generation: u64,
    artists: Vec<Artist>,
    albums: Vec<(AlbumId, String)>,
    track_ids: Vec<TrackId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TrackSort {
    #[default]
    Default,
    Title,
    Artist,
    Album,
    TrackNumber,
    Favorites,
    Rating,
    PlayCount,
    LastPlayed,
    DateAdded,
    Duration,
}

impl std::fmt::Display for TrackSort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Default => "Catalogue order",
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::TrackNumber => "Track / disc",
            Self::Favorites => "Favorites first",
            Self::Rating => "Rating",
            Self::PlayCount => "Play count",
            Self::LastPlayed => "Last played",
            Self::DateAdded => "Date added",
            Self::Duration => "Duration",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TriFilter {
    #[default]
    Any,
    With,
    Without,
}

impl TriFilter {
    fn cycle(self) -> Self {
        match self {
            Self::Any => Self::With,
            Self::With => Self::Without,
            Self::Without => Self::Any,
        }
    }
}

impl std::fmt::Display for TriFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Any => "Any",
            Self::With => "With",
            Self::Without => "Without",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ReleaseFilter {
    #[default]
    Any,
    Album,
    Ep,
    Single,
    Compilation,
    Other,
}

impl ReleaseFilter {
    fn matches(self, release_type: ReleaseType) -> bool {
        match self {
            Self::Any => true,
            Self::Album => release_type == ReleaseType::Album,
            Self::Ep => release_type == ReleaseType::Ep,
            Self::Single => release_type == ReleaseType::Single,
            Self::Compilation => release_type == ReleaseType::Compilation,
            Self::Other => release_type == ReleaseType::Other,
        }
    }
}

impl std::fmt::Display for ReleaseFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Any => "Any release",
            Self::Album => "Album",
            Self::Ep => "EP",
            Self::Single => "Single",
            Self::Compilation => "Compilation",
            Self::Other => "Other",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RatingFilter {
    #[default]
    Any,
    One,
    Two,
    Three,
    Four,
    Five,
}

impl RatingFilter {
    fn minimum(self) -> Option<u8> {
        match self {
            Self::Any => None,
            Self::One => Some(1),
            Self::Two => Some(2),
            Self::Three => Some(3),
            Self::Four => Some(4),
            Self::Five => Some(5),
        }
    }
}

impl std::fmt::Display for RatingFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Any => "Any rating",
            Self::One => "★ 1+",
            Self::Two => "★ 2+",
            Self::Three => "★ 3+",
            Self::Four => "★ 4+",
            Self::Five => "★ 5",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmartCollection {
    Favorites,
    HighlyRated,
    RecentlyAdded,
    RecentlyPlayed,
    MostPlayed,
    NoLyrics,
    SyncedLyrics,
    NoArtwork,
}

impl SmartCollection {
    const ALL: [Self; 8] = [
        Self::Favorites,
        Self::HighlyRated,
        Self::RecentlyAdded,
        Self::RecentlyPlayed,
        Self::MostPlayed,
        Self::NoLyrics,
        Self::SyncedLyrics,
        Self::NoArtwork,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Favorites => "Favorites",
            Self::HighlyRated => "Highly rated",
            Self::RecentlyAdded => "Recently added",
            Self::RecentlyPlayed => "Recently played",
            Self::MostPlayed => "Most played",
            Self::NoLyrics => "Without lyrics",
            Self::SyncedLyrics => "Synchronized lyrics",
            Self::NoArtwork => "Without artwork",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Favorites => "Tracks marked as favorites.",
            Self::HighlyRated => "Tracks rated 4★ or higher.",
            Self::RecentlyAdded => "Tracks with a reliable canonical creation timestamp.",
            Self::RecentlyPlayed => "Tracks with meaningful local playback history, newest first.",
            Self::MostPlayed => {
                "Historically most played tracks counted after the meaningful-play threshold."
            }
            Self::NoLyrics => "Tracks with no managed lyrics.",
            Self::SyncedLyrics => "Tracks with synchronized LRC lyrics.",
            Self::NoArtwork => "Tracks whose album has no managed cover.",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct DashboardSummary {
    generation: u64,
    history_generation: u64,
    total_tracks: usize,
    total_artists: usize,
    total_albums: usize,
    favorites: usize,
    rated: usize,
    with_lyrics: usize,
    synced_lyrics: usize,
    with_artwork: usize,
    recently_added: Vec<TrackId>,
    recently_played: Vec<TrackId>,
    most_played: Vec<TrackId>,
    highly_rated: Vec<TrackId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaylistChoice {
    id: PlaylistId,
    label: String,
}

impl std::fmt::Display for PlaylistChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label)
    }
}

#[derive(Debug, Clone)]
struct BatchReview {
    playlist_id: Option<PlaylistId>,
    playlist_label: String,
    expected_playlist_track_ids: Option<Vec<TrackId>>,
    new_playlist_name: String,
    track_ids: Vec<TrackId>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LyricsMode {
    #[default]
    Read,
    Edit,
    Karaoke,
}

#[derive(Debug, Clone)]
struct LyricsEditLine {
    text: String,
    timestamp: String,
}

#[derive(Debug, Clone)]
struct LyricsEditorState {
    track_id: TrackId,
    expected_text: Option<String>,
    metadata: Vec<(String, String)>,
    lines: Vec<LyricsEditLine>,
    original_synced: bool,
    allow_plain_conversion: bool,
    dirty: bool,
    discard_confirm: bool,
    remove_confirm: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct LyricsImportPreview {
    track_id: TrackId,
    path: PathBuf,
    raw: String,
    document: LyricsDocument,
    expected_text: Option<String>,
    error: Option<String>,
    overwrite_confirm: bool,
}

#[derive(Debug, Clone)]
struct LyricsCacheEntry {
    path: PathBuf,
    modified_ns: u128,
    len: u64,
    document: Option<LyricsDocument>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct QueueDrag {
    entry: QueueEntryId,
    target_gap: Option<usize>,
    moved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtistChoice {
    id: Option<ArtistId>,
    label: String,
}

impl std::fmt::Display for ArtistChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AlbumChoice {
    id: AlbumId,
    label: String,
}

impl std::fmt::Display for AlbumChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeletionUiStage {
    Review,
    Executing,
    Completed,
    Failed,
}

#[derive(Debug)]
struct DeletionManager {
    plan: flux_app::ArtistDeletionPlan,
    selected: Option<TrackId>,
    destination_artist: Option<ArtistChoice>,
    destination_album: Option<AlbumChoice>,
    new_album_title: String,
    stage: DeletionUiStage,
    report: Option<flux_app::ArtistDeletionReport>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlbumTransferUiStage {
    Review,
    Executing,
    Completed,
    Failed,
}

#[derive(Debug)]
struct AlbumTransferManager {
    album_id: AlbumId,
    destination_artist: Option<ArtistChoice>,
    plan: flux_app::AlbumTransferPlan,
    stage: AlbumTransferUiStage,
    report: Option<flux_app::AlbumTransferReport>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct TrackEditor {
    id: TrackId,
    title: String,
    album: String,
    track_number: String,
    disc_number: String,
    artist_id: Option<ArtistId>,
    new_artist: String,
    error: Option<String>,
    metadata_plan: Option<TrackMetadataUpdatePlan>,
    credits: Vec<ArtistId>,
    credit_artist: Option<ArtistId>,
    replacement_source: String,
    replacement_picked: Option<PathBuf>,
    replacement_plan: Option<TrackAudioReplacementPlan>,
    artwork_input: String,
    artwork_picked: Option<PathBuf>,
    artwork_remove_confirm: bool,
    deletion_plan: Option<TrackDeletionPlan>,
    move_destination: Option<AlbumChoice>,
    move_position: String,
    move_plan: Option<TrackMovePlan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportUiStage {
    Choose,
    Scanning,
    Review,
    Importing,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserMode {
    Files,
    Folder,
}

#[derive(Debug, Clone)]
struct BrowserEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    supported: bool,
    size: u64,
}

#[derive(Debug, Clone)]
struct FileBrowser {
    mode: BrowserMode,
    current: PathBuf,
    entries: Vec<BrowserEntry>,
    selected: HashSet<PathBuf>,
    path_input: String,
    error: Option<String>,
    omitted: usize,
    symlinks_skipped: usize,
}

#[derive(Debug)]
struct ImportManager {
    stage: ImportUiStage,
    target_album: Option<ImportAlbumTarget>,
    sources: Vec<PathBuf>,
    recursive: bool,
    storage: ImportStorage,
    plan: Option<ImportPlan>,
    selected: Option<usize>,
    browser: Option<FileBrowser>,
    progress: Arc<Mutex<ImportProgress>>,
    cancel: Arc<AtomicBool>,
    progress_view: ImportProgress,
    prepared: Option<PreparedImport>,
    report: Option<ImportApplyReport>,
    error: Option<String>,
    drop_hover: bool,
    pending_drops: Vec<PathBuf>,
    drop_deadline: Option<Instant>,
    manual_input: String,
    edit_title: String,
    edit_artist: String,
    edit_credit: String,
    edit_album: String,
    edit_track_number: String,
    edit_disc_number: String,
    edit_artwork: String,
    edit_album_artwork: String,
    edit_lyrics_path: String,
    checked: HashSet<usize>,
    bulk_artist: Option<ArtistChoice>,
    bulk_new_artist_name: String,
    bulk_album_title: String,
    bulk_album_artwork_path: String,
    asset_error: Option<String>,
    number_error: Option<String>,
}

impl ImportManager {
    fn new() -> Self {
        Self {
            stage: ImportUiStage::Choose,
            target_album: None,
            sources: Vec::new(),
            recursive: true,
            storage: ImportStorage::Copy,
            plan: None,
            selected: None,
            browser: None,
            progress: Arc::new(Mutex::new(ImportProgress::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            progress_view: ImportProgress::default(),
            prepared: None,
            report: None,
            error: None,
            drop_hover: false,
            pending_drops: Vec::new(),
            drop_deadline: None,
            manual_input: String::new(),
            edit_title: String::new(),
            edit_artist: String::new(),
            edit_credit: String::new(),
            edit_album: String::new(),
            edit_track_number: String::new(),
            edit_disc_number: String::new(),
            edit_artwork: String::new(),
            edit_album_artwork: String::new(),
            edit_lyrics_path: String::new(),
            checked: HashSet::new(),
            bulk_artist: None,
            bulk_new_artist_name: String::new(),
            bulk_album_title: String::new(),
            bulk_album_artwork_path: String::new(),
            asset_error: None,
            number_error: None,
        }
    }

    fn target_album(&self) -> Option<ImportAlbumTarget> {
        self.target_album.clone()
    }

    fn set_album_target(&mut self, target: ImportAlbumTarget) {
        self.target_album = Some(target);
        self.stage = ImportUiStage::Choose;
        self.sources.clear();
        self.plan = None;
        self.selected = None;
        self.error = None;
        self.report = None;
        self.prepared = None;
    }

    fn reset_for_selection(&mut self) {
        self.stage = ImportUiStage::Scanning;
        self.plan = None;
        self.selected = None;
        self.browser = None;
        self.prepared = None;
        self.report = None;
        self.error = None;
        self.progress = Arc::new(Mutex::new(ImportProgress::default()));
        self.progress_view = ImportProgress::default();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.checked.clear();
        self.bulk_artist = None;
        self.bulk_new_artist_name.clear();
        self.bulk_album_title.clear();
        self.bulk_album_artwork_path.clear();
        self.asset_error = None;
        self.number_error = None;
    }

    fn checked_indices(&self) -> Vec<usize> {
        let Some(plan) = self.plan.as_ref() else {
            return Vec::new();
        };
        let mut indices = self
            .checked
            .iter()
            .copied()
            .filter(|index| *index < plan.candidates.len())
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices
    }

    fn select_candidate(&mut self, index: usize) {
        let Some(candidate) = self
            .plan
            .as_ref()
            .and_then(|plan| plan.candidates.get(index))
        else {
            return;
        };
        self.selected = Some(index);
        self.edit_title = candidate.title().to_owned();
        self.edit_artist = candidate.artist_name().unwrap_or_default().to_owned();
        self.edit_credit.clear();
        self.edit_album = candidate.album().unwrap_or_default().to_owned();
        self.edit_track_number = candidate
            .track_number()
            .map(|number| number.to_string())
            .unwrap_or_default();
        self.edit_disc_number = candidate
            .disc_number()
            .map(|number| number.to_string())
            .unwrap_or_default();
        self.edit_artwork = candidate
            .artwork_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        self.edit_album_artwork = candidate
            .album_artwork_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        self.edit_lyrics_path.clear();
        self.asset_error = None;
        self.number_error = None;
    }
}

struct FluxReady {
    controller: Controller,
    search: String,
    search_cache: RefCell<SearchCache>,
    selected_tracks: Vec<TrackId>,
    focused_track: Option<TrackId>,
    filter_favorites: bool,
    filter_lyrics: TriFilter,
    filter_artwork: TriFilter,
    filter_release: ReleaseFilter,
    sort_tracks: TrackSort,
    sort_descending: bool,
    filter_rating: RatingFilter,
    collection: Option<SmartCollection>,
    dashboard_cache: RefCell<Option<DashboardSummary>>,
    batch_review: Option<BatchReview>,
    selected_playlist: Option<PlaylistId>,
    playlist_entry_selection: Vec<usize>,
    playlist_create_open: bool,
    playlist_create_name: String,
    playlist_name_input: String,
    playlist_delete_confirm: bool,
    playlist_clear_confirm: bool,
    playlist_remove_confirm: bool,
    playlist_drag: Option<PlaylistDrag>,
    playlist_error: Option<String>,
    selected_lyrics: Option<TrackId>,
    lyrics_mode: LyricsMode,
    lyrics_follow_current: bool,
    lyrics_filter: String,
    lyrics_editor: Option<LyricsEditorState>,
    lyrics_import: Option<LyricsImportPreview>,
    lyrics_export_path: String,
    lyrics_export_pending: Option<(PathBuf, String)>,
    lyrics_error: Option<String>,
    lyrics_cache: RefCell<BTreeMap<TrackId, LyricsCacheEntry>>,
    lib_input: String,
    track_editor: Option<TrackEditor>,
    import_manager: Option<ImportManager>,
    deletion_manager: Option<DeletionManager>,
    album_transfer: Option<AlbumTransferManager>,
    external_drag_hover: bool,
    artist_selected: Option<ArtistId>,
    artist_name_input: String,
    artist_rename_input: String,
    new_album_title: String,
    new_album_type: ReleaseType,
    album_title_input: String,
    album_type: ReleaseType,
    album_cover_input: String,
    album_cover_picked: Option<PathBuf>,
    album_delete_confirm: bool,
    album_cover_remove_confirm: bool,
    artwork: BTreeMap<AlbumId, ArtworkEntry>,
    artist_artwork: BTreeMap<ArtistId, ArtworkEntry>,
    track_artwork: BTreeMap<TrackId, ArtworkEntry>,
    artwork_loading: HashSet<ArtworkKey>,
    artist_detail_name_input: String,
    artist_description_input: String,
    artist_image_input: String,
    artist_image_picked: Option<PathBuf>,
    artist_image_remove_confirm: bool,
    last_reconcile: Option<ReconcileReport>,
    migrate_source: String,
    pending_plan: Option<CanonicalMigrationPlan>,
    migrated_dest: Option<PathBuf>,
    lifecycle_msg: Option<String>,
    view: LibraryView,
    selected_group: Option<String>,
    page: usize,
    queue_page: usize,
    queue_drag: Option<QueueDrag>,
    last_queue_current: Option<QueueEntryId>,
    scope: FlowScope,
    seek_preview: Option<f32>,
    crossfade_preview: Option<u64>,
    show_setup: bool,
    queue_visible: bool,
    queue_overlay_open: bool,
    window_width: f32,
    closed: bool,
}

#[derive(Debug, Clone)]
enum Message {
    LibInputChanged(String),
    EditTrack(TrackId),
    EditorTitleChanged(String),
    EditorAlbumChanged(String),
    EditorTrackNumberChanged(String),
    EditorDiscNumberChanged(String),
    EditorArtistChanged(Option<ArtistChoice>),
    EditorNewArtistChanged(String),
    EditorCreateArtist,
    EditorSave,
    EditorTransferOwner,
    EditorCancel,
    EditorCreditSelected(Option<ArtistChoice>),
    EditorCreditAdd,
    EditorCreditMove(usize, isize),
    EditorCreditRemove(usize),
    EditorReplacementChanged(String),
    EditorReplacementChoose,
    EditorReplacementPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    EditorPlanReplacement,
    EditorConfirmReplacement,
    EditorPlanDeletion,
    EditorConfirmDeletion,
    EditorCancelDeletion,
    EditorMoveDestination(Option<AlbumChoice>),
    EditorMovePositionChanged(String),
    EditorPlanMove,
    EditorPlanDetach,
    EditorConfirmMove,
    ArtistSelected(Option<ArtistChoice>),
    ArtistNameChanged(String),
    ArtistCreatePressed,
    ArtistRenameNameChanged(String),
    ArtistRenamePressed,
    ArtistDeleteOpen,
    ArtistDeletionSelect(TrackId),
    ArtistDeletionDestinationArtist(Option<ArtistChoice>),
    ArtistDeletionDestinationAlbum(Option<AlbumChoice>),
    ArtistDeletionNewAlbum(String),
    ArtistDeletionSetTransfer,
    ArtistDeletionSetDelete,
    ArtistDeletionSetRemoveCredit,
    ArtistDeletionResetDecision,
    ArtistDeletionExecute,
    ArtistDeletionCommit,
    ArtistDeletionCancel,
    AlbumTransferOpen,
    AlbumTransferDestinationArtist(Option<ArtistChoice>),
    AlbumTransferAllowCredit(TrackId, bool),
    AlbumTransferExecute,
    AlbumTransferCommit,
    AlbumTransferCancel,
    AlbumCreatePressed,
    NewAlbumTitleChanged(String),
    NewAlbumType(ReleaseType),
    AlbumTitleChanged(String),
    AlbumType(ReleaseType),
    AlbumMetadataSave,
    AlbumCoverChanged(String),
    AlbumCoverChoose,
    AlbumCoverPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    AlbumCoverApply,
    AlbumCoverRemove,
    ArtistDetailNameChanged(String),
    ArtistDescriptionChanged(String),
    ArtistMetadataSave,
    ArtistImageChanged(String),
    ArtistImageChoose,
    ArtistImagePicked(u64, Result<Option<Vec<PathBuf>>, String>),
    ArtistImageApply,
    ArtistImageRemove,
    TrackArtworkChanged(String),
    TrackArtworkChoose,
    TrackArtworkPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    TrackArtworkApply,
    TrackArtworkRemove,
    ArtworkLoaded(u64, ArtworkKey, Result<ArtworkData, String>),
    AlbumTrackMove(TrackId, isize),
    AlbumDeletePressed,
    AlbumDeleteConfirm,
    AlbumAddTracks,
    RelinkTrack(TrackId, RelPath),
    ImportOpen,
    ImportOpenSetup,
    ImportClose,
    ImportReset,
    ImportNoop,
    ImportNativeFiles,
    ImportNativeFolder,
    ImportNativeFilesPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    ImportNativeFolderPicked(u64, Result<Option<PathBuf>, String>),
    ImportBrowserOpenFiles,
    ImportBrowserOpenFolder,
    ImportBrowserUp,
    ImportBrowserEnter(PathBuf),
    ImportBrowserToggle(PathBuf),
    ImportBrowserSelectAll,
    ImportBrowserConfirm,
    ImportBrowserCancel,
    ImportBrowserRefresh,
    ImportBrowserPathChanged(String),
    ImportBrowserPathSubmit,
    ImportRecursiveToggled,
    ImportPlanReady(u64, Box<ImportPlan>),
    ImportPrepared(u64, Box<PreparedImport>),
    ImportSelect(usize),
    ImportToggleChecked(usize),
    ImportCheckAll,
    ImportClearChecked,
    ImportBulkArtistPicked(Option<ArtistChoice>),
    ImportBulkApplyArtist,
    ImportBulkNewArtistChanged(String),
    ImportBulkApplyNewArtist,
    ImportBulkAlbumChanged(String),
    ImportBulkApplyAlbum,
    ImportBulkMakeStandalone,
    ImportBulkAlbumArtworkChanged(String),
    ImportBulkAlbumArtworkPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    ImportBulkAlbumArtworkChoose,
    ImportBulkApplyAlbumArtwork,
    ImportBulkClearAlbumArtwork,
    ImportDecisionChanged(usize, ImportDecision),
    ImportBulkSkip,
    ImportBulkImport,
    ImportBulkUpdate,
    ImportTitleChanged(String),
    ImportArtistChanged(String),
    ImportAlbumChanged(String),
    ImportAlbumPicked(Option<AlbumChoice>),
    ImportTrackNumberChanged(String),
    ImportDiscNumberChanged(String),
    ImportArtistPicked(Option<ArtistChoice>),
    ImportCreditNameChanged(String),
    ImportCreditAddNamed,
    ImportCreditAddExisting(ArtistChoice),
    ImportCreditMove(usize, isize),
    ImportCreditRemove(usize),
    ImportCreditSetOwner(usize),
    ImportArtworkChanged(String),
    ImportArtworkChoose,
    ImportArtworkPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    ImportArtworkApply,
    ImportArtworkClear,
    ImportAlbumArtworkChanged(String),
    ImportAlbumArtworkChoose,
    ImportAlbumArtworkPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    ImportAlbumArtworkApply,
    ImportAlbumArtworkClear,
    ImportLyricsPathChanged(String),
    ImportLyricsAttach,
    ImportLyricsClear,
    ImportManualChanged(String),
    ImportManualAdd,
    ImportDropPath(PathBuf),
    ImportFileHovered,
    ImportFileHoverLeft,
    ImportApply,
    ImportCancel,
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
    RefreshPressed,
    SearchChanged(String),
    ClearSearch,
    SearchNavigateArtist(ArtistId),
    SearchNavigateAlbum(AlbumId),
    SearchNavigateTrack(TrackId),
    OpenAlbum(AlbumId),
    OpenArtist(ArtistId),
    TrackSelectionToggle(TrackId),
    TrackSelectionSelectAll,
    TrackSelectionClear,
    FilterFavoritesToggled,
    FilterLyricsCycle,
    FilterArtworkCycle,
    FilterReleaseChanged(ReleaseFilter),
    SortTracksChanged(TrackSort),
    SortDirectionToggled,
    FilterRatingChanged(RatingFilter),
    ResetLibraryFilters,
    CollectionSelect(SmartCollection),
    CollectionBack,
    TrackFavoriteToggle(TrackId),
    TrackRatingSet(TrackId, Option<u8>),
    BatchRating(Option<u8>),
    BatchPlaylistAdd(PlaylistId),
    BatchPlaylistCreateOpen,
    BatchPlaylistNameChanged(String),
    BatchReviewConfirm,
    BatchReviewCancel,
    BatchFavorites(bool),
    PlaylistSelect(PlaylistId),
    PlaylistBack,
    PlaylistCreateOpen,
    PlaylistCreateNameChanged(String),
    PlaylistCreateConfirm,
    PlaylistCreateCancel,
    PlaylistRenameChanged(String),
    PlaylistRenameSave,
    PlaylistPlay,
    PlaylistPlayEntry(usize),
    PlaylistShuffle,
    PlaylistEnqueue,
    PlaylistEnqueueSelected,
    PlaylistEnqueueEntry(usize),
    PlaylistEntryToggle(usize),
    PlaylistEntrySelectAll,
    PlaylistEntryClearSelection,
    PlaylistEntryRemove(usize),
    PlaylistRemoveSelected,
    PlaylistEntryMove(usize, isize),
    PlaylistClear,
    PlaylistDelete,
    PlaylistDragStart(usize),
    PlaylistDragHover(usize),
    PlaylistDragLeave,
    LyricsSelect(TrackId),
    LyricsBack,
    LyricsOpenCurrent,
    LyricsFilterChanged(String),
    LyricsModeChanged(LyricsMode),
    LyricsEditOpen,
    LyricsEditorTextChanged(usize, String),
    LyricsEditorTimestampChanged(usize, String),
    LyricsEditorAddLine,
    LyricsEditorRemoveLine(usize),
    LyricsEditorMoveLine(usize, isize),
    LyricsEditorSetCurrentTime(usize),
    LyricsEditorNudge(usize, i64),
    LyricsEditorClearTimestamp(usize),
    LyricsEditorSave,
    LyricsEditorCancel,
    LyricsEditorRemove,
    LyricsImportOpen,
    LyricsImportPicked(u64, Result<Option<Vec<PathBuf>>, String>),
    LyricsImportConfirm,
    LyricsImportCancel,
    LyricsExportPathChanged(String),
    LyricsExport(String),
    LyricsOpenAlbum(AlbumId),
    LyricsOpenArtist(ArtistId),
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
    RetryAudioOutput,
    DismissNotice,
    Next,
    Previous,
    CycleRepeat,
    VolumeChanged(f32),
    SeekPreview(f32),
    SeekCommit,
    CrossfadeEnabledChanged(bool),
    CrossfadeDurationPreview(f32),
    CrossfadeDurationCommit,
    KeyPressed(Key, Modifiers),
    Tick,
    WaveTick,
    WindowResized(f32),
    WindowCloseRequested(window::Id),
}

impl Message {
    /// The library instance that initiated an asynchronous operation, when
    /// the result is allowed to mutate library-derived UI state.
    fn library_session(&self) -> Option<u64> {
        match self {
            Self::EditorReplacementPicked(session, _)
            | Self::AlbumCoverPicked(session, _)
            | Self::ArtistImagePicked(session, _)
            | Self::TrackArtworkPicked(session, _)
            | Self::ImportArtworkPicked(session, _)
            | Self::ImportAlbumArtworkPicked(session, _)
            | Self::ImportBulkAlbumArtworkPicked(session, _)
            | Self::ArtworkLoaded(session, _, _)
            | Self::ImportNativeFilesPicked(session, _)
            | Self::ImportNativeFolderPicked(session, _)
            | Self::ImportPlanReady(session, _)
            | Self::ImportPrepared(session, _)
            | Self::LyricsImportPicked(session, _) => Some(*session),
            _ => None,
        }
    }

    #[cfg(test)]
    fn with_library_session(mut self, session: u64) -> Self {
        match &mut self {
            Self::EditorReplacementPicked(current, _)
            | Self::AlbumCoverPicked(current, _)
            | Self::ArtistImagePicked(current, _)
            | Self::TrackArtworkPicked(current, _)
            | Self::ImportArtworkPicked(current, _)
            | Self::ImportAlbumArtworkPicked(current, _)
            | Self::ImportBulkAlbumArtworkPicked(current, _)
            | Self::ArtworkLoaded(current, _, _)
            | Self::ImportNativeFilesPicked(current, _)
            | Self::ImportNativeFolderPicked(current, _)
            | Self::ImportPlanReady(current, _)
            | Self::ImportPrepared(current, _)
            | Self::LyricsImportPicked(current, _) => *current = session,
            _ => {}
        }
        self
    }
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
            search: String::new(),
            search_cache: RefCell::new(SearchCache::default()),
            selected_tracks: Vec::new(),
            focused_track: None,
            filter_favorites: false,
            filter_lyrics: TriFilter::Any,
            filter_artwork: TriFilter::Any,
            filter_release: ReleaseFilter::Any,
            sort_tracks: TrackSort::Default,
            sort_descending: false,
            filter_rating: RatingFilter::Any,
            collection: None,
            dashboard_cache: RefCell::new(None),
            batch_review: None,
            selected_playlist: None,
            playlist_entry_selection: Vec::new(),
            playlist_create_open: false,
            playlist_create_name: String::new(),
            playlist_name_input: String::new(),
            playlist_delete_confirm: false,
            playlist_clear_confirm: false,
            playlist_remove_confirm: false,
            playlist_drag: None,
            playlist_error: None,
            selected_lyrics: None,
            lyrics_mode: LyricsMode::Read,
            lyrics_follow_current: false,
            lyrics_filter: String::new(),
            lyrics_editor: None,
            lyrics_import: None,
            lyrics_export_path: String::new(),
            lyrics_export_pending: None,
            lyrics_error: None,
            lyrics_cache: RefCell::new(BTreeMap::new()),
            lib_input: String::new(),
            track_editor: None,
            import_manager: None,
            deletion_manager: None,
            album_transfer: None,
            external_drag_hover: false,
            artist_selected: None,
            artist_name_input: String::new(),
            artist_rename_input: String::new(),
            new_album_title: String::new(),
            new_album_type: ReleaseType::Album,
            album_title_input: String::new(),
            album_type: ReleaseType::Album,
            album_cover_input: String::new(),
            album_cover_picked: None,
            album_delete_confirm: false,
            album_cover_remove_confirm: false,
            artwork: BTreeMap::new(),
            artist_artwork: BTreeMap::new(),
            track_artwork: BTreeMap::new(),
            artwork_loading: HashSet::new(),
            artist_detail_name_input: String::new(),
            artist_description_input: String::new(),
            artist_image_input: String::new(),
            artist_image_picked: None,
            artist_image_remove_confirm: false,
            last_reconcile: None,
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
            seek_preview: None,
            crossfade_preview: None,
            show_setup,
            queue_visible: true,
            queue_overlay_open: false,
            window_width: 1_180.0,
            closed: false,
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
            iced::event::listen_with(global_input_events),
        ];
        if self.controller.player_state() == PlaybackState::Playing
            && self.controller.audio_status().is_ready()
        {
            subscriptions.push(iced::time::every(WAVE_TICK).map(|_| Message::WaveTick));
        }
        Subscription::batch(subscriptions)
    }

    fn after_switch(&mut self) {
        self.close_import_manager();
        self.batch_review = None;
        self.deletion_manager = None;
        self.album_transfer = None;
        self.search.clear();
        self.selected_tracks.clear();
        self.focused_track = None;
        self.pending_plan = None;
        self.migrated_dest = None;
        self.lifecycle_msg = None;
        self.queue_drag = None;
        self.seek_preview = None;
        self.track_editor = None;
        self.selected_lyrics = None;
        self.lyrics_mode = LyricsMode::Read;
        self.lyrics_follow_current = false;
        self.lyrics_editor = None;
        self.lyrics_import = None;
        self.lyrics_export_pending = None;
        self.lyrics_error = None;
        self.lyrics_cache.borrow_mut().clear();
        self.artist_selected = None;
        self.artist_name_input.clear();
        self.artist_rename_input.clear();
        self.new_album_title.clear();
        self.album_title_input.clear();
        self.album_cover_input.clear();
        self.album_cover_picked = None;
        self.album_delete_confirm = false;
        self.album_cover_remove_confirm = false;
        self.playlist_create_open = false;
        self.playlist_create_name.clear();
        self.playlist_name_input.clear();
        self.playlist_delete_confirm = false;
        self.playlist_clear_confirm = false;
        self.playlist_remove_confirm = false;
        self.playlist_drag = None;
        self.playlist_error = None;
        self.last_reconcile = None;
        self.search_cache.borrow_mut().generation = u64::MAX;
        let current = self.controller.current_queue_entry_id();
        self.queue_page = current
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .map_or(0, |index| index / QUEUE_PAGE_SIZE);
        self.last_queue_current = current;
        self.artwork.clear();
        self.artist_artwork.clear();
        self.track_artwork.clear();
        self.artwork_loading.clear();
        self.artist_detail_name_input.clear();
        self.artist_description_input.clear();
        self.artist_image_input.clear();
        self.artist_image_picked = None;
        self.artist_image_remove_confirm = false;
        *self.dashboard_cache.borrow_mut() = None;
        self.scope.reset();
        self.show_setup = false;
        self.external_drag_hover = false;
        self.select_view(LibraryView::Tracks);
    }

    fn follow_queue_current(&mut self) {
        let current = self.controller.current_queue_entry_id();
        if current != self.last_queue_current {
            self.last_queue_current = current;
            self.seek_preview = None;
            self.focus_queue_current();
            if self.lyrics_follow_current {
                self.selected_lyrics = self.controller.current_id();
                if self.lyrics_mode == LyricsMode::Edit {
                    self.lyrics_mode = LyricsMode::Read;
                    self.lyrics_editor = None;
                }
                self.lyrics_import = None;
            }
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

    fn finish_playlist_drag(&mut self) {
        let Some(drag) = self.playlist_drag.take() else {
            return;
        };
        let Some(gap) = drag.target_gap else {
            return;
        };
        if !drag.moved || self.selected_playlist != Some(drag.playlist_id) {
            return;
        }
        let Some(playlist) = self.controller.playlist(drag.playlist_id) else {
            self.playlist_error = Some("The playlist is no longer available.".to_owned());
            return;
        };
        if playlist.track_ids != drag.expected_track_ids {
            self.playlist_error =
                Some("Playlist changed during the drag; reload and try again.".to_owned());
            return;
        }
        if gap > playlist.track_ids.len() {
            return;
        }
        let to = if gap > drag.from { gap - 1 } else { gap };
        if to.min(playlist.track_ids.len().saturating_sub(1)) == drag.from {
            return;
        }
        match self.controller.reorder_playlist_entry(
            drag.playlist_id,
            drag.from,
            to.min(playlist.track_ids.len() - 1),
            &drag.expected_track_ids,
        ) {
            Ok(()) => {
                self.playlist_entry_selection.clear();
                self.playlist_error = None;
            }
            Err(error) => self.playlist_error = Some(error),
        }
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

    fn commit_seek_preview(&mut self) {
        let Some(preview) = self.seek_preview.take() else {
            return;
        };
        let Some(total) = self.controller.current_duration() else {
            return;
        };
        if total.is_zero() || !preview.is_finite() {
            return;
        }
        let target = Duration::from_secs_f32(preview.max(0.0)).min(total);
        if self.controller.seek(target).is_ok() {
            self.scope.reset();
        }
    }

    fn artist_choices(&self) -> Vec<ArtistChoice> {
        let artists: Vec<Artist> = self.controller.artists();
        let mut counts = BTreeMap::new();
        for artist in &artists {
            *counts.entry(artist.name().to_owned()).or_insert(0usize) += 1;
        }
        artists
            .into_iter()
            .map(|artist| {
                let duplicate = counts.get(artist.name()).copied().unwrap_or(0) > 1;
                ArtistChoice {
                    id: Some(artist.id()),
                    label: if duplicate {
                        format!("{} (ID {})", artist.name(), artist.id())
                    } else {
                        artist.name().to_owned()
                    },
                }
            })
            .collect()
    }

    fn editor_artist_choices(&self) -> Vec<ArtistChoice> {
        let mut choices = vec![ArtistChoice {
            id: None,
            label: "No stable artist".to_owned(),
        }];
        choices.extend(self.artist_choices());
        choices
    }

    fn selected_artist_id(&self) -> Option<ArtistId> {
        if self.view != LibraryView::Artists {
            return None;
        }
        let key = self.selected_group.as_deref()?;
        let (_, raw) = artist_key_parts(key);
        raw.parse::<u64>().ok().map(ArtistId::new)
    }

    fn selected_album_id(&self) -> Option<AlbumId> {
        if self.view != LibraryView::Albums {
            return None;
        }
        let key = self.selected_group.as_deref()?;
        let (_, raw) = album_key_parts(key);
        raw.parse::<u64>().ok().map(AlbumId::new)
    }

    fn open_album_transfer(&mut self, album_id: AlbumId, destination: Artist) {
        let destination_choice = ArtistChoice {
            id: Some(destination.id()),
            label: destination.name().to_owned(),
        };
        match self
            .controller
            .plan_album_transfer(album_id, destination.id())
        {
            Ok(plan) => {
                self.deletion_manager = None;
                self.album_transfer = Some(AlbumTransferManager {
                    album_id,
                    destination_artist: Some(destination_choice),
                    plan,
                    stage: AlbumTransferUiStage::Review,
                    report: None,
                    error: None,
                });
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn sync_artist_editor(&mut self) {
        self.artist_detail_name_input.clear();
        self.artist_description_input.clear();
        self.artist_image_input.clear();
        self.artist_image_picked = None;
        self.artist_image_remove_confirm = false;
        let Some(id) = self.selected_artist_id() else {
            return;
        };
        let Some(artist) = self.controller.artist(id) else {
            return;
        };
        self.artist_detail_name_input = artist.name().to_owned();
        self.artist_description_input = self
            .controller
            .artist_description(id)
            .unwrap_or_default()
            .to_owned();
    }

    fn sync_album_editor(&mut self) {
        self.album_delete_confirm = false;
        self.album_cover_remove_confirm = false;
        let Some(album_id) = self.selected_album_id() else {
            self.album_title_input.clear();
            self.album_cover_input.clear();
            self.album_cover_picked = None;
            self.album_type = ReleaseType::Album;
            return;
        };
        let Some(album) = self.controller.album(album_id) else {
            return;
        };
        self.album_title_input = album.title;
        self.album_type = album.release_type;
        self.album_cover_input.clear();
        self.album_cover_picked = None;
    }

    fn sync_deletion_defaults(&mut self) {
        let Some(manager) = self.deletion_manager.as_mut() else {
            return;
        };
        let Some(id) = manager.selected else {
            return;
        };
        let Some(track) = manager
            .plan
            .owned_tracks
            .iter()
            .chain(manager.plan.credited_tracks.iter())
            .find(|track| track.track_id == id)
            .cloned()
        else {
            return;
        };
        if track.owner_artist_id != manager.plan.artist_id {
            manager.destination_artist = None;
            manager.destination_album = None;
            manager.new_album_title.clear();
            return;
        }
        let (wanted_owner, wanted_destination) = match &track.decision {
            flux_app::ArtistDeletionDecision::Transfer {
                new_owner,
                destination,
            } => (Some(*new_owner), Some(destination.clone())),
            _ => (track.eligible_owners.first().copied(), None),
        };
        manager.destination_artist = wanted_owner.map(|owner| ArtistChoice {
            id: Some(owner),
            label: self
                .controller
                .artist(owner)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {owner}")),
        });
        manager.destination_album = None;
        manager.new_album_title.clear();
        if let Some(destination) = wanted_destination {
            match destination {
                flux_app::ArtistDeletionDestination::Existing(id) => {
                    manager.destination_album = manager
                        .plan
                        .destination_albums
                        .iter()
                        .find(|album| album.id == id)
                        .map(|album| AlbumChoice {
                            id: album.id,
                            label: format!("{} · {}", album.title, album.id),
                        });
                }
                flux_app::ArtistDeletionDestination::New { title, .. } => {
                    manager.new_album_title = title;
                }
            }
        }
        if manager.destination_album.is_none()
            && manager.new_album_title.is_empty()
            && let Some(owner) = manager
                .destination_artist
                .as_ref()
                .and_then(|choice| choice.id)
            && let Some(album) = manager
                .plan
                .destination_albums
                .iter()
                .find(|album| album.owner_artist_id == owner)
        {
            manager.destination_album = Some(AlbumChoice {
                id: album.id,
                label: format!("{} · {}", album.title, album.id),
            });
        }
    }

    fn start_track_editor(&mut self, id: TrackId) {
        let Some(track) = self.controller.track(id) else {
            return;
        };
        let metadata_plan = self.controller.plan_track_metadata_update(id).ok();
        self.track_editor = Some(TrackEditor {
            id,
            title: track.title().to_owned(),
            album: track.album().unwrap_or_default().to_owned(),
            track_number: track
                .track_number()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            disc_number: track
                .disc_number()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            artist_id: self.controller.track_artist_id(id),
            new_artist: String::new(),
            error: None,
            metadata_plan,
            credits: self.controller.track_credits(id),
            credit_artist: None,
            replacement_source: String::new(),
            replacement_picked: None,
            replacement_plan: None,
            artwork_input: String::new(),
            artwork_picked: None,
            artwork_remove_confirm: false,
            deletion_plan: None,
            move_destination: None,
            move_position: "1".to_owned(),
            move_plan: None,
        });
        self.show_setup = false;
    }

    fn import_album_choices(&self, candidate: &flux_app::ImportCandidate) -> Vec<AlbumChoice> {
        let Some(owner) = candidate.artist_id() else {
            return Vec::new();
        };
        self.controller
            .albums()
            .into_iter()
            .filter(|album| album.owner_artist_id == owner)
            .map(|album| AlbumChoice {
                id: album.id,
                label: format!("{} · AlbumId {}", album.title, album.id),
            })
            .collect()
    }

    fn editor_move_album_choices(&self, track_id: TrackId) -> Vec<AlbumChoice> {
        let Some(owner) = self.controller.track_artist_id(track_id) else {
            return Vec::new();
        };
        let current = self.controller.track_album_id(track_id);
        self.controller
            .albums()
            .into_iter()
            .filter(|album| album.owner_artist_id == owner && Some(album.id) != current)
            .map(|album| AlbumChoice {
                id: album.id,
                label: format!("{} · {}", album.title, album.id),
            })
            .collect()
    }

    fn add_editor_credit(&mut self) {
        let Some(editor) = self.track_editor.as_mut() else {
            return;
        };
        let Some(artist) = editor.credit_artist else {
            editor.error = Some("Select an artist to add as a credit.".to_owned());
            return;
        };
        if editor.credits.contains(&artist) {
            editor.error = Some("That artist is already credited.".to_owned());
            return;
        }
        editor.credits.push(artist);
        editor.credit_artist = None;
        editor.error = None;
    }

    fn move_editor_credit(&mut self, index: usize, delta: isize) {
        let Some(editor) = self.track_editor.as_mut() else {
            return;
        };
        if index >= editor.credits.len() {
            return;
        }
        let target = index as isize + delta;
        if target < 0 || target as usize >= editor.credits.len() {
            return;
        }
        editor.credits.swap(index, target as usize);
    }

    fn remove_editor_credit(&mut self, index: usize) {
        let Some(editor) = self.track_editor.as_mut() else {
            return;
        };
        if index >= editor.credits.len() || editor.credits.len() <= 1 {
            return;
        }
        let owner = self.controller.track_artist_id(editor.id);
        if owner == Some(editor.credits[index]) {
            editor.error = Some("The physical owner must remain credited.".to_owned());
            return;
        }
        editor.credits.remove(index);
    }

    fn plan_editor_replacement(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        let path = editor
            .replacement_picked
            .clone()
            .unwrap_or_else(|| PathBuf::from(editor.replacement_source.trim()));
        if path.as_os_str().is_empty() {
            if let Some(editor) = self.track_editor.as_mut() {
                editor.error = Some("Enter the replacement audio file path.".to_owned());
            }
            return;
        }
        match self.controller.plan_track_audio_replacement(id, &path) {
            Ok(plan) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.replacement_plan = Some(plan);
                    editor.error = None;
                }
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.replacement_plan = None;
                    editor.error = Some(error);
                }
            }
        }
    }

    fn confirm_editor_replacement(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let Some(plan) = editor.replacement_plan.clone() else {
            return;
        };
        match self.controller.execute_track_audio_replacement(&plan) {
            Ok(report) => {
                self.track_editor = None;
                self.lifecycle_msg = Some(format!(
                    "Replaced audio for track {}; TrackId, album, owner and references preserved.",
                    report.track_id
                ));
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.replacement_plan = None;
                    editor.error = Some(format!("{error} Review the replacement again."));
                }
            }
        }
    }

    fn plan_editor_deletion(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        match self.controller.plan_track_deletion(id) {
            Ok(plan) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.deletion_plan = Some(plan);
                    editor.error = None;
                }
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.deletion_plan = None;
                    editor.error = Some(error);
                }
            }
        }
    }

    fn confirm_editor_deletion(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let Some(plan) = editor.deletion_plan.clone() else {
            return;
        };
        let id = plan.track_id();
        match self.controller.delete_track(&plan) {
            Ok(report) => {
                self.selected_tracks.retain(|track| *track != id);
                if self.focused_track == Some(id) {
                    self.focused_track = None;
                }
                if self.selected_lyrics == Some(id) {
                    self.selected_lyrics = None;
                    self.lyrics_editor = None;
                    self.lyrics_import = None;
                }
                self.lyrics_cache.borrow_mut().remove(&id);
                self.track_editor = None;
                self.batch_review = None;
                self.album_transfer = None;
                self.deletion_manager = None;
                self.seek_preview = None;
                self.last_queue_current = self.controller.current_queue_entry_id();
                self.queue_page = self
                    .last_queue_current
                    .and_then(|entry| self.controller.queue_entry_index(entry))
                    .map_or(0, |index| index / QUEUE_PAGE_SIZE);
                self.scope.reset();
                *self.dashboard_cache.borrow_mut() = None;
                self.invalidate_search_cache();
                self.lifecycle_msg = Some(if report.cleanup_errors.is_empty() {
                    format!(
                        "Deleted track {} from the library; {} and artist retained.",
                        report.track_id,
                        report.album_id.map_or_else(
                            || "standalone state".to_owned(),
                            |album| format!("album {album}")
                        )
                    )
                } else {
                    format!(
                        "Deleted track {} from the catalogue. Managed-resource cleanup needs attention: {}",
                        report.track_id,
                        report.cleanup_errors.join("; ")
                    )
                });
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.deletion_plan = None;
                    editor.error = Some(error);
                }
            }
        }
    }

    fn plan_editor_move(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        let Some(destination) = editor.move_destination.as_ref().map(|choice| choice.id) else {
            if let Some(editor) = self.track_editor.as_mut() {
                editor.error = Some("Select a destination album.".to_owned());
            }
            return;
        };
        let index = match editor.move_position.trim().parse::<usize>() {
            Ok(position) if position >= 1 => position - 1,
            _ => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error =
                        Some("Insertion position must be a whole number starting at 1.".to_owned());
                }
                return;
            }
        };
        match self
            .controller
            .plan_track_move(id, Some(destination), index)
        {
            Ok(plan) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_plan = Some(plan);
                    editor.error = None;
                }
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_plan = None;
                    editor.error = Some(error);
                }
            }
        }
    }

    fn plan_editor_detach(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        match self.controller.plan_track_move(id, None, 0) {
            Ok(plan) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_destination = None;
                    editor.move_plan = Some(plan);
                    editor.error = None;
                }
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_plan = None;
                    editor.error = Some(error);
                }
            }
        }
    }

    fn confirm_editor_move(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let Some(plan) = editor.move_plan.clone() else {
            return;
        };
        match self.controller.execute_track_move(&plan) {
            Ok(report) => {
                self.track_editor = None;
                self.lifecycle_msg = Some(format!(
                    "Moved track {} from {} to {} at position {}.",
                    report.track_id,
                    report
                        .from_album_id
                        .map_or_else(|| "Standalone".to_owned(), |album| format!("album {album}")),
                    report
                        .to_album_id
                        .map_or_else(|| "Standalone".to_owned(), |album| format!("album {album}")),
                    report.insertion_index + 1
                ));
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_plan = None;
                    editor.error = Some(format!("{error} Review the move again."));
                }
            }
        }
    }

    fn save_track_editor(&mut self) {
        let Some(editor) = self.track_editor.as_ref().cloned() else {
            return;
        };
        let id = editor.id;
        let (track_number, disc_number) = match (
            parse_optional_number(&editor.track_number, "track number"),
            parse_optional_number(&editor.disc_number, "disc number"),
        ) {
            (Ok(track_number), Ok(disc_number)) => (track_number, disc_number),
            (Err(error), _) | (_, Err(error)) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
                return;
            }
        };
        let edit = TrackMetadataEdit {
            title: nonempty_string(&editor.title),
            artist_id: self.controller.track_artist_id(id),
            credited_artist_ids: Some(editor.credits.clone()),
            album: nonempty_string(&editor.album),
            track_number,
            disc_number,
        };
        let result = if let Some(plan) = &editor.metadata_plan {
            self.controller.execute_track_metadata_update(plan, &edit)
        } else {
            self.controller.edit_track_metadata(id, edit)
        };
        match result {
            Ok(()) => {
                self.track_editor = None;
                self.lifecycle_msg = Some(format!("Saved metadata for track {id}."));
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
    }

    fn transfer_track_editor_owner(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        let Some(new_owner) = editor.artist_id else {
            if let Some(editor) = self.track_editor.as_mut() {
                editor.error = Some("Select a credited artist as the new owner.".to_owned());
            }
            return;
        };
        let album_title = editor.album.trim().to_owned();
        if album_title.is_empty() {
            if let Some(editor) = self.track_editor.as_mut() {
                editor.error = Some("Enter the destination release title.".to_owned());
            }
            return;
        }
        let current_owner = self.controller.track_artist_id(id);
        if current_owner == Some(new_owner) {
            if let Some(editor) = self.track_editor.as_mut() {
                editor.error = Some("The selected artist already owns this track.".to_owned());
            }
            return;
        }
        let destination = self
            .controller
            .albums()
            .into_iter()
            .find(|album| album.owner_artist_id == new_owner && album.title == album_title)
            .map(|album| flux_app::OwnershipDestination::Existing(album.id))
            .unwrap_or_else(|| flux_app::OwnershipDestination::New {
                title: album_title,
                release_type: flux_app::ReleaseType::Album,
            });
        let plan = match self
            .controller
            .plan_track_ownership_transfer(id, new_owner, destination)
        {
            Ok(plan) => plan,
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
                return;
            }
        };
        match self.controller.transfer_track_ownership(&plan) {
            Ok(report) => {
                self.track_editor = None;
                self.lifecycle_msg = Some(format!(
                    "Track {} transferred from {} to {}.",
                    report.track_id, report.from_owner, report.to_owner
                ));
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
    }

    fn create_artist_from_editor(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let name = editor.new_artist.trim().to_owned();
        if name.is_empty() {
            return;
        }
        match self.controller.create_artist(&name) {
            Ok(id) => {
                let track_id = self.track_editor.as_ref().map(|editor| editor.id);
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.artist_id = Some(id);
                    editor.credit_artist = Some(id);
                    editor.new_artist.clear();
                    editor.error = None;
                    if let Some(track_id) = track_id {
                        editor.metadata_plan =
                            self.controller.plan_track_metadata_update(track_id).ok();
                    }
                }
                self.lifecycle_msg = Some(format!("Created artist {name} (ID {id})."));
            }
            Err(error) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
    }

    fn open_import_manager(&mut self) {
        if self.import_manager.is_none() {
            self.import_manager = Some(ImportManager::new());
        }
        if let Some(manager) = self.import_manager.as_mut() {
            manager.target_album = None;
        }
        self.show_setup = false;
        self.track_editor = None;
        self.external_drag_hover = false;
    }

    fn open_import_manager_for_album(&mut self, album_id: AlbumId) {
        let Some(album) = self.controller.album(album_id) else {
            self.lifecycle_msg = Some("Album is no longer in the library.".to_owned());
            return;
        };
        let Some(owner) = self.controller.artist(album.owner_artist_id) else {
            self.lifecycle_msg = Some("Album owner is unavailable.".to_owned());
            return;
        };
        let target = ImportAlbumTarget {
            album_id,
            owner_artist_id: owner.id(),
            owner_name: owner.name().to_owned(),
            title: album.title,
            release_type: album.release_type,
            cover: album.cover,
            track_order: album.track_order,
            generation: self.controller.generation(),
        };
        if self.import_manager.is_none() {
            self.import_manager = Some(ImportManager::new());
        }
        if let Some(manager) = self.import_manager.as_mut() {
            manager.set_album_target(target);
        }
        self.show_setup = false;
        self.track_editor = None;
        self.album_transfer = None;
        self.external_drag_hover = false;
    }

    fn close_import_manager(&mut self) {
        if let Some(manager) = &self.import_manager {
            manager.cancel.store(true, Ordering::Release);
        }
        self.import_manager = None;
        self.external_drag_hover = false;
    }

    fn start_import_scan(&mut self) -> Task<Message> {
        let Some(manager) = self.import_manager.as_mut() else {
            return Task::none();
        };
        if manager.sources.is_empty() {
            manager.error = Some("Choose at least one audio file or folder.".to_owned());
            manager.stage = ImportUiStage::Choose;
            return Task::none();
        }
        if !matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        ) {
            manager.error = Some(
                "Importing requires a portable library. Close this dialog and create or open \
                 one in Library setup."
                    .to_owned(),
            );
            manager.stage = ImportUiStage::Failed;
            return Task::none();
        }
        let Some(root) = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
        else {
            manager.error = Some("The portable library is unavailable.".to_owned());
            manager.stage = ImportUiStage::Failed;
            return Task::none();
        };
        let request = ImportRequest {
            sources: manager.sources.clone(),
            recursive: manager.recursive,
            storage: manager.storage,
            library_root: root,
            existing_tracks: self.controller.tracks().to_vec(),
            existing_artists: self
                .controller
                .tracks()
                .iter()
                .filter_map(|(id, _)| {
                    self.controller
                        .track_artist_id(*id)
                        .map(|artist| (*id, artist))
                })
                .collect(),
            existing_credits: self
                .controller
                .tracks()
                .iter()
                .map(|(id, _)| (*id, self.controller.track_credits(*id)))
                .collect(),
            existing_album_ids: self
                .controller
                .tracks()
                .iter()
                .filter_map(|(id, _)| {
                    self.controller
                        .track_album_id(*id)
                        .map(|album| (*id, album))
                })
                .collect(),
            target_album: manager.target_album(),
            generation: self.controller.generation(),
            library_session: self.controller.library_session(),
        };
        manager.reset_for_selection();
        manager.stage = ImportUiStage::Scanning;
        let progress = Arc::clone(&manager.progress);
        let cancel = Arc::clone(&manager.cancel);
        let session = self.controller.library_session();
        Task::perform(
            async move { prepare_import_plan(request, progress, cancel) },
            move |plan| Message::ImportPlanReady(session, Box::new(plan)),
        )
    }

    fn handle_import_key(&mut self, key: &Key, modifiers: &Modifiers) -> Task<Message> {
        let Some(manager) = self.import_manager.as_mut() else {
            return Task::none();
        };
        if manager.browser.is_some() {
            if matches!(key, Key::Named(Named::Escape)) {
                manager.browser = None;
            } else if matches!(key, Key::Named(Named::Enter)) {
                return self.update(Message::ImportBrowserConfirm);
            }
            return Task::none();
        }
        if matches!(key, Key::Named(Named::Escape)) {
            return self.update(Message::ImportCancel);
        }
        if manager.stage == ImportUiStage::Review {
            if let Some(selected) = manager.selected {
                let count = manager
                    .plan
                    .as_ref()
                    .map_or(0, |plan| plan.candidates.len());
                match key.as_ref() {
                    Key::Named(Named::ArrowUp) => {
                        manager.select_candidate(selected.saturating_sub(1));
                        return Task::none();
                    }
                    Key::Named(Named::ArrowDown) => {
                        manager.select_candidate((selected + 1).min(count.saturating_sub(1)));
                        return Task::none();
                    }
                    Key::Character("s") => {
                        return self.update(Message::ImportDecisionChanged(
                            selected,
                            ImportDecision::Skip,
                        ));
                    }
                    Key::Character("i") => {
                        return self.update(Message::ImportDecisionChanged(
                            selected,
                            ImportDecision::ImportNew,
                        ));
                    }
                    Key::Character("u") => {
                        return self.update(Message::ImportDecisionChanged(
                            selected,
                            ImportDecision::UpdateExisting,
                        ));
                    }
                    _ => {}
                }
            }
            if modifiers.contains(Modifiers::CTRL) && matches!(key, Key::Named(Named::Enter)) {
                return self.start_import_copy();
            }
        }
        Task::none()
    }

    fn start_import_copy(&mut self) -> Task<Message> {
        let Some(manager) = self.import_manager.as_mut() else {
            return Task::none();
        };
        let Some(plan) = manager.plan.clone() else {
            manager.error = Some("Review a selection before importing.".to_owned());
            return Task::none();
        };
        if manager.number_error.is_some() {
            manager.error = Some("Fix the highlighted track/disc number first.".to_owned());
            return Task::none();
        }
        if !plan.can_import() {
            manager.error = Some(
                plan.blocking_reason()
                    .unwrap_or_else(|| "There is nothing selected to import.".to_owned()),
            );
            return Task::none();
        }
        manager.stage = ImportUiStage::Importing;
        manager.prepared = None;
        manager.report = None;
        manager.error = None;
        manager.progress = Arc::new(Mutex::new(ImportProgress {
            stage: ImportStage::Copying,
            message: "Preparing selected files…".to_owned(),
            ..ImportProgress::default()
        }));
        manager.progress_view = ImportProgress {
            stage: ImportStage::Copying,
            message: "Preparing selected files…".to_owned(),
            ..ImportProgress::default()
        };
        manager.cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::clone(&manager.progress);
        let cancel = Arc::clone(&manager.cancel);
        let session = self.controller.library_session();
        Task::perform(
            async move { prepare_import_files(&plan, progress, cancel) },
            move |prepared| Message::ImportPrepared(session, Box::new(prepared)),
        )
    }

    fn apply_import_review(&mut self, prepared: PreparedImport) {
        let Some(manager) = self.import_manager.as_mut() else {
            return;
        };
        if prepared.cancelled {
            manager.stage = ImportUiStage::Cancelled;
            manager.progress_view.stage = ImportStage::Cancelled;
            manager.progress_view.message =
                "Import cancelled; no catalogue changes were made.".to_owned();
            manager.prepared = Some(prepared);
            return;
        }
        let Some(plan) = manager.plan.clone() else {
            manager.error = Some("The reviewed plan is unavailable.".to_owned());
            manager.stage = ImportUiStage::Failed;
            return;
        };
        manager.progress_view.stage = ImportStage::WritingCatalogue;
        manager.progress_view.message = "Writing catalogue changes…".to_owned();
        match self.controller.apply_import_plan(&plan, &prepared) {
            Ok(report) => {
                let failed = report.failed > 0 && !import_has_committed_success(&report);
                if failed {
                    manager.error = report
                        .items
                        .iter()
                        .find(|item| item.state == ImportItemState::Failed)
                        .map(|item| item.message.clone());
                    manager.stage = ImportUiStage::Failed;
                    manager.progress_view.stage = ImportStage::Failed;
                    manager.progress_view.message = "Import failed.".to_owned();
                } else {
                    manager.stage = ImportUiStage::Completed;
                    manager.progress_view.stage = ImportStage::Completed;
                    manager.progress_view.message = "Import complete.".to_owned();
                }
                manager.report = Some(report);
                manager.prepared = Some(prepared);
            }
            Err(error) => {
                manager.error = Some(error.clone());
                manager.prepared = Some(prepared);
                manager.stage = ImportUiStage::Failed;
                manager.progress_view.stage = ImportStage::Failed;
                manager.progress_view.message = error;
            }
        }
    }

    fn prepare_native_import_picker(&mut self) {
        if self.import_manager.is_none() {
            self.import_manager = Some(ImportManager::new());
        }
        self.show_setup = false;
        self.track_editor = None;
        self.external_drag_hover = false;
    }

    fn native_files_task(&mut self) -> Task<Message> {
        let start = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
            .or_else(home_directory);
        self.prepare_native_import_picker();
        let session = self.controller.library_session();
        Task::perform(
            async move { flux_platform::pick_audio_files_in(start.as_deref()) },
            move |result| Message::ImportNativeFilesPicked(session, result),
        )
    }

    fn native_folder_task(&mut self) -> Task<Message> {
        let start = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
            .or_else(home_directory);
        self.prepare_native_import_picker();
        let session = self.controller.library_session();
        Task::perform(
            async move { flux_platform::pick_folder_in(start.as_deref()) },
            move |result| Message::ImportNativeFolderPicked(session, result),
        )
    }

    fn open_import_browser(&mut self, mode: BrowserMode) {
        self.prepare_native_import_picker();
        let Some(manager) = self.import_manager.as_mut() else {
            return;
        };
        let start = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
            .or_else(home_directory)
            .unwrap_or_else(|| PathBuf::from("."));
        manager.browser = Some(new_browser(mode, start));
    }

    fn refresh_import_browser(&mut self) {
        if let Some(manager) = self.import_manager.as_mut()
            && let Some(browser) = manager.browser.as_mut()
        {
            refresh_browser(browser);
        }
    }

    fn start_import_from_selection(
        &mut self,
        sources: Vec<PathBuf>,
        mode: BrowserMode,
    ) -> Task<Message> {
        let Some(manager) = self.import_manager.as_mut() else {
            return Task::none();
        };
        manager.browser = None;
        manager.sources = sources;
        let _ = mode;
        self.start_import_scan()
    }

    fn queue_external_drop(&mut self, path: PathBuf) -> Task<Message> {
        if !matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        ) {
            self.open_import_manager();
            if let Some(manager) = self.import_manager.as_mut() {
                manager.stage = ImportUiStage::Failed;
                manager.error = Some(
                    "Importing requires a canonical library. Create or open one in Library \
                     setup, then drop the files again."
                        .to_owned(),
                );
            }
            return Task::none();
        }
        self.open_import_manager();
        let Some(manager) = self.import_manager.as_mut() else {
            return Task::none();
        };
        if !matches!(manager.stage, ImportUiStage::Choose | ImportUiStage::Failed) {
            manager.error = Some(
                "Finish or cancel the current import review before adding more files.".to_owned(),
            );
            return Task::none();
        }
        manager.stage = ImportUiStage::Choose;
        manager.error = None;
        manager.plan = None;
        manager.pending_drops.push(path);
        manager.drop_deadline = Some(Instant::now());
        Task::none()
    }

    fn seek_relative(&mut self, seconds: f32) {
        let Some(total) = self.controller.current_duration() else {
            return;
        };
        if total.is_zero() {
            return;
        }
        let base = self
            .seek_preview
            .unwrap_or_else(|| self.controller.position().as_secs_f32());
        self.seek_preview = None;
        let target = Duration::from_secs_f32((base + seconds).max(0.0)).min(total);
        if self.controller.seek(target).is_ok() {
            self.scope.reset();
        }
    }

    fn select_view(&mut self, view: LibraryView) {
        if let Some(editor) = self.lyrics_editor.as_mut()
            && editor.dirty
            && !editor.discard_confirm
        {
            editor.discard_confirm = true;
            editor.error = Some(
                "Unsaved lyrics changes will be discarded. Choose the destination again to leave the editor."
                    .to_owned(),
            );
            return;
        }
        self.view = view;
        self.selected_group = None;
        self.sync_artist_editor();
        self.selected_playlist = None;
        self.collection = None;
        self.playlist_entry_selection.clear();
        self.playlist_drag = None;
        self.queue_overlay_open = false;
        self.selected_lyrics = None;
        self.lyrics_follow_current = false;
        self.lyrics_editor = None;
        self.lyrics_import = None;
        self.search.clear();
        self.page = 0;
        self.crossfade_preview = None;
        self.show_setup = false;
    }

    fn queue_placement(&self) -> QueuePlacement {
        if self.window_width >= QUEUE_INLINE_MIN_WIDTH {
            if self.queue_visible {
                QueuePlacement::Inline
            } else {
                QueuePlacement::Hidden
            }
        } else if self.queue_overlay_open {
            QueuePlacement::Overlay
        } else {
            QueuePlacement::Hidden
        }
    }

    fn toggle_queue(&mut self) {
        self.cancel_queue_drag();
        if self.window_width >= QUEUE_INLINE_MIN_WIDTH {
            self.queue_visible = !self.queue_visible;
            self.queue_overlay_open = false;
        } else {
            self.queue_overlay_open = !self.queue_overlay_open;
        }
    }

    fn active_library_path(&self) -> Option<&Path> {
        self.controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .or_else(|| self.controller.legacy_index_path())
    }

    fn active_library_label(&self) -> String {
        let Some(path) = self.active_library_path() else {
            return "Session-only library".to_owned();
        };
        path.file_name()
            .filter(|name| !name.is_empty())
            .map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
    }

    fn toggle_setup(&mut self) {
        self.show_setup = !self.show_setup;
        self.page = 0;
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if self.closed {
            return Task::none();
        }
        if message
            .library_session()
            .is_some_and(|session| session != self.controller.library_session())
        {
            return Task::none();
        }
        match message {
            Message::LibInputChanged(value) => self.lib_input = value,
            Message::EditTrack(id) => self.start_track_editor(id),
            Message::EditorTitleChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.title = value;
                }
            }
            Message::EditorAlbumChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.album = value;
                }
            }
            Message::EditorTrackNumberChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.track_number = value;
                }
            }
            Message::EditorDiscNumberChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.disc_number = value;
                }
            }
            Message::EditorArtistChanged(choice) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.artist_id = choice.and_then(|choice| choice.id);
                }
            }
            Message::EditorNewArtistChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.new_artist = value;
                }
            }
            Message::EditorCreateArtist => self.create_artist_from_editor(),
            Message::EditorSave => self.save_track_editor(),
            Message::EditorTransferOwner => self.transfer_track_editor_owner(),
            Message::EditorCancel => self.track_editor = None,
            Message::EditorCreditSelected(choice) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.credit_artist = choice.and_then(|choice| choice.id);
                }
            }
            Message::EditorCreditAdd => self.add_editor_credit(),
            Message::EditorCreditMove(index, delta) => self.move_editor_credit(index, delta),
            Message::EditorCreditRemove(index) => self.remove_editor_credit(index),
            Message::EditorReplacementChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.replacement_source = value;
                    editor.replacement_picked = None;
                    editor.replacement_plan = None;
                }
            }
            Message::EditorReplacementChoose => {
                let start = self
                    .track_editor
                    .as_ref()
                    .and_then(|editor| self.controller.track(editor.id))
                    .and_then(|track| track.source().path().parent().map(Path::to_path_buf))
                    .or_else(|| self.controller.canonical_root().map(Path::to_path_buf));
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_audio_files_in(start.as_deref()) },
                    move |result| Message::EditorReplacementPicked(session, result),
                );
            }
            Message::EditorReplacementPicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.first()
                        && let Some(editor) = self.track_editor.as_mut()
                    {
                        editor.replacement_source = path.display().to_string();
                        editor.replacement_picked = Some(path.clone());
                        editor.replacement_plan = None;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(editor) = self.track_editor.as_mut() {
                        editor.error = Some(error);
                    } else {
                        self.lifecycle_msg = Some(error);
                    }
                }
            },
            Message::EditorPlanReplacement => self.plan_editor_replacement(),
            Message::EditorConfirmReplacement => self.confirm_editor_replacement(),
            Message::EditorPlanDeletion => self.plan_editor_deletion(),
            Message::EditorConfirmDeletion => self.confirm_editor_deletion(),
            Message::EditorCancelDeletion => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.deletion_plan = None;
                }
            }
            Message::EditorMoveDestination(choice) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_destination = choice;
                    editor.move_plan = None;
                }
            }
            Message::EditorMovePositionChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.move_position = value;
                    editor.move_plan = None;
                }
            }
            Message::EditorPlanMove => self.plan_editor_move(),
            Message::EditorPlanDetach => self.plan_editor_detach(),
            Message::EditorConfirmMove => self.confirm_editor_move(),
            Message::ArtistSelected(choice) => {
                self.artist_selected = choice.as_ref().and_then(|choice| choice.id);
                self.artist_rename_input = self
                    .artist_selected
                    .and_then(|id| self.controller.artist(id))
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_default();
            }
            Message::ArtistNameChanged(value) => self.artist_name_input = value,
            Message::ArtistCreatePressed => {
                let name = self.artist_name_input.trim().to_owned();
                if !name.is_empty() {
                    match self.controller.create_artist(&name) {
                        Ok(id) => {
                            self.artist_selected = Some(id);
                            self.artist_name_input.clear();
                            self.artist_rename_input = name.clone();
                            self.invalidate_search_cache();
                            self.lifecycle_msg = Some(format!("Created artist {name} (ID {id})."));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                }
            }
            Message::ArtistRenameNameChanged(value) => self.artist_rename_input = value,
            Message::ArtistRenamePressed => {
                if let Some(id) = self.artist_selected {
                    let name = self.artist_rename_input.trim().to_owned();
                    if !name.is_empty() {
                        match self.controller.rename_artist(id, &name) {
                            Ok(()) => {
                                self.invalidate_search_cache();
                                self.lifecycle_msg =
                                    Some(format!("Renamed artist {id} to {name}."));
                            }
                            Err(error) => self.lifecycle_msg = Some(error),
                        }
                    }
                }
            }
            Message::ArtistDeleteOpen => {
                let Some(id) = self.artist_selected else {
                    self.lifecycle_msg = Some("Select an artist first.".to_owned());
                    return Task::none();
                };
                match self.controller.plan_artist_deletion(id) {
                    Ok(plan) => {
                        self.import_manager = None;
                        let selected = plan
                            .owned_tracks
                            .first()
                            .or_else(|| plan.credited_tracks.first())
                            .map(|track| track.track_id);
                        self.deletion_manager = Some(DeletionManager {
                            plan,
                            selected,
                            destination_artist: None,
                            destination_album: None,
                            new_album_title: String::new(),
                            stage: DeletionUiStage::Review,
                            report: None,
                            error: None,
                        });
                        self.show_setup = false;
                        self.sync_deletion_defaults();
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::ArtistDeletionSelect(id) => {
                if let Some(manager) = self.deletion_manager.as_mut() {
                    manager.selected = Some(id);
                    manager.destination_artist = None;
                    manager.destination_album = None;
                    manager.new_album_title.clear();
                    manager.error = None;
                    manager.stage = DeletionUiStage::Review;
                }
                self.sync_deletion_defaults();
            }
            Message::ArtistDeletionDestinationArtist(choice) => {
                if let Some(manager) = self.deletion_manager.as_mut() {
                    manager.destination_artist = choice;
                    manager.destination_album = None;
                }
            }
            Message::ArtistDeletionDestinationAlbum(choice) => {
                if let Some(manager) = self.deletion_manager.as_mut() {
                    manager.destination_album = choice;
                    if manager.destination_album.is_some() {
                        manager.new_album_title.clear();
                    }
                }
            }
            Message::ArtistDeletionNewAlbum(value) => {
                if let Some(manager) = self.deletion_manager.as_mut() {
                    manager.new_album_title = value;
                    if !manager.new_album_title.trim().is_empty() {
                        manager.destination_album = None;
                    }
                }
            }
            Message::ArtistDeletionSetTransfer => {
                let Some(manager) = self.deletion_manager.as_mut() else {
                    return Task::none();
                };
                let Some(id) = manager.selected else {
                    return Task::none();
                };
                let Some(new_owner) = manager
                    .destination_artist
                    .as_ref()
                    .and_then(|choice| choice.id)
                else {
                    manager.error = Some("Choose a surviving destination artist.".to_owned());
                    return Task::none();
                };
                let new_title = manager.new_album_title.trim().to_owned();
                let destination = if new_title.is_empty() {
                    let Some(album) = manager.destination_album.as_ref() else {
                        manager.error =
                            Some("Choose a destination album or enter a new title.".to_owned());
                        return Task::none();
                    };
                    flux_app::ArtistDeletionDestination::Existing(album.id)
                } else {
                    flux_app::ArtistDeletionDestination::New {
                        title: new_title,
                        release_type: flux_app::ReleaseType::Album,
                    }
                };
                match manager.plan.set_transfer(id, new_owner, destination) {
                    Ok(()) => manager.error = None,
                    Err(error) => manager.error = Some(error),
                }
            }
            Message::ArtistDeletionSetDelete => {
                if let Some(manager) = self.deletion_manager.as_mut()
                    && let Some(id) = manager.selected
                {
                    match manager.plan.set_delete(id, true) {
                        Ok(()) => manager.error = None,
                        Err(error) => manager.error = Some(error),
                    }
                }
            }
            Message::ArtistDeletionSetRemoveCredit => {
                if let Some(manager) = self.deletion_manager.as_mut()
                    && let Some(id) = manager.selected
                {
                    match manager.plan.set_remove_credit(id, None) {
                        Ok(()) => manager.error = None,
                        Err(error) => manager.error = Some(error),
                    }
                }
            }
            Message::ArtistDeletionResetDecision => {
                if let Some(manager) = self.deletion_manager.as_mut()
                    && let Some(id) = manager.selected
                {
                    manager.plan.set_unresolved(id);
                    manager.error = None;
                }
            }
            Message::ArtistDeletionExecute => {
                let Some(manager) = self.deletion_manager.as_mut() else {
                    return Task::none();
                };
                if !manager.plan.is_ready() {
                    manager.error = manager
                        .plan
                        .blocking_reason()
                        .map(str::to_owned)
                        .or_else(|| Some("Resolve every affected track first.".to_owned()));
                    return Task::none();
                }
                manager.stage = DeletionUiStage::Executing;
                manager.error = None;
                return Task::done(Message::ArtistDeletionCommit);
            }
            Message::ArtistDeletionCommit => {
                let Some(manager) = self.deletion_manager.as_mut() else {
                    return Task::none();
                };
                let plan = manager.plan.clone();
                match self.controller.execute_artist_deletion(&plan) {
                    Ok(report) => {
                        if let Some(manager) = self.deletion_manager.as_mut() {
                            manager.report = Some(report);
                            manager.stage = DeletionUiStage::Completed;
                        }
                        self.lifecycle_msg = Some("Artist deletion committed.".to_owned());
                    }
                    Err(error) => {
                        if let Some(manager) = self.deletion_manager.as_mut() {
                            manager.error = Some(error);
                            manager.stage = DeletionUiStage::Failed;
                        }
                    }
                }
            }
            Message::ArtistDeletionCancel => {
                self.deletion_manager = None;
            }
            Message::AlbumAddTracks => {
                let Some(album) = self.selected_album_id() else {
                    self.lifecycle_msg = Some("Select an album first.".to_owned());
                    return Task::none();
                };
                self.open_import_manager_for_album(album);
            }
            Message::AlbumTransferOpen => {
                let Some(album_id) = self.selected_album_id() else {
                    self.lifecycle_msg = Some("Select an album first.".to_owned());
                    return Task::none();
                };
                let Some(owner) = self.controller.album_owner(album_id) else {
                    self.lifecycle_msg =
                        Some("The selected album is no longer in the library.".to_owned());
                    return Task::none();
                };
                let destination = self
                    .controller
                    .artists()
                    .into_iter()
                    .find(|artist| artist.id() != owner);
                let Some(destination) = destination else {
                    self.lifecycle_msg =
                        Some("No other artist is available as a destination.".to_owned());
                    return Task::none();
                };
                self.open_album_transfer(album_id, destination);
            }
            Message::AlbumTransferDestinationArtist(choice) => {
                let Some(album_id) = self.album_transfer.as_ref().map(|manager| manager.album_id)
                else {
                    return Task::none();
                };
                let Some(destination) =
                    choice.and_then(|choice| choice.id.and_then(|id| self.controller.artist(id)))
                else {
                    return Task::none();
                };
                self.open_album_transfer(album_id, destination);
            }
            Message::AlbumTransferAllowCredit(track, allowed) => {
                if let Some(manager) = self.album_transfer.as_mut() {
                    match manager.plan.set_credit_allowed(track, allowed) {
                        Ok(()) => manager.error = None,
                        Err(error) => manager.error = Some(error),
                    }
                }
            }
            Message::AlbumTransferExecute => {
                let Some(manager) = self.album_transfer.as_mut() else {
                    return Task::none();
                };
                if !manager.plan.is_ready() {
                    manager.error = manager
                        .plan
                        .blocking_reason()
                        .map(str::to_owned)
                        .or_else(|| Some("Resolve every blocker before transferring.".to_owned()));
                    return Task::none();
                }
                manager.stage = AlbumTransferUiStage::Executing;
                manager.error = None;
                return Task::done(Message::AlbumTransferCommit);
            }
            Message::AlbumTransferCommit => {
                let Some(manager) = self.album_transfer.as_mut() else {
                    return Task::none();
                };
                let plan = manager.plan.clone();
                match self.controller.execute_album_transfer(&plan) {
                    Ok(report) => {
                        if let Some(manager) = self.album_transfer.as_mut() {
                            manager.report = Some(report);
                            manager.stage = AlbumTransferUiStage::Completed;
                        }
                        self.lifecycle_msg = Some("Album transfer committed.".to_owned());
                    }
                    Err(error) => {
                        if let Some(manager) = self.album_transfer.as_mut() {
                            manager.error = Some(error);
                            manager.stage = AlbumTransferUiStage::Failed;
                        }
                    }
                }
            }
            Message::AlbumTransferCancel => {
                self.album_transfer = None;
            }
            Message::AlbumCreatePressed => {
                let Some(artist) = self.artist_selected else {
                    self.lifecycle_msg = Some("Select an artist first.".to_owned());
                    return Task::none();
                };
                let title = self.new_album_title.trim().to_owned();
                if title.is_empty() {
                    self.lifecycle_msg = Some("Album title is required.".to_owned());
                    return Task::none();
                }
                match self
                    .controller
                    .create_album(artist, &title, self.new_album_type)
                {
                    Ok(album) => {
                        self.new_album_title.clear();
                        self.view = LibraryView::Albums;
                        let title = self
                            .controller
                            .album(album)
                            .map(|album| album.title)
                            .unwrap_or(title);
                        self.selected_group = Some(album_key_for_title(album, &title));
                        self.sync_album_editor();
                        self.lifecycle_msg = Some(format!("Created empty album {album}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::NewAlbumTitleChanged(value) => self.new_album_title = value,
            Message::NewAlbumType(kind) => self.new_album_type = kind,
            Message::AlbumTitleChanged(value) => self.album_title_input = value,
            Message::AlbumType(kind) => self.album_type = kind,
            Message::AlbumMetadataSave => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                match self.controller.update_album_metadata(
                    album,
                    &self.album_title_input,
                    self.album_type,
                ) {
                    Ok(()) => {
                        self.invalidate_search_cache();
                        self.sync_album_editor();
                        if let Some(updated) = self.controller.album(album) {
                            self.selected_group = Some(album_key_for_title(album, &updated.title));
                        }
                        self.lifecycle_msg = Some(format!("Updated album {album}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::AlbumCoverChanged(value) => {
                self.album_cover_input = value;
                self.album_cover_picked = None;
            }
            Message::AlbumCoverChoose => {
                let start = self
                    .selected_album_id()
                    .and_then(|album| self.controller.album_cover_path(album))
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                    .or_else(|| self.controller.canonical_root().map(Path::to_path_buf));
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::AlbumCoverPicked(session, result),
                );
            }
            Message::AlbumCoverPicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.first() {
                        self.album_cover_input = path.display().to_string();
                        self.album_cover_picked = Some(path.clone());
                    }
                }
                Ok(None) => {}
                Err(error) => self.lifecycle_msg = Some(error),
            },
            Message::ArtworkLoaded(_, key, result) => {
                self.artwork_loading.remove(&key);
                let still_current = match key.owner {
                    ArtworkOwner::Album(album) => self
                        .controller
                        .album_cover_path(album)
                        .is_some_and(|path| artwork_key(key.owner, path) == key),
                    ArtworkOwner::Artist(artist) => self
                        .controller
                        .artist_image_path(artist)
                        .is_some_and(|path| artwork_key(key.owner, path) == key),
                    ArtworkOwner::Track(track) => self
                        .controller
                        .track_artwork_path(track)
                        .is_some_and(|path| artwork_key(key.owner, path) == key),
                };
                if still_current {
                    let (handle, error) = match result {
                        Ok(data) => (Some(data.handle), None),
                        Err(error) => (None, Some(error)),
                    };
                    let owner = key.owner;
                    let entry = ArtworkEntry { key, handle, error };
                    match owner {
                        ArtworkOwner::Album(album) => {
                            if self.artwork.len() >= ARTWORK_CACHE_LIMIT
                                && !self.artwork.contains_key(&album)
                            {
                                self.artwork.clear();
                            }
                            self.artwork.insert(album, entry);
                        }
                        ArtworkOwner::Artist(artist) => {
                            self.artist_artwork.insert(artist, entry);
                        }
                        ArtworkOwner::Track(track) => {
                            self.track_artwork.insert(track, entry);
                        }
                    }
                }
            }
            Message::AlbumCoverApply => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                let path = self
                    .album_cover_picked
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(self.album_cover_input.trim()));
                if path.as_os_str().is_empty() {
                    self.lifecycle_msg = Some("Choose a cover image path.".to_owned());
                    return Task::none();
                }
                if let Err(error) =
                    decode_artwork(&artwork_key(ArtworkOwner::Album(album), path.clone()))
                {
                    self.lifecycle_msg = Some(format!(
                        "Cover not assigned; the image was rejected: {error}"
                    ));
                    return Task::none();
                }
                match self.controller.set_album_cover(album, &path) {
                    Ok(()) => {
                        *self.dashboard_cache.borrow_mut() = None;
                        self.album_cover_input.clear();
                        self.album_cover_picked = None;
                        self.artwork.remove(&album);
                        self.sync_album_editor();
                        self.lifecycle_msg = Some(format!("Updated cover for album {album}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::AlbumCoverRemove => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                if !self.album_cover_remove_confirm {
                    self.album_cover_remove_confirm = true;
                    self.lifecycle_msg = Some(
                        "Click “Confirm remove cover” to remove the managed artwork reference and file."
                            .to_owned(),
                    );
                    return Task::none();
                }
                match self.controller.remove_album_cover(album) {
                    Ok(()) => {
                        *self.dashboard_cache.borrow_mut() = None;
                        self.artwork.remove(&album);
                        self.sync_album_editor();
                        self.lifecycle_msg = Some(format!("Removed cover for album {album}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::ArtistDetailNameChanged(value) => self.artist_detail_name_input = value,
            Message::ArtistDescriptionChanged(value) => self.artist_description_input = value,
            Message::ArtistMetadataSave => {
                let Some(artist) = self.selected_artist_id() else {
                    return Task::none();
                };
                match self.controller.update_artist_metadata(
                    artist,
                    &self.artist_detail_name_input,
                    Some(self.artist_description_input.as_str()),
                ) {
                    Ok(()) => {
                        self.invalidate_search_cache();
                        *self.dashboard_cache.borrow_mut() = None;
                        if let Some(updated) = self.controller.artist(artist) {
                            self.selected_group =
                                Some(format!("{}{ARTIST_KEY_SEPARATOR}{artist}", updated.name()));
                        }
                        self.sync_artist_editor();
                        self.lifecycle_msg = Some(format!("Updated artist {artist}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::ArtistImageChanged(value) => {
                self.artist_image_input = value;
                self.artist_image_picked = None;
                self.artist_image_remove_confirm = false;
            }
            Message::ArtistImageChoose => {
                let start = self
                    .selected_artist_id()
                    .and_then(|artist| self.controller.artist_image_path(artist))
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                    .or_else(|| self.controller.canonical_root().map(Path::to_path_buf));
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::ArtistImagePicked(session, result),
                );
            }
            Message::ArtistImagePicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.first() {
                        self.artist_image_input = path.display().to_string();
                        self.artist_image_picked = Some(path.clone());
                        self.artist_image_remove_confirm = false;
                    }
                }
                Ok(None) => {}
                Err(error) => self.lifecycle_msg = Some(error),
            },
            Message::ArtistImageApply => {
                let Some(artist) = self.selected_artist_id() else {
                    return Task::none();
                };
                let path = self
                    .artist_image_picked
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(self.artist_image_input.trim()));
                if path.as_os_str().is_empty() {
                    self.lifecycle_msg = Some("Choose an artist image path.".to_owned());
                    return Task::none();
                }
                if let Err(error) =
                    decode_artwork(&artwork_key(ArtworkOwner::Artist(artist), path.clone()))
                {
                    self.lifecycle_msg = Some(format!("Artist image rejected: {error}"));
                    return Task::none();
                }
                match self.controller.set_artist_image(artist, &path) {
                    Ok(()) => {
                        self.artist_artwork.remove(&artist);
                        self.artist_image_input.clear();
                        self.artist_image_picked = None;
                        self.sync_artist_editor();
                        self.lifecycle_msg = Some(format!("Updated image for artist {artist}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::ArtistImageRemove => {
                let Some(artist) = self.selected_artist_id() else {
                    return Task::none();
                };
                if !self.artist_image_remove_confirm {
                    self.artist_image_remove_confirm = true;
                    self.lifecycle_msg = Some(
                        "Click “Confirm remove image” again to remove the managed artist image."
                            .to_owned(),
                    );
                    return Task::none();
                }
                match self.controller.remove_artist_image(artist) {
                    Ok(()) => {
                        self.artist_artwork.remove(&artist);
                        self.sync_artist_editor();
                        self.lifecycle_msg = Some(format!("Removed image for artist {artist}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::TrackArtworkChanged(value) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.artwork_input = value;
                    editor.artwork_picked = None;
                    editor.artwork_remove_confirm = false;
                }
            }
            Message::TrackArtworkChoose => {
                let Some(editor) = self.track_editor.as_ref() else {
                    return Task::none();
                };
                let start = self
                    .controller
                    .track_artwork_path(editor.id)
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                    .or_else(|| self.controller.canonical_root().map(Path::to_path_buf));
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::TrackArtworkPicked(session, result),
                );
            }
            Message::TrackArtworkPicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.first()
                        && let Some(editor) = self.track_editor.as_mut()
                    {
                        editor.artwork_input = path.display().to_string();
                        editor.artwork_picked = Some(path.clone());
                        editor.artwork_remove_confirm = false;
                    }
                }
                Ok(None) => {}
                Err(error) => self.lifecycle_msg = Some(error),
            },
            Message::TrackArtworkApply => {
                let Some(editor) = self.track_editor.as_ref() else {
                    return Task::none();
                };
                let track = editor.id;
                let path = editor
                    .artwork_picked
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(editor.artwork_input.trim()));
                if path.as_os_str().is_empty() {
                    self.lifecycle_msg = Some("Choose a track artwork path.".to_owned());
                    return Task::none();
                }
                if let Err(error) =
                    decode_artwork(&artwork_key(ArtworkOwner::Track(track), path.clone()))
                {
                    self.lifecycle_msg = Some(format!("Track artwork rejected: {error}"));
                    return Task::none();
                }
                match self.controller.set_track_artwork(track, &path) {
                    Ok(()) => {
                        self.track_artwork.remove(&track);
                        if let Some(editor) = self.track_editor.as_mut() {
                            editor.artwork_input.clear();
                            editor.artwork_picked = None;
                        }
                        self.lifecycle_msg =
                            Some(format!("Updated artwork override for track {track}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::TrackArtworkRemove => {
                let Some(editor) = self.track_editor.as_mut() else {
                    return Task::none();
                };
                if !editor.artwork_remove_confirm {
                    editor.artwork_remove_confirm = true;
                    self.lifecycle_msg =
                        Some("Click “Confirm restore album artwork” again to remove this Track override.".to_owned());
                    return Task::none();
                }
                let track = editor.id;
                match self.controller.remove_track_artwork(track) {
                    Ok(()) => {
                        self.track_artwork.remove(&track);
                        if let Some(editor) = self.track_editor.as_mut() {
                            editor.artwork_remove_confirm = false;
                        }
                        self.lifecycle_msg =
                            Some(format!("Restored album artwork for track {track}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::AlbumTrackMove(track, delta) => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                match self.controller.reorder_album_track(album, track, delta) {
                    Ok(()) => {
                        self.lifecycle_msg = Some("Updated canonical album order.".to_owned())
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::AlbumDeletePressed => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                if self
                    .controller
                    .album(album)
                    .is_some_and(|album| !album.track_order.is_empty())
                {
                    self.lifecycle_msg = Some(
                        "Populated albums cannot be deleted here; transfer or delete tracks first."
                            .to_owned(),
                    );
                } else {
                    self.album_delete_confirm = true;
                }
            }
            Message::AlbumDeleteConfirm => {
                let Some(album) = self.selected_album_id() else {
                    return Task::none();
                };
                match self.controller.delete_empty_album(album) {
                    Ok(()) => {
                        self.selected_group = None;
                        self.album_delete_confirm = false;
                        self.lifecycle_msg = Some(format!("Deleted empty album {album}."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::RelinkTrack(id, candidate) => {
                match self.controller.relink_portable_track(id, &candidate) {
                    Ok(()) => {
                        self.last_reconcile = None;
                        self.lifecycle_msg = Some(format!(
                            "Relinked track {id} to {}.",
                            candidate.display_lossy()
                        ));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::ImportOpen => {
                self.open_import_manager();
            }
            Message::ImportOpenSetup => {
                self.close_import_manager();
                self.show_setup = true;
            }
            Message::ImportClose => self.close_import_manager(),
            Message::ImportReset => {
                let target = self
                    .import_manager
                    .as_ref()
                    .and_then(|manager| manager.target_album.clone());
                let recursive = self
                    .import_manager
                    .as_ref()
                    .is_none_or(|manager| manager.recursive);
                self.import_manager = Some(ImportManager::new());
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.recursive = recursive;
                    manager.storage = ImportStorage::Copy;
                    manager.target_album = target;
                }
                self.show_setup = false;
                self.track_editor = None;
                self.external_drag_hover = false;
            }
            Message::ImportNoop => {}
            Message::ImportNativeFiles => return self.native_files_task(),
            Message::ImportNativeFolder => return self.native_folder_task(),
            Message::ImportNativeFilesPicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(manager) = self.import_manager.as_mut() {
                        manager.browser = None;
                        manager.sources = paths;
                        manager.error = None;
                    }
                    return self.start_import_scan();
                }
                Ok(None) => {}
                Err(error) => {
                    self.open_import_browser(BrowserMode::Files);
                    if let Some(manager) = self.import_manager.as_mut() {
                        manager.error =
                            Some(format!("{error} The Flux Browser is available below."));
                    }
                }
            },
            Message::ImportNativeFolderPicked(_, result) => match result {
                Ok(Some(folder)) => {
                    if let Some(manager) = self.import_manager.as_mut() {
                        manager.browser = None;
                        manager.sources = vec![folder];
                        manager.error = None;
                    }
                    return self.start_import_scan();
                }
                Ok(None) => {}
                Err(error) => {
                    self.open_import_browser(BrowserMode::Folder);
                    if let Some(manager) = self.import_manager.as_mut() {
                        manager.error =
                            Some(format!("{error} The Flux Browser is available below."));
                    }
                }
            },
            Message::ImportBrowserOpenFiles => self.open_import_browser(BrowserMode::Files),
            Message::ImportBrowserOpenFolder => self.open_import_browser(BrowserMode::Folder),
            Message::ImportBrowserUp => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                {
                    browser_up(browser);
                }
            }
            Message::ImportBrowserEnter(path) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                {
                    browser_enter(browser, path);
                }
            }
            Message::ImportBrowserToggle(path) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                {
                    if browser.selected.contains(&path) {
                        browser.selected.remove(&path);
                    } else if browser.mode == BrowserMode::Files {
                        browser.selected.insert(path);
                    }
                }
            }
            Message::ImportBrowserSelectAll => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                    && browser.mode == BrowserMode::Files
                {
                    browser.selected = browser
                        .entries
                        .iter()
                        .filter(|entry| !entry.is_dir && entry.supported)
                        .map(|entry| entry.path.clone())
                        .collect();
                }
            }
            Message::ImportBrowserConfirm => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(browser) = manager.browser.as_mut() else {
                    return Task::none();
                };
                let (sources, mode) = match browser.mode {
                    BrowserMode::Files => {
                        let mut sources: Vec<PathBuf> = browser.selected.iter().cloned().collect();
                        sources.sort();
                        if sources.is_empty() {
                            browser.error =
                                Some("Select at least one supported audio file.".to_owned());
                            return Task::none();
                        }
                        (sources, BrowserMode::Files)
                    }
                    BrowserMode::Folder => (vec![browser.current.clone()], BrowserMode::Folder),
                };
                return self.start_import_from_selection(sources, mode);
            }
            Message::ImportBrowserCancel => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.browser = None;
                }
            }
            Message::ImportBrowserRefresh => self.refresh_import_browser(),
            Message::ImportBrowserPathChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                {
                    browser.path_input = value;
                }
            }
            Message::ImportBrowserPathSubmit => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(browser) = manager.browser.as_mut()
                {
                    let path = PathBuf::from(browser.path_input.trim());
                    if path.is_dir() {
                        browser_enter(browser, path);
                    } else {
                        browser.error = Some("That path is not a directory.".to_owned());
                    }
                }
            }
            Message::ImportRecursiveToggled => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.recursive = !manager.recursive;
                }
            }
            Message::ImportPlanReady(_, plan) => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                manager.progress_view = manager
                    .progress
                    .lock()
                    .map(|progress| progress.clone())
                    .unwrap_or_default();
                if plan.cancelled {
                    manager.stage = ImportUiStage::Cancelled;
                    manager.progress_view.stage = ImportStage::Cancelled;
                    manager.progress_view.message =
                        "Import cancelled; no catalogue changes were made.".to_owned();
                    manager.plan = Some(*plan);
                    return Task::none();
                }
                let empty = plan.candidates.is_empty();
                manager.plan = Some(*plan);
                manager.stage = if empty {
                    ImportUiStage::Failed
                } else {
                    ImportUiStage::Review
                };
                if empty {
                    manager.error = Some(
                        "No importable audio files were found. Check the issue list and try \
                         another selection."
                            .to_owned(),
                    );
                } else {
                    manager.select_candidate(0);
                }
            }
            Message::ImportPrepared(_, prepared) => self.apply_import_review(*prepared),
            Message::ImportSelect(index) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.select_candidate(index);
                }
            }
            Message::ImportToggleChecked(index) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager
                        .plan
                        .as_ref()
                        .is_some_and(|plan| index < plan.candidates.len())
                    && !manager.checked.remove(&index)
                {
                    manager.checked.insert(index);
                }
            }
            Message::ImportCheckAll => {
                if let Some(manager) = self.import_manager.as_mut()
                    && let Some(plan) = manager.plan.as_ref()
                {
                    manager.checked = (0..plan.candidates.len()).collect();
                }
            }
            Message::ImportClearChecked => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.checked.clear();
                }
            }
            Message::ImportBulkArtistPicked(choice) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.bulk_artist = choice;
                }
            }
            Message::ImportBulkApplyArtist => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(choice) = manager.bulk_artist.clone() else {
                    manager.asset_error = Some("Select an existing Artist to apply.".to_owned());
                    return Task::none();
                };
                let Some(artist_id) = choice.id else {
                    manager.asset_error = Some(
                        "Bulk Artist assignment requires a stable existing Artist.".to_owned(),
                    );
                    return Task::none();
                };
                let name = self
                    .controller
                    .artist(artist_id)
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or(choice.label);
                let indices = manager.checked_indices();
                if indices.is_empty() {
                    manager.asset_error = Some("Select one or more candidates first.".to_owned());
                    return Task::none();
                }
                if manager.plan.as_ref().is_some_and(|plan| {
                    indices.iter().any(|index| {
                        plan.candidates.get(*index).is_some_and(|candidate| {
                            candidate.decision() == ImportDecision::UpdateExisting
                        })
                    })
                }) {
                    manager.asset_error = Some(
                        "Bulk Artist assignment cannot change an existing Track's physical owner; update that candidate individually or skip it."
                            .to_owned(),
                    );
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut() {
                    plan.apply_artist_to(&indices, artist_id, &name);
                }
                manager.asset_error = None;
                if let Some(selected) = manager.selected {
                    manager.select_candidate(selected);
                }
            }
            Message::ImportBulkNewArtistChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.bulk_new_artist_name = value;
                }
            }
            Message::ImportBulkApplyNewArtist => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let name = manager.bulk_new_artist_name.trim().to_owned();
                if name.is_empty() {
                    manager.asset_error = Some("Enter an Artist name to apply.".to_owned());
                    return Task::none();
                }
                let indices = manager.checked_indices();
                if indices.is_empty() {
                    manager.asset_error = Some("Select one or more candidates first.".to_owned());
                    return Task::none();
                }
                if manager.plan.as_ref().is_some_and(|plan| {
                    indices.iter().any(|index| {
                        plan.candidates.get(*index).is_some_and(|candidate| {
                            candidate.decision() == ImportDecision::UpdateExisting
                        })
                    })
                }) {
                    manager.asset_error = Some(
                        "Bulk Artist assignment cannot change an existing Track's physical owner; update that candidate individually or skip it."
                            .to_owned(),
                    );
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut() {
                    plan.apply_artist_name_to(&indices, &name);
                }
                manager.asset_error = None;
                if let Some(selected) = manager.selected {
                    manager.select_candidate(selected);
                }
            }
            Message::ImportBulkAlbumChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.bulk_album_title = value;
                }
            }
            Message::ImportBulkApplyAlbum => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let indices = manager.checked_indices();
                if indices.is_empty() {
                    manager.asset_error = Some("Select one or more candidates first.".to_owned());
                    return Task::none();
                }
                let title = manager.bulk_album_title.trim().to_owned();
                if let Some(plan) = manager.plan.as_mut() {
                    plan.apply_album_to(&indices, (!title.is_empty()).then_some(title.as_str()));
                }
                manager.asset_error = None;
                if let Some(selected) = manager.selected {
                    manager.select_candidate(selected);
                }
            }
            Message::ImportBulkMakeStandalone => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let indices = manager.checked_indices();
                if indices.is_empty() {
                    manager.asset_error = Some("Select one or more candidates first.".to_owned());
                    return Task::none();
                }
                if manager.plan.as_ref().is_some_and(|plan| {
                    indices.iter().any(|index| {
                        plan.candidates.get(*index).is_some_and(|candidate| {
                            candidate.decision() == ImportDecision::UpdateExisting
                        })
                    })
                }) {
                    manager.asset_error = Some(
                        "Bulk Album membership does not rewrite existing Tracks. Use the Track editor move workflow for those candidates."
                            .to_owned(),
                    );
                    return Task::none();
                }
                manager.bulk_album_title.clear();
                if let Some(plan) = manager.plan.as_mut() {
                    plan.apply_album_to(&indices, None);
                }
                manager.asset_error = None;
                if let Some(selected) = manager.selected {
                    manager.select_candidate(selected);
                }
            }
            Message::ImportBulkAlbumArtworkChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.bulk_album_artwork_path = value;
                    manager.asset_error = None;
                }
            }
            Message::ImportBulkAlbumArtworkChoose => {
                let start = self.controller.canonical_root().map(Path::to_path_buf);
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::ImportBulkAlbumArtworkPicked(session, result),
                );
            }
            Message::ImportBulkAlbumArtworkPicked(_, result) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    match result {
                        Ok(Some(paths)) => {
                            if let Some(path) = paths.first() {
                                manager.bulk_album_artwork_path = path.display().to_string();
                                manager.asset_error = None;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportBulkApplyAlbumArtwork => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let indices = manager.checked_indices();
                if indices.is_empty() {
                    manager.asset_error = Some("Select one or more candidates first.".to_owned());
                    return Task::none();
                }
                let path = PathBuf::from(manager.bulk_album_artwork_path.trim());
                if path.as_os_str().is_empty() {
                    manager.asset_error = Some("Choose an Album artwork file.".to_owned());
                    return Task::none();
                }
                if let Err(error) = decode_artwork(&artwork_key(
                    ArtworkOwner::Album(AlbumId::new(0)),
                    path.clone(),
                )) {
                    manager.asset_error = Some(format!("Album artwork rejected: {error}"));
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut() {
                    match plan.apply_album_artwork_from_path(&indices, &path) {
                        Ok(()) => {
                            manager.asset_error = None;
                            if let Some(selected) = manager.selected {
                                manager.select_candidate(selected);
                            }
                        }
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportBulkClearAlbumArtwork => {
                if let Some(manager) = self.import_manager.as_mut() {
                    let indices = manager.checked_indices();
                    if indices.is_empty() {
                        manager.asset_error =
                            Some("Select one or more candidates first.".to_owned());
                    } else {
                        if let Some(plan) = manager.plan.as_mut() {
                            plan.clear_album_artwork_for(&indices);
                        }
                        manager.asset_error = None;
                        if let Some(selected) = manager.selected {
                            manager.select_candidate(selected);
                        }
                    }
                }
            }
            Message::ImportDecisionChanged(index, decision) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    candidate.set_decision(decision);
                }
            }
            Message::ImportBulkSkip => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                {
                    plan.bulk_skip();
                }
            }
            Message::ImportBulkImport => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                {
                    plan.bulk_import_separate();
                }
            }
            Message::ImportBulkUpdate => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                {
                    plan.bulk_update_existing();
                }
            }
            Message::ImportTitleChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    manager.edit_title = value;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.set_title(manager.edit_title.clone());
                    }
                }
            }
            Message::ImportArtistChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    manager.edit_artist = value;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.set_artist_id(None, None);
                        candidate.set_artist_name(nonempty_string(&manager.edit_artist));
                    }
                }
            }
            Message::ImportAlbumChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    manager.edit_album = value;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.set_album(nonempty_string(&manager.edit_album));
                        if candidate.album().is_none() {
                            candidate.clear_album_artwork();
                            manager.edit_album_artwork.clear();
                        }
                    }
                }
            }
            Message::ImportAlbumPicked(choice) => {
                let Some(album_id) = choice.as_ref().map(|choice| choice.id) else {
                    return Task::none();
                };
                let Some(album) = self.controller.album(album_id) else {
                    return Task::none();
                };
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                    && let Some(index) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    if candidate.artist_id() != Some(album.owner_artist_id) {
                        manager.asset_error = Some(
                            "That existing Album belongs to a different owner Artist. Select that Artist first."
                                .to_owned(),
                        );
                        return Task::none();
                    }
                    candidate.set_album(Some(album.title.clone()));
                    manager.edit_album = album.title;
                    manager.asset_error = None;
                }
            }
            Message::ImportTrackNumberChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    manager.edit_track_number = value;
                    match parse_optional_number(&manager.edit_track_number, "track number") {
                        Ok(number) => {
                            manager.number_error = None;
                            if let Some(plan) = manager.plan.as_mut()
                                && let Some(index) = manager.selected
                                && let Some(candidate) = plan.candidates.get_mut(index)
                            {
                                candidate.set_track_number(number);
                            }
                        }
                        Err(error) => manager.number_error = Some(error),
                    }
                }
            }
            Message::ImportDiscNumberChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    manager.edit_disc_number = value;
                    match parse_optional_number(&manager.edit_disc_number, "disc number") {
                        Ok(number) => {
                            manager.number_error = None;
                            if let Some(plan) = manager.plan.as_mut()
                                && let Some(index) = manager.selected
                                && let Some(candidate) = plan.candidates.get_mut(index)
                            {
                                candidate.set_disc_number(number);
                            }
                        }
                        Err(error) => manager.number_error = Some(error),
                    }
                }
            }
            Message::ImportArtistPicked(choice) => {
                let artist_id = choice.as_ref().and_then(|choice| choice.id);
                let canonical_name = artist_id
                    .and_then(|id| self.controller.artist(id))
                    .map(|artist| artist.name().to_owned());
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                {
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.set_artist_id(artist_id, canonical_name.clone());
                    }
                    if let Some(name) = canonical_name {
                        manager.edit_artist = name;
                    }
                }
            }
            Message::ImportCreditNameChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_credit = value;
                }
            }
            Message::ImportCreditAddNamed => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let name = manager.edit_credit.trim().to_owned();
                if name.is_empty() {
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut()
                    && let Some(index) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    candidate.add_credit(flux_app::ImportCredit::new(None, name, false));
                    manager.edit_credit.clear();
                }
            }
            Message::ImportCreditAddExisting(choice) => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(id) = choice.id else {
                    return Task::none();
                };
                if let Some(plan) = manager.plan.as_mut()
                    && let Some(index) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    candidate.add_credit(flux_app::ImportCredit::new(
                        Some(id),
                        choice.label,
                        false,
                    ));
                }
            }
            Message::ImportCreditMove(index, delta) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                    && let Some(selected) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(selected)
                {
                    candidate.move_credit(index, delta);
                }
            }
            Message::ImportCreditRemove(index) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                    && let Some(selected) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(selected)
                {
                    candidate.remove_credit(index);
                }
            }
            Message::ImportCreditSetOwner(index) => {
                if let Some(manager) = self.import_manager.as_mut()
                    && manager.stage == ImportUiStage::Review
                    && let Some(plan) = manager.plan.as_mut()
                    && let Some(selected) = manager.selected
                    && let Some(candidate) = plan.candidates.get_mut(selected)
                {
                    candidate.set_owner_index(index);
                    manager.edit_artist = candidate.artist_name().unwrap_or_default().to_owned();
                }
            }
            Message::ImportArtworkChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_artwork = value;
                    manager.asset_error = None;
                }
            }
            Message::ImportArtworkChoose => {
                let start = self.controller.canonical_root().map(Path::to_path_buf);
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::ImportArtworkPicked(session, result),
                );
            }
            Message::ImportArtworkPicked(_, result) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    match result {
                        Ok(Some(paths)) => {
                            if let Some(path) = paths.first() {
                                manager.edit_artwork = path.display().to_string();
                                manager.asset_error = None;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportArtworkApply => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(index) = manager.selected else {
                    return Task::none();
                };
                let path = PathBuf::from(manager.edit_artwork.trim());
                if path.as_os_str().is_empty() {
                    manager.asset_error = Some("Choose a Track artwork file.".to_owned());
                    return Task::none();
                }
                if let Err(error) = decode_artwork(&artwork_key(
                    ArtworkOwner::Track(TrackId::new(0)),
                    path.clone(),
                )) {
                    manager.asset_error = Some(format!("Track artwork rejected: {error}"));
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut()
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    match candidate.set_artwork(&path) {
                        Ok(()) => {
                            manager.asset_error = None;
                            manager.edit_artwork = path.display().to_string();
                        }
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportArtworkClear => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_artwork.clear();
                    manager.asset_error = None;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.clear_artwork();
                    }
                }
            }
            Message::ImportAlbumArtworkChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_album_artwork = value;
                    manager.asset_error = None;
                }
            }
            Message::ImportAlbumArtworkChoose => {
                let start = self.controller.canonical_root().map(Path::to_path_buf);
                let session = self.controller.library_session();
                return Task::perform(
                    async move { flux_platform::pick_image_files_in(start.as_deref()) },
                    move |result| Message::ImportAlbumArtworkPicked(session, result),
                );
            }
            Message::ImportAlbumArtworkPicked(_, result) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    match result {
                        Ok(Some(paths)) => {
                            if let Some(path) = paths.first() {
                                manager.edit_album_artwork = path.display().to_string();
                                manager.asset_error = None;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportAlbumArtworkApply => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(index) = manager.selected else {
                    return Task::none();
                };
                let path = PathBuf::from(manager.edit_album_artwork.trim());
                if path.as_os_str().is_empty() {
                    manager.asset_error = Some("Choose an Album artwork file.".to_owned());
                    return Task::none();
                }
                if manager
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.candidates.get(index))
                    .is_some_and(|candidate| candidate.album().is_none())
                {
                    manager.asset_error = Some(
                        "Attach the candidate to an Album before applying Album artwork."
                            .to_owned(),
                    );
                    return Task::none();
                }
                let album = manager.plan.as_ref().map_or(AlbumId::new(0), |plan| {
                    plan.target_album
                        .as_ref()
                        .map_or(AlbumId::new(0), |target| target.album_id)
                });
                if let Err(error) =
                    decode_artwork(&artwork_key(ArtworkOwner::Album(album), path.clone()))
                {
                    manager.asset_error = Some(format!("Album artwork rejected: {error}"));
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut()
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    match candidate.set_album_artwork(&path) {
                        Ok(()) => {
                            manager.asset_error = None;
                            manager.edit_album_artwork = path.display().to_string();
                        }
                        Err(error) => manager.asset_error = Some(error),
                    }
                }
            }
            Message::ImportAlbumArtworkClear => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_album_artwork.clear();
                    manager.asset_error = None;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.clear_album_artwork();
                    }
                }
            }
            Message::ImportLyricsPathChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_lyrics_path = value;
                    manager.asset_error = None;
                }
            }
            Message::ImportLyricsAttach => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                let Some(index) = manager.selected else {
                    return Task::none();
                };
                let path = PathBuf::from(manager.edit_lyrics_path.trim());
                if path.as_os_str().is_empty() {
                    manager.asset_error = Some("Choose an LRC or text lyrics file.".to_owned());
                    return Task::none();
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        manager.asset_error = Some(format!("Could not read lyrics file: {error}"));
                        return Task::none();
                    }
                };
                if text.trim().is_empty() {
                    manager.asset_error = Some("Lyrics file is empty.".to_owned());
                    return Task::none();
                }
                if let Err(error) = LyricsDocument::parse(&text) {
                    manager.asset_error = Some(format!("Lyrics rejected: {error}"));
                    return Task::none();
                }
                if let Some(plan) = manager.plan.as_mut()
                    && let Some(candidate) = plan.candidates.get_mut(index)
                {
                    candidate.set_lyrics_text(Some(text));
                    manager.asset_error = None;
                }
            }
            Message::ImportLyricsClear => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.edit_lyrics_path.clear();
                    manager.asset_error = None;
                    if let Some(plan) = manager.plan.as_mut()
                        && let Some(index) = manager.selected
                        && let Some(candidate) = plan.candidates.get_mut(index)
                    {
                        candidate.set_lyrics_text(None);
                    }
                }
            }
            Message::ImportManualChanged(value) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.manual_input = value;
                }
            }
            Message::ImportManualAdd => {
                let Some(manager) = self.import_manager.as_mut() else {
                    return Task::none();
                };
                if !matches!(manager.stage, ImportUiStage::Choose | ImportUiStage::Failed) {
                    manager.error = Some("Finish or cancel the current review first.".to_owned());
                    return Task::none();
                }
                let paths: Vec<PathBuf> = manager
                    .manual_input
                    .split(';')
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .collect();
                if paths.is_empty() {
                    manager.error = Some("Enter at least one path.".to_owned());
                } else {
                    manager.sources.extend(paths);
                    manager.manual_input.clear();
                    manager.error = None;
                    return self.start_import_scan();
                }
            }
            Message::ImportDropPath(path) => return self.queue_external_drop(path),
            Message::ImportFileHovered => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.drop_hover = true;
                } else {
                    self.external_drag_hover = true;
                }
            }
            Message::ImportFileHoverLeft => {
                self.external_drag_hover = false;
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.drop_hover = false;
                }
            }
            Message::ImportApply => return self.start_import_copy(),
            Message::ImportCancel => {
                if let Some(manager) = self.import_manager.as_mut() {
                    if matches!(
                        manager.stage,
                        ImportUiStage::Scanning | ImportUiStage::Importing
                    ) {
                        manager.cancel.store(true, Ordering::Release);
                        manager.progress_view.message =
                            "Cancelling at the next safe point…".to_owned();
                    } else {
                        self.import_manager = None;
                    }
                }
            }
            Message::OpenLibraryPressed => {
                let path = PathBuf::from(self.lib_input.trim());
                if self.controller.switch_to_canonical(&path).is_ok() {
                    self.after_switch();
                }
            }
            Message::CreateLibraryPressed => {
                let path = PathBuf::from(self.lib_input.trim());
                if self
                    .controller
                    .switch_to_new_canonical_library(&path)
                    .is_ok()
                {
                    self.after_switch();
                }
            }
            Message::UseLegacyIndexPressed => {
                self.lifecycle_msg = Some(
                    "Legacy runtime switching is disabled. Use the migration workflow to \
                     carry an existing index into a canonical library."
                        .to_owned(),
                );
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
                match flux_app::plan_canonical_migration(&source, &destination) {
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
                    let destination = plan.destination().to_path_buf();
                    match flux_app::execute_canonical_migration(&plan) {
                        Ok(report) => {
                            self.migrated_dest = Some(destination);
                            self.lifecycle_msg = Some(format!(
                                "Canonical migration complete: {} track(s), {} artist(s), \
                                 {} album(s) validated. The source is untouched; choose \
                                 “Open migrated library” to activate the result.",
                                report.tracks(),
                                report.artists(),
                                report.albums(),
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
                    && self.controller.switch_to_canonical(&destination).is_ok()
                {
                    self.after_switch();
                }
            }
            Message::ReconcilePressed => {
                if self.controller.storage_mode() == StorageMode::Canonical {
                    self.controller.refresh();
                    self.last_reconcile = None;
                    self.lifecycle_msg = Some(
                        "Canonical library refreshed; manifest, catalogue and resource paths validated."
                            .to_owned(),
                    );
                } else if let Ok(report) = self.controller.reconcile_portable_library() {
                    self.lifecycle_msg = Some(reconcile_summary(&report));
                    self.last_reconcile = Some(report);
                }
            }
            Message::RefreshPressed => self.controller.refresh(),
            Message::SearchChanged(value) => {
                self.search = value;
                self.page = 0;
                self.selected_tracks.clear();
                self.focused_track = None;
            }
            Message::ClearSearch => {
                self.search.clear();
                self.page = 0;
                self.selected_tracks.clear();
                self.focused_track = None;
            }
            Message::SearchNavigateArtist(id) => {
                if let Some(artist) = self.controller.artist(id) {
                    self.view = LibraryView::Artists;
                    self.selected_group =
                        Some(format!("{}{ARTIST_KEY_SEPARATOR}{id}", artist.name()));
                    self.selected_playlist = None;
                    self.selected_lyrics = None;
                    self.lyrics_follow_current = false;
                    self.search.clear();
                    self.selected_tracks.clear();
                    self.focused_track = None;
                    self.sync_artist_editor();
                    self.page = 0;
                    self.show_setup = false;
                }
            }
            Message::SearchNavigateAlbum(id) => {
                if let Some(album) = self.controller.album(id) {
                    self.view = LibraryView::Albums;
                    self.selected_playlist = None;
                    self.selected_lyrics = None;
                    self.lyrics_follow_current = false;
                    self.selected_group = Some(album_key_for_title(id, &album.title));
                    self.search.clear();
                    self.selected_tracks.clear();
                    self.focused_track = None;
                    self.sync_album_editor();
                    self.page = 0;
                    self.show_setup = false;
                }
            }
            Message::OpenAlbum(id) => {
                return self.update(Message::SearchNavigateAlbum(id));
            }
            Message::OpenArtist(id) => {
                return self.update(Message::SearchNavigateArtist(id));
            }
            Message::SearchNavigateTrack(id) => {
                if self.controller.track(id).is_some() {
                    let index = self
                        .controller
                        .tracks()
                        .iter()
                        .position(|(track_id, _)| *track_id == id)
                        .unwrap_or(0);
                    self.view = LibraryView::Tracks;
                    self.selected_playlist = None;
                    self.selected_lyrics = None;
                    self.lyrics_follow_current = false;
                    self.selected_group = None;
                    self.search.clear();
                    self.focused_track = Some(id);
                    self.selected_tracks.clear();
                    self.filter_favorites = false;
                    self.filter_lyrics = TriFilter::Any;
                    self.filter_artwork = TriFilter::Any;
                    self.filter_release = ReleaseFilter::Any;
                    self.sort_tracks = TrackSort::Default;
                    self.page = index / TRACK_PAGE_SIZE;
                    self.show_setup = false;
                }
            }
            Message::TrackSelectionToggle(id) => {
                if self.controller.track(id).is_some() {
                    if let Some(index) = self
                        .selected_tracks
                        .iter()
                        .position(|selected| *selected == id)
                    {
                        self.selected_tracks.remove(index);
                    } else {
                        self.selected_tracks.push(id);
                    }
                }
            }
            Message::TrackSelectionSelectAll => {
                let visible = self.visible_track_ids();
                self.selected_tracks = visible;
            }
            Message::TrackSelectionClear => self.selected_tracks.clear(),
            Message::FilterFavoritesToggled => {
                self.filter_favorites = !self.filter_favorites;
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::FilterLyricsCycle => {
                self.filter_lyrics = self.filter_lyrics.cycle();
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::FilterArtworkCycle => {
                self.filter_artwork = self.filter_artwork.cycle();
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::FilterReleaseChanged(release) => {
                self.filter_release = release;
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::SortTracksChanged(sort) => {
                self.sort_tracks = sort;
                self.page = 0;
            }
            Message::ResetLibraryFilters => {
                self.filter_favorites = false;
                self.filter_lyrics = TriFilter::Any;
                self.filter_artwork = TriFilter::Any;
                self.filter_release = ReleaseFilter::Any;
                self.filter_rating = RatingFilter::Any;
                self.sort_tracks = TrackSort::Default;
                self.sort_descending = false;
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::BatchPlaylistAdd(playlist_id) => {
                if let Some(playlist) = self
                    .controller
                    .playlists()
                    .into_iter()
                    .find(|playlist| playlist.id == playlist_id)
                {
                    self.batch_review = Some(BatchReview {
                        playlist_id: Some(playlist_id),
                        playlist_label: playlist.name,
                        expected_playlist_track_ids: Some(playlist.track_ids),
                        new_playlist_name: String::new(),
                        track_ids: self.selected_tracks.clone(),
                        error: None,
                    });
                }
            }
            Message::BatchPlaylistCreateOpen => {
                self.batch_review = Some(BatchReview {
                    playlist_id: None,
                    playlist_label: String::new(),
                    expected_playlist_track_ids: None,
                    new_playlist_name: String::new(),
                    track_ids: self.selected_tracks.clone(),
                    error: None,
                });
            }
            Message::BatchPlaylistNameChanged(value) => {
                if let Some(review) = self.batch_review.as_mut() {
                    review.new_playlist_name = value;
                }
            }
            Message::BatchReviewCancel => self.batch_review = None,
            Message::BatchReviewConfirm => self.confirm_batch_review(),
            Message::BatchFavorites(favorite) => {
                let ids = self.selected_tracks.clone();
                match self.controller.set_favorites(&ids, favorite) {
                    Ok(count) => {
                        self.selected_tracks.clear();
                        self.lifecycle_msg = Some(format!(
                            "{} track(s) marked {}favorite.",
                            count,
                            if favorite { "" } else { "not " }
                        ));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::PlaylistSelect(id) => {
                if self.controller.playlist(id).cloned().is_some() {
                    self.view = LibraryView::Playlists;
                    self.selected_playlist = Some(id);
                    self.selected_group = None;
                    self.search.clear();
                    self.selected_tracks.clear();
                    self.playlist_entry_selection.clear();
                    self.playlist_delete_confirm = false;
                    self.playlist_clear_confirm = false;
                    self.playlist_remove_confirm = false;
                    self.playlist_drag = None;
                    self.playlist_error = None;
                    self.show_setup = false;
                    self.page = 0;
                    if let Some(playlist) = self.controller.playlist(id).cloned() {
                        self.playlist_name_input = playlist.name.clone();
                    }
                }
            }
            Message::PlaylistBack => {
                self.selected_playlist = None;
                self.playlist_entry_selection.clear();
                self.playlist_drag = None;
                self.playlist_create_open = false;
                self.playlist_error = None;
            }
            Message::PlaylistCreateOpen => {
                self.playlist_create_open = true;
                self.playlist_create_name.clear();
                self.playlist_error = None;
            }
            Message::PlaylistCreateNameChanged(value) => self.playlist_create_name = value,
            Message::PlaylistCreateConfirm => {
                let name = self.playlist_create_name.trim().to_owned();
                if name.is_empty() {
                    self.playlist_error = Some("Playlist name must not be empty.".to_owned());
                    return Task::none();
                }
                match self.controller.create_playlist(&name) {
                    Ok(id) => {
                        self.playlist_create_open = false;
                        self.playlist_create_name.clear();
                        self.view = LibraryView::Playlists;
                        self.selected_playlist = Some(id);
                        self.playlist_name_input = name;
                        self.playlist_error = None;
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistCreateCancel => self.playlist_create_open = false,
            Message::PlaylistRenameChanged(value) => self.playlist_name_input = value,
            Message::PlaylistRenameSave => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                let name = self.playlist_name_input.trim().to_owned();
                if name.is_empty() {
                    self.playlist_error = Some("Playlist name must not be empty.".to_owned());
                    return Task::none();
                }
                match self.controller.rename_playlist(
                    id,
                    &name,
                    &playlist.name,
                    &playlist.track_ids,
                ) {
                    Ok(()) => {
                        self.playlist_error = None;
                        self.lifecycle_msg = Some(format!("Renamed playlist {id}."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistPlay => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                match self.controller.play_playlist(id, 0) {
                    Ok(()) => {
                        self.follow_queue_current();
                        self.lifecycle_msg = Some(format!("Playing playlist {id}."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistPlayEntry(position) => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                match self.controller.play_playlist(id, position) {
                    Ok(()) => {
                        self.follow_queue_current();
                        self.lifecycle_msg =
                            Some(format!("Playing playlist entry {}.", position + 1));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistShuffle => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                match self.controller.play_playlist_shuffled(id) {
                    Ok(()) => {
                        self.follow_queue_current();
                        self.lifecycle_msg = Some(format!(
                            "Playing playlist {id} shuffled; canonical order unchanged."
                        ));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistEnqueue => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                match self.controller.enqueue_playlist(id) {
                    Ok(count) => {
                        self.follow_queue_current();
                        self.lifecycle_msg =
                            Some(format!("Added {count} playlist entr(ies) to the queue."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistEnqueueSelected => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                if self.playlist_entry_selection.is_empty() {
                    self.playlist_error = Some("Select at least one playlist entry.".to_owned());
                    return Task::none();
                }
                match self
                    .controller
                    .enqueue_playlist_entries(id, &self.playlist_entry_selection)
                {
                    Ok(count) => {
                        self.follow_queue_current();
                        self.lifecycle_msg =
                            Some(format!("Added {count} selected entr(ies) to the queue."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistEnqueueEntry(position) => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                match self.controller.enqueue_playlist_entries(id, &[position]) {
                    Ok(_) => {
                        self.follow_queue_current();
                        self.lifecycle_msg = Some("Added playlist entry to the queue.".to_owned());
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistEntryToggle(position) => {
                self.playlist_remove_confirm = false;
                if let Some(index) = self
                    .playlist_entry_selection
                    .iter()
                    .position(|selected| *selected == position)
                {
                    self.playlist_entry_selection.remove(index);
                } else {
                    self.playlist_entry_selection.push(position);
                }
            }
            Message::PlaylistEntrySelectAll => {
                if let Some(id) = self.selected_playlist
                    && let Some(playlist) = self.controller.playlist(id).cloned()
                {
                    self.playlist_entry_selection = (0..playlist.track_ids.len()).collect();
                    self.playlist_remove_confirm = false;
                }
            }
            Message::PlaylistEntryClearSelection => {
                self.playlist_entry_selection.clear();
                self.playlist_remove_confirm = false;
            }
            Message::PlaylistEntryRemove(position) => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                match self
                    .controller
                    .remove_playlist_entries(id, &[position], &playlist.track_ids)
                {
                    Ok(_) => {
                        self.playlist_entry_selection.clear();
                        self.playlist_error = None;
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistRemoveSelected => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                if self.playlist_entry_selection.is_empty() {
                    self.playlist_error = Some("Select at least one playlist entry.".to_owned());
                    return Task::none();
                }
                if !self.playlist_remove_confirm {
                    self.playlist_remove_confirm = true;
                    return Task::none();
                }
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                match self.controller.remove_playlist_entries(
                    id,
                    &self.playlist_entry_selection,
                    &playlist.track_ids,
                ) {
                    Ok(count) => {
                        self.playlist_entry_selection.clear();
                        self.playlist_remove_confirm = false;
                        self.playlist_error = None;
                        self.lifecycle_msg = Some(format!("Removed {count} playlist entr(ies)."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistEntryMove(position, delta) => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                let target = position as isize + delta;
                if target < 0 || target as usize >= playlist.track_ids.len() {
                    return Task::none();
                }
                match self.controller.reorder_playlist_entry(
                    id,
                    position,
                    target as usize,
                    &playlist.track_ids,
                ) {
                    Ok(()) => {
                        self.playlist_entry_selection.clear();
                        self.playlist_error = None;
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistClear => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                if !self.playlist_clear_confirm {
                    self.playlist_clear_confirm = true;
                    return Task::none();
                }
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                match self.controller.clear_playlist(id, &playlist.track_ids) {
                    Ok(()) => {
                        self.playlist_clear_confirm = false;
                        self.playlist_entry_selection.clear();
                        self.playlist_error = None;
                        self.lifecycle_msg =
                            Some(format!("Cleared playlist {id}; tracks were not deleted."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistDelete => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                if !self.playlist_delete_confirm {
                    self.playlist_delete_confirm = true;
                    return Task::none();
                }
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    self.playlist_error = Some("The playlist is no longer available.".to_owned());
                    return Task::none();
                };
                match self
                    .controller
                    .delete_playlist(id, &playlist.name, &playlist.track_ids)
                {
                    Ok(()) => {
                        self.selected_playlist = None;
                        self.playlist_entry_selection.clear();
                        self.playlist_delete_confirm = false;
                        self.playlist_error = None;
                        self.lifecycle_msg =
                            Some(format!("Deleted playlist {id}; no tracks were removed."));
                    }
                    Err(error) => self.playlist_error = Some(error),
                }
            }
            Message::PlaylistDragStart(position) => {
                let Some(id) = self.selected_playlist else {
                    return Task::none();
                };
                let Some(playlist) = self.controller.playlist(id).cloned() else {
                    return Task::none();
                };
                if position < playlist.track_ids.len() {
                    self.playlist_drag = Some(PlaylistDrag {
                        playlist_id: id,
                        from: position,
                        expected_track_ids: playlist.track_ids,
                        target_gap: None,
                        moved: false,
                    });
                }
            }
            Message::PlaylistDragHover(gap) => {
                if let Some(drag) = self.playlist_drag.as_mut() {
                    drag.target_gap = Some(gap);
                    drag.moved = true;
                }
            }
            Message::PlaylistDragLeave => {
                if let Some(drag) = self.playlist_drag.as_mut() {
                    drag.target_gap = None;
                    drag.moved = true;
                }
            }
            Message::LyricsSelect(id) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && editor.dirty
                    && !editor.discard_confirm
                {
                    editor.discard_confirm = true;
                    editor.error = Some(
                        "Save or cancel the current lyrics edit before opening another track."
                            .to_owned(),
                    );
                    return Task::none();
                }
                if self.controller.track(id).is_some() {
                    self.view = LibraryView::Lyrics;
                    self.selected_lyrics = Some(id);
                    self.selected_playlist = None;
                    self.lyrics_mode = LyricsMode::Read;
                    self.lyrics_follow_current = false;
                    self.lyrics_editor = None;
                    self.lyrics_import = None;
                    self.lyrics_error = None;
                    self.lyrics_export_pending = None;
                    self.selected_group = None;
                    self.search.clear();
                    self.show_setup = false;
                    self.page = 0;
                }
            }
            Message::LyricsBack => {
                self.selected_lyrics = None;
                self.lyrics_mode = LyricsMode::Read;
                self.lyrics_follow_current = false;
                self.lyrics_editor = None;
                self.lyrics_import = None;
                self.lyrics_error = None;
                self.lyrics_export_pending = None;
            }
            Message::LyricsOpenCurrent => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && editor.dirty
                    && !editor.discard_confirm
                {
                    editor.discard_confirm = true;
                    editor.error = Some(
                        "Save or cancel the current lyrics edit before following playback."
                            .to_owned(),
                    );
                    return Task::none();
                }
                if let Some(id) = self.controller.current_id() {
                    self.view = LibraryView::Lyrics;
                    self.selected_lyrics = Some(id);
                    self.lyrics_mode = LyricsMode::Read;
                    self.lyrics_follow_current = true;
                    self.lyrics_editor = None;
                    self.lyrics_import = None;
                    self.lyrics_error = None;
                } else {
                    self.lifecycle_msg = Some("Nothing is playing.".to_owned());
                }
            }
            Message::LyricsFilterChanged(value) => {
                self.lyrics_filter = value;
                self.page = 0;
            }
            Message::LyricsModeChanged(mode) => {
                if mode == LyricsMode::Edit {
                    self.open_lyrics_editor();
                } else {
                    self.lyrics_mode = mode;
                    self.lyrics_follow_current = mode == LyricsMode::Karaoke
                        && self
                            .selected_lyrics
                            .is_some_and(|id| self.controller.current_id() == Some(id));
                }
            }
            Message::LyricsEditOpen => self.open_lyrics_editor(),
            Message::LyricsEditorTextChanged(index, value) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && let Some(line) = editor.lines.get_mut(index)
                {
                    line.text = value;
                    editor.dirty = true;
                    editor.discard_confirm = false;
                    editor.error = None;
                }
            }
            Message::LyricsEditorTimestampChanged(index, value) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && let Some(line) = editor.lines.get_mut(index)
                {
                    line.timestamp = value;
                    editor.dirty = true;
                    editor.discard_confirm = false;
                    editor.allow_plain_conversion = false;
                    editor.error = None;
                }
            }
            Message::LyricsEditorAddLine => {
                if let Some(editor) = self.lyrics_editor.as_mut() {
                    editor.lines.push(LyricsEditLine {
                        text: String::new(),
                        timestamp: String::new(),
                    });
                    editor.dirty = true;
                    editor.discard_confirm = false;
                }
            }
            Message::LyricsEditorRemoveLine(index) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && index < editor.lines.len()
                {
                    editor.lines.remove(index);
                    editor.dirty = true;
                    editor.discard_confirm = false;
                }
            }
            Message::LyricsEditorMoveLine(index, delta) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && index < editor.lines.len()
                {
                    let target = index as isize + delta;
                    if target >= 0 && (target as usize) < editor.lines.len() {
                        editor.lines.swap(index, target as usize);
                        editor.dirty = true;
                        editor.discard_confirm = false;
                    }
                }
            }
            Message::LyricsEditorSetCurrentTime(index) => {
                let Some(editor) = self.lyrics_editor.as_mut() else {
                    return Task::none();
                };
                if self.controller.current_id() != Some(editor.track_id) {
                    editor.error = Some(
                        "Play this track before assigning the current playback position."
                            .to_owned(),
                    );
                    return Task::none();
                }
                if let Some(line) = editor.lines.get_mut(index) {
                    line.timestamp =
                        format_timestamp_ms(self.controller.position().as_millis() as u64);
                    editor.dirty = true;
                    editor.discard_confirm = false;
                    editor.allow_plain_conversion = false;
                    editor.error = None;
                }
            }
            Message::LyricsEditorNudge(index, delta) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && let Some(line) = editor.lines.get_mut(index)
                {
                    let current = parse_editor_timestamp(&line.timestamp)
                        .ok()
                        .flatten()
                        .unwrap_or(0);
                    let adjusted = (i128::from(current) + i128::from(delta))
                        .clamp(0, i128::from(u64::MAX)) as u64;
                    line.timestamp = format_timestamp_ms(adjusted);
                    editor.dirty = true;
                    editor.discard_confirm = false;
                    editor.allow_plain_conversion = false;
                    editor.error = None;
                }
            }
            Message::LyricsEditorClearTimestamp(index) => {
                if let Some(editor) = self.lyrics_editor.as_mut()
                    && let Some(line) = editor.lines.get_mut(index)
                {
                    line.timestamp.clear();
                    editor.dirty = true;
                    editor.discard_confirm = false;
                    editor.allow_plain_conversion = false;
                    editor.error = None;
                }
            }
            Message::LyricsEditorSave => self.save_lyrics_editor(),
            Message::LyricsEditorCancel => self.cancel_lyrics_editor(),
            Message::LyricsEditorRemove => self.remove_lyrics_from_editor(),
            Message::LyricsImportOpen => return self.begin_lyrics_import(),
            Message::LyricsImportPicked(_, result) => match result {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.first() {
                        match std::fs::read_to_string(path) {
                            Ok(raw) => {
                                let expected = self
                                    .selected_lyrics
                                    .and_then(|id| self.controller.track_lyrics(id));
                                self.set_lyrics_import_preview(path.clone(), raw, expected);
                            }
                            Err(error) => {
                                self.lyrics_error =
                                    Some(format!("Could not read lyrics file: {error}"));
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => self.lyrics_error = Some(error),
            },
            Message::LyricsImportConfirm => self.confirm_lyrics_import(),
            Message::LyricsImportCancel => self.lyrics_import = None,
            Message::LyricsExportPathChanged(value) => {
                self.lyrics_export_path = value;
                self.lyrics_export_pending = None;
            }
            Message::LyricsExport(format) => self.export_lyrics(&format),
            Message::LyricsOpenAlbum(album_id) => {
                if let Some(album) = self.controller.album(album_id) {
                    self.view = LibraryView::Albums;
                    self.selected_playlist = None;
                    self.selected_lyrics = None;
                    self.lyrics_follow_current = false;
                    self.selected_group = Some(album_key_for_title(album_id, &album.title));
                    self.search.clear();
                    self.sync_album_editor();
                }
            }
            Message::LyricsOpenArtist(artist_id) => {
                if let Some(artist) = self.controller.artist(artist_id) {
                    self.view = LibraryView::Artists;
                    self.selected_playlist = None;
                    self.selected_lyrics = None;
                    self.lyrics_follow_current = false;
                    self.selected_group = Some(format!(
                        "{}{ARTIST_KEY_SEPARATOR}{artist_id}",
                        artist.name()
                    ));
                    self.search.clear();
                }
            }
            Message::SortDirectionToggled => {
                self.sort_descending = !self.sort_descending;
                self.page = 0;
            }
            Message::FilterRatingChanged(rating) => {
                self.filter_rating = rating;
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::CollectionSelect(collection) => {
                self.view = LibraryView::Collections;
                self.collection = Some(collection);
                self.selected_group = None;
                self.selected_playlist = None;
                self.selected_lyrics = None;
                self.search.clear();
                self.selected_tracks.clear();
                self.page = 0;
                self.show_setup = false;
            }
            Message::CollectionBack => {
                self.collection = None;
                self.page = 0;
                self.selected_tracks.clear();
            }
            Message::TrackFavoriteToggle(id) => match self.controller.toggle_track_favorite(id) {
                Ok(favorite) => {
                    *self.dashboard_cache.borrow_mut() = None;
                    self.lifecycle_msg = Some(format!(
                        "Track {id} marked {}favorite.",
                        if favorite { "" } else { "not " }
                    ));
                }
                Err(error) => self.lifecycle_msg = Some(error),
            },
            Message::TrackRatingSet(id, rating) => {
                match self.controller.set_track_rating(id, rating) {
                    Ok(()) => {
                        *self.dashboard_cache.borrow_mut() = None;
                        self.lifecycle_msg = Some(format!(
                            "Track {id} rating set to {}.",
                            rating.map_or_else(|| "none".to_owned(), |rating| rating.to_string())
                        ));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::BatchRating(rating) => {
                let ids = self.selected_tracks.clone();
                match self.controller.set_ratings_for_tracks(&ids, rating) {
                    Ok(count) => {
                        *self.dashboard_cache.borrow_mut() = None;
                        self.lifecycle_msg = Some(format!("Updated rating for {count} track(s)."));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            Message::SelectView(view) => {
                self.selected_tracks.clear();
                self.focused_track = None;
                self.select_view(view);
            }
            Message::SelectGroup(group) => {
                if group.is_none() {
                    self.search.clear();
                }
                self.selected_group = group;
                self.selected_playlist = None;
                self.selected_lyrics = None;
                self.lyrics_follow_current = false;
                self.selected_tracks.clear();
                self.focused_track = None;
                self.sync_artist_editor();
                self.sync_album_editor();
                self.page = 0;
                self.show_setup = false;
            }
            Message::ToggleSetup => self.toggle_setup(),
            Message::ToggleQueue => self.toggle_queue(),
            Message::PagePrev => {
                self.page = self.page.saturating_sub(1);
                if self.selected_playlist.is_some() {
                    self.playlist_entry_selection.clear();
                    self.playlist_remove_confirm = false;
                }
            }
            Message::PageNext => {
                self.page = self.page.saturating_add(1);
                if self.selected_playlist.is_some() {
                    self.playlist_entry_selection.clear();
                    self.playlist_remove_confirm = false;
                }
            }
            Message::QueuePagePrev => {
                self.cancel_queue_drag();
                self.queue_page = self.queue_page.saturating_sub(1);
            }
            Message::QueuePageNext => {
                self.cancel_queue_drag();
                self.queue_page = self.queue_page.saturating_add(1);
            }
            Message::PlayTrack(id) => {
                self.seek_preview = None;
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
            Message::QueueDragCancel => {
                self.cancel_queue_drag();
                self.playlist_drag = None;
                self.commit_seek_preview();
            }
            Message::SeekPreview(value) => {
                if let Some(total) = self.controller.current_duration()
                    && !total.is_zero()
                    && value.is_finite()
                {
                    self.seek_preview = Some(value.clamp(0.0, total.as_secs_f32()));
                }
            }
            Message::SeekCommit => self.commit_seek_preview(),
            Message::CrossfadeEnabledChanged(enabled) => {
                self.crossfade_preview = None;
                let _ = self.controller.set_crossfade_enabled(enabled);
            }
            Message::CrossfadeDurationPreview(value) => {
                if value.is_finite() {
                    let maximum = CROSSFADE_MAX_MS as f32 / 1_000.0;
                    let seconds = value.clamp(0.0, maximum);
                    self.crossfade_preview = Some((seconds * 1_000.0).round() as u64);
                }
            }
            Message::CrossfadeDurationCommit => {
                if let Some(milliseconds) = self.crossfade_preview.take() {
                    let _ = self.controller.set_crossfade_duration_ms(milliseconds);
                }
            }
            Message::QueueDragRelease => {
                self.finish_playlist_drag();
                self.finish_queue_drag();
                self.commit_seek_preview();
            }
            Message::TogglePlayPause => self.controller.toggle_play_pause(),
            Message::Stop => {
                self.seek_preview = None;
                self.controller.stop();
            }
            Message::RetryAudioOutput => {
                if self.controller.retry_audio_output() {
                    self.lifecycle_msg = Some(
                        "Retrying audio output; playback follows the current state.".to_owned(),
                    );
                } else {
                    self.lifecycle_msg =
                        Some("Audio output is not in a retryable failed state.".to_owned());
                }
            }
            Message::DismissNotice => {
                self.controller.dismiss_notice();
                self.lifecycle_msg = None;
            }
            Message::Next => {
                self.seek_preview = None;
                self.controller.next();
                self.follow_queue_current();
            }
            Message::Previous => {
                self.seek_preview = None;
                self.controller.previous();
                self.follow_queue_current();
            }
            Message::CycleRepeat => self.controller.cycle_repeat_mode(),
            Message::VolumeChanged(volume) => self.controller.set_volume(volume),
            Message::KeyPressed(key, modifiers) => {
                if self.batch_review.is_some() {
                    if matches!(key, Key::Named(Named::Escape)) {
                        self.batch_review = None;
                    }
                    return Task::none();
                }
                if self.album_transfer.is_some() {
                    if matches!(key, Key::Named(Named::Escape)) {
                        self.album_transfer = None;
                    }
                    return Task::none();
                }
                if self.deletion_manager.is_some() {
                    if matches!(key, Key::Named(Named::Escape)) {
                        self.deletion_manager = None;
                    }
                    return Task::none();
                }
                if self.import_manager.is_some() {
                    return self.handle_import_key(&key, &modifiers);
                }
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
                    Key::Character("q") => self.toggle_queue(),
                    Key::Character("e") => {
                        if let Some(id) = self.controller.current_id() {
                            self.start_track_editor(id);
                        }
                    }
                    Key::Named(Named::ArrowLeft) => {
                        let step = if modifiers.contains(Modifiers::SHIFT) {
                            -30.0
                        } else {
                            -5.0
                        };
                        self.seek_relative(step);
                    }
                    Key::Named(Named::ArrowRight) => {
                        let step = if modifiers.contains(Modifiers::SHIFT) {
                            30.0
                        } else {
                            5.0
                        };
                        self.seek_relative(step);
                    }
                    Key::Character("/") => {
                        return iced::widget::operation::focus(iced::widget::Id::new(
                            "flux-search",
                        ));
                    }
                    Key::Character("1") => self.select_view(LibraryView::Tracks),
                    Key::Character("2") => self.select_view(LibraryView::Artists),
                    Key::Character("3") => self.select_view(LibraryView::Albums),
                    Key::Character("4") => self.select_view(LibraryView::Folders),
                    Key::Character("5") => self.select_view(LibraryView::Playlists),
                    Key::Character("6") => self.select_view(LibraryView::Lyrics),
                    Key::Character("7") => self.select_view(LibraryView::Dashboard),
                    Key::Character("8") => self.select_view(LibraryView::Collections),
                    Key::Character("9") => self.select_view(LibraryView::RecentlyAdded),
                    Key::Character("0") => self.select_view(LibraryView::RecentlyPlayed),
                    Key::Character("k") => self.select_view(LibraryView::Settings),
                    Key::Character("l") => self.toggle_setup(),
                    Key::Named(Named::Escape) => {
                        if self.track_editor.take().is_some() {
                            return Task::none();
                        }
                        if self.lyrics_editor.is_some() {
                            self.cancel_lyrics_editor();
                            return Task::none();
                        }
                        self.cancel_queue_drag();
                        self.playlist_drag = None;
                        self.search.clear();
                        self.selected_tracks.clear();
                        self.focused_track = None;
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
                let mut start_dropped_scan = false;
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.progress_view = manager
                        .progress
                        .lock()
                        .map(|progress| progress.clone())
                        .unwrap_or_default();
                    if manager
                        .drop_deadline
                        .is_some_and(|deadline| deadline.elapsed() >= Duration::from_millis(220))
                    {
                        start_dropped_scan = true;
                    }
                }
                if start_dropped_scan {
                    if let Some(manager) = self.import_manager.as_mut() {
                        let pending = std::mem::take(&mut manager.pending_drops);
                        manager.drop_deadline = None;
                        manager.sources.extend(pending);
                        manager.stage = ImportUiStage::Choose;
                    }
                    return self.start_import_scan();
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
                self.seek_preview = None;
                self.window_width = width;
                if width >= QUEUE_INLINE_MIN_WIDTH {
                    self.queue_overlay_open = false;
                }
            }
            Message::WindowCloseRequested(id) => {
                self.closed = true;
                let _ = self.controller.save_index();
                self.controller.shutdown_in_place();
                return window::close(id);
            }
        }
        self.ensure_artwork_tasks()
    }

    // -------------------------------------------------------------- view ---

    fn view(&self) -> Element<'_, Message> {
        let queue_placement = self.queue_placement();
        let mut body = row![self.sidebar(), rule(), self.central()]
            .height(Length::Fill)
            .width(Length::Fill);
        if queue_placement == QueuePlacement::Inline {
            body = body.push(rule()).push(self.queue_panel(false));
        }

        let body: Element<'_, Message> = body.into();
        let base: Element<'_, Message> = if queue_placement == QueuePlacement::Overlay {
            stack([body, self.queue_overlay()])
                .height(Length::Fill)
                .into()
        } else {
            body
        };
        let body: Element<'_, Message> = if self.lyrics_import.is_some() {
            stack([base, self.lyrics_import_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.playlist_create_open {
            stack([base, self.playlist_create_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.batch_review.is_some() {
            stack([base, self.batch_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.album_transfer.is_some() {
            stack([base, self.album_transfer_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.deletion_manager.is_some() {
            stack([base, self.deletion_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.import_manager.is_some() {
            stack([base, self.import_overlay()])
                .height(Length::Fill)
                .into()
        } else if self.external_drag_hover {
            stack([base, self.drop_hint()]).height(Length::Fill).into()
        } else {
            base
        };

        container(column![body, horizontal_rule(), self.player_bar()].height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(app_style)
            .into()
    }

    fn album_transfer_overlay(&self) -> Element<'_, Message> {
        let Some(manager) = self.album_transfer.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let plan = &manager.plan;
        let source = self
            .controller
            .artist(plan.from_owner)
            .map(|artist| artist.name().to_owned())
            .unwrap_or_else(|| format!("Artist {}", plan.from_owner));
        let destination = self
            .controller
            .artist(plan.to_owner)
            .map(|artist| artist.name().to_owned())
            .unwrap_or_else(|| format!("Artist {}", plan.to_owner));
        let artist_choices = self
            .controller
            .artists()
            .into_iter()
            .filter(|artist| artist.id() != plan.from_owner)
            .map(|artist| ArtistChoice {
                id: Some(artist.id()),
                label: artist.name().to_owned(),
            })
            .collect::<Vec<_>>();
        let selected_artist = manager.destination_artist.clone().filter(|choice| {
            artist_choices
                .iter()
                .any(|candidate| candidate.id == choice.id)
        });
        let lyrics_count = plan
            .tracks
            .iter()
            .filter(|track| track.lyrics_path.is_some())
            .count();
        let rated_count = plan
            .tracks
            .iter()
            .filter(|track| track.rating.is_some())
            .count();
        let playlist_refs = plan
            .tracks
            .iter()
            .map(|track| track.playlist_ids.len())
            .sum::<usize>();
        let decision_count = plan.credit_decision_tracks().len();
        let mut panel = column![
            row![
                column![
                    text(format!("Transfer album · {}", plan.title)).size(20).color(TEXT),
                    text("One canonical transaction. AlbumId, TrackIds, track order, cover, lyrics, ratings, playlists and queue identities are preserved.")
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(3)
                .width(Length::Fill),
                button(text("Cancel").size(11))
                    .style(ghost_style)
                    .padding([7, 11])
                    .on_press(Message::AlbumTransferCancel),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            container(
                column![
                    text(format!(
                        "AlbumId {} · {source} → {destination} · {} track(s) · cover {} · {lyrics_count} lyrics · {rated_count} rated · {playlist_refs} playlist reference(s)",
                        plan.album_id,
                        plan.tracks.len(),
                        if plan.cover.is_some() { "managed" } else { "none" },
                    ))
                    .size(10)
                    .color(MUTED),
                    text("Destination is a new AlbumId-preserving location under the destination artist. Merging into a different existing AlbumId is intentionally blocked; no album is ever silently merged.")
                        .size(9)
                        .color(MUTED),
                ]
                .spacing(3),
            )
            .padding(10)
            .style(card_style),
            text("DESTINATION ARTIST").size(8).color(MUTED),
            pick_list(artist_choices, selected_artist, |choice| {
                Message::AlbumTransferDestinationArtist(Some(choice))
            })
            .placeholder("Destination artist…")
            .text_size(10)
            .padding([5, 7])
            .width(Length::Fill),
        ]
        .spacing(8);
        let mut rows = column![].spacing(4);
        for track in &plan.tracks {
            let credits = track
                .credited_artist_ids
                .iter()
                .map(|artist| {
                    self.controller
                        .artist(*artist)
                        .map(|artist| artist.name().to_owned())
                        .unwrap_or_else(|| format!("Artist {artist}"))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let owner = self
                .controller
                .artist(track.owner_artist_id)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {}", track.owner_artist_id));
            let credit_ui: Element<'_, Message> = if track.requires_credit {
                if track.credit_allowed {
                    row![
                        text("destination credit approved").size(9).color(SUCCESS),
                        button(text("Revoke").size(9))
                            .style(ghost_style)
                            .padding([4, 7])
                            .on_press(Message::AlbumTransferAllowCredit(track.track_id, false)),
                    ]
                    .spacing(6)
                    .into()
                } else {
                    row![
                        text("destination artist is not credited")
                            .size(9)
                            .color(DANGER),
                        button(text("Append destination credit").size(9))
                            .style(danger_button_style)
                            .padding([4, 7])
                            .on_press(Message::AlbumTransferAllowCredit(track.track_id, true)),
                    ]
                    .spacing(6)
                    .into()
                }
            } else {
                text("destination already credited")
                    .size(9)
                    .color(MUTED)
                    .into()
            };
            rows = rows.push(
                container(
                    column![
                        row![
                            text(track.title.clone())
                                .size(11)
                                .color(TEXT)
                                .width(Length::Fill),
                            credit_ui,
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                        text(format!("owner {owner} · credits {credits}"))
                            .size(9)
                            .color(MUTED),
                        text(format!(
                            "lyrics {} · rating {} · playlists {}",
                            track.lyrics_path.as_ref().map(|_| "yes").unwrap_or("no"),
                            track
                                .rating
                                .map(|rating| format!("{:?}", rating.rating))
                                .unwrap_or_else(|| "none".to_owned()),
                            track.playlist_ids.len()
                        ))
                        .size(8)
                        .color(MUTED),
                    ]
                    .spacing(2),
                )
                .padding(7)
                .style(card_style),
            );
        }
        panel = panel.push(
            container(scrollable(rows).height(Length::Fixed(230.0)))
                .padding(5)
                .style(card_style),
        );
        if decision_count > 0 {
            panel = panel.push(
                text(format!(
                    "{decision_count} track(s) require an explicit destination-credit decision. Rejecting or leaving them unresolved blocks the whole transfer."
                ))
                .size(9)
                .color(DANGER),
            );
        }
        if !plan.issues.is_empty() {
            panel = panel.push(
                text(format!(
                    "Blocked: {}",
                    plan.issues
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · ")
                ))
                .size(9)
                .color(DANGER),
            );
        }
        if manager.stage == AlbumTransferUiStage::Executing {
            panel = panel.push(
                text("Executing one canonical transaction (staging, verification, publication and cleanup)…")
                    .size(10)
                    .color(ACCENT),
            );
        }
        if manager.stage == AlbumTransferUiStage::Failed {
            panel = panel.push(
                text("Album transfer did not commit cleanly. Reopen/refresh the library to run recovery before retrying.")
                    .size(10)
                    .color(DANGER),
            );
        }
        if let Some(error) = &manager.error {
            panel = panel.push(text(error.clone()).size(10).color(DANGER));
        }
        if let Some(report) = &manager.report {
            panel = panel.push(
                text(format!(
                    "Committed: AlbumId {} transferred {} track(s) from {} to {}.",
                    report.album_id, report.tracks, report.from_owner, report.to_owner
                ))
                .size(10)
                .color(SUCCESS),
            );
        }
        panel = panel.push(
            row![
                text("This changes physical ownership and paths but preserves album/track identities and order.")
                    .size(9)
                    .color(MUTED)
                    .width(Length::Fill),
                button(text("Cancel / close").size(10))
                    .style(ghost_style)
                    .padding([6, 10])
                    .on_press(Message::AlbumTransferCancel),
                button(text("Commit album transfer").size(11))
                    .style(accent_button_style)
                    .padding([7, 11])
                    .on_press_maybe(
                        (manager.stage == AlbumTransferUiStage::Review && plan.is_ready())
                            .then_some(Message::AlbumTransferExecute)
                    ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        let panel = container(scrollable(panel).height(Length::Fill))
            .padding(16)
            .width(Length::Fill)
            .max_width(1_040.0)
            .height(Length::Fill)
            .max_height(720.0)
            .style(import_panel_style);
        stack([
            container(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(modal_backdrop_style)
            .into(),
            container(panel)
                .padding(18)
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .into(),
        ])
        .height(Length::Fill)
        .into()
    }

    fn deletion_overlay(&self) -> Element<'_, Message> {
        let Some(manager) = self.deletion_manager.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let selected = manager.selected.and_then(|id| {
            manager
                .plan
                .owned_tracks
                .iter()
                .chain(manager.plan.credited_tracks.iter())
                .find(|track| track.track_id == id)
                .cloned()
        });
        let transfer_count = manager
            .plan
            .owned_tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.decision,
                    flux_app::ArtistDeletionDecision::Transfer { .. }
                )
            })
            .count();
        let delete_count = manager.plan.deleted_track_ids().len();
        let lyrics_moved = manager
            .plan
            .owned_tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.decision,
                    flux_app::ArtistDeletionDecision::Transfer { .. }
                ) && track.lyrics_path.is_some()
            })
            .count();
        let lyrics_deleted = manager
            .plan
            .owned_tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.decision,
                    flux_app::ArtistDeletionDecision::Delete { confirmed: true }
                ) && track.lyrics_path.is_some()
            })
            .count();
        let mut panel = column![].spacing(10);
        panel = panel.push(
            row![
                column![
                    text(format!("Delete artist · {}", manager.plan.artist_name))
                        .size(20)
                        .color(TEXT),
                    text("One logical transaction. The artist is removed only after every affected track is resolved, staged, verified and committed.")
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(3)
                .width(Length::Fill),
                button(text("Cancel").size(11))
                    .style(ghost_style)
                    .padding([7, 11])
                    .on_press(Message::ArtistDeletionCancel),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        panel = panel.push(
            container(
                column![
                    text(format!(
                        "{} owned track(s) · {} credited-only track(s) · {} owned album(s) · {} affected playlist(s)",
                        manager.plan.owned_tracks.len(),
                        manager.plan.credited_tracks.len(),
                        manager.plan.albums.len(),
                        manager.plan.affected_playlists.len(),
                    ))
                    .size(10)
                    .color(MUTED),
                    text(format!(
                        "Resources: {transfer_count} audio file(s) copied, {delete_count} audio file(s) permanently deleted, {lyrics_moved} lyrics file(s) moved, {lyrics_deleted} lyrics file(s) deleted."
                    ))
                    .size(9)
                    .color(DANGER),
                    text("Transfers preserve TrackId, credits, playlist/rating references and album order. Explicit deletion permanently removes audio, lyrics, ratings and playlist entries.")
                        .size(9)
                        .color(MUTED),
                ]
                .spacing(3),
            )
            .padding(10)
            .style(card_style),
        );
        let mut rows = column![].spacing(4);
        for track in manager
            .plan
            .owned_tracks
            .iter()
            .chain(manager.plan.credited_tracks.iter())
        {
            let is_selected = manager.selected == Some(track.track_id);
            let owner = self
                .controller
                .artist(track.owner_artist_id)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {}", track.owner_artist_id));
            let decision = deletion_decision_label(&track.decision);
            let kind = if track.owner_artist_id == manager.plan.artist_id {
                "owned"
            } else {
                "credited-only"
            };
            rows = rows.push(
                button(
                    row![
                        column![
                            text(track.title.clone()).size(12).color(if is_selected {
                                ACCENT
                            } else {
                                TEXT
                            }),
                            text(format!(
                                "{kind} · owner {owner} · album {}",
                                track.album_title
                            ))
                            .size(9)
                            .color(MUTED),
                        ]
                        .spacing(1)
                        .width(Length::Fill),
                        column![
                            text(decision).size(9).color(if track.issues.is_empty() {
                                MUTED
                            } else {
                                DANGER
                            }),
                            text(format!(
                                "{} playlist(s) · rating {} · lyrics {}",
                                track.playlist_ids.len(),
                                track
                                    .rating
                                    .map(|rating| rating.rating.unwrap_or(0).to_string())
                                    .unwrap_or_else(|| "—".to_owned()),
                                if track.lyrics_path.is_some() {
                                    "yes"
                                } else {
                                    "no"
                                }
                            ))
                            .size(8)
                            .color(MUTED),
                        ]
                        .spacing(1),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([7, 9])
                .style(track_row_style(is_selected))
                .on_press(Message::ArtistDeletionSelect(track.track_id)),
            );
        }
        panel = panel.push(
            container(scrollable(rows).height(Length::Fixed(180.0)))
                .padding(6)
                .style(card_style),
        );
        if let Some(track) = selected {
            let owned = track.owner_artist_id == manager.plan.artist_id;
            panel = panel.push(
                container(
                    column![
                        text(format!("Selected: {}", track.title))
                            .size(13)
                            .color(TEXT),
                        text(format!("Credits: {}", self.credit_display(track.track_id)))
                            .size(9)
                            .color(MUTED),
                        text(format!(
                            "Audio: {} · lyrics: {} · rating: {} · playlists: {}",
                            track.audio_path.display_lossy(),
                            track
                                .lyrics_path
                                .as_ref()
                                .map(|path| path.display_lossy())
                                .unwrap_or_else(|| "none".to_owned()),
                            track
                                .rating
                                .map(|rating| format!("{:?}", rating.rating))
                                .unwrap_or_else(|| "none".to_owned()),
                            {
                                let names = track
                                    .playlist_ids
                                    .iter()
                                    .map(|id| {
                                        self.controller
                                            .playlists()
                                            .into_iter()
                                            .find(|playlist| playlist.id == *id)
                                            .map(|playlist| playlist.name)
                                            .unwrap_or_else(|| id.to_string())
                                    })
                                    .collect::<Vec<_>>();
                                if names.is_empty() {
                                    "none".to_owned()
                                } else {
                                    names.join(", ")
                                }
                            }
                        ))
                        .size(9)
                        .color(MUTED),
                    ]
                    .spacing(3),
                )
                .padding(9)
                .style(card_style),
            );
            if owned {
                let artist_choices = track
                    .eligible_owners
                    .iter()
                    .map(|artist| ArtistChoice {
                        id: Some(*artist),
                        label: self
                            .controller
                            .artist(*artist)
                            .map(|artist| artist.name().to_owned())
                            .unwrap_or_else(|| format!("Artist {artist}")),
                    })
                    .collect::<Vec<_>>();
                let selected_artist = manager.destination_artist.clone().filter(|choice| {
                    artist_choices
                        .iter()
                        .any(|candidate| candidate.id == choice.id)
                });
                let album_choices = manager
                    .destination_artist
                    .as_ref()
                    .and_then(|choice| choice.id)
                    .map(|owner| {
                        manager
                            .plan
                            .destination_albums
                            .iter()
                            .filter(|album| album.owner_artist_id == owner)
                            .map(|album| AlbumChoice {
                                id: album.id,
                                label: format!("{} · {}", album.title, album.id),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let selected_album = manager.destination_album.clone().filter(|choice| {
                    album_choices
                        .iter()
                        .any(|candidate| candidate.id == choice.id)
                });
                panel = panel.push(
                    column![
                        text("TRANSFER TO A SURVIVING CREDITED ARTIST").size(8).color(ACCENT),
                        pick_list(artist_choices, selected_artist, |choice| {
                            Message::ArtistDeletionDestinationArtist(Some(choice))
                        })
                        .placeholder("Destination artist…")
                        .text_size(10)
                        .padding([5, 7])
                        .width(Length::Fill),
                        pick_list(album_choices, selected_album, |choice| {
                            Message::ArtistDeletionDestinationAlbum(Some(choice))
                        })
                        .placeholder("Existing destination album…")
                        .text_size(10)
                        .padding([5, 7])
                        .width(Length::Fill),
                        text_input("…or new destination album title", &manager.new_album_title)
                            .on_input(Message::ArtistDeletionNewAlbum)
                            .style(input_style)
                            .width(Length::Fill),
                        row![
                            button(text("Set transfer").size(10))
                                .style(accent_button_style)
                                .padding([5, 9])
                                .on_press(Message::ArtistDeletionSetTransfer),
                            button(text("Delete this track permanently").size(10))
                                .style(danger_button_style)
                                .padding([5, 9])
                                .on_press(Message::ArtistDeletionSetDelete),
                            button(text("Reset decision").size(9))
                                .style(ghost_style)
                                .padding([5, 8])
                                .on_press(Message::ArtistDeletionResetDecision),
                        ]
                        .spacing(6),
                        text("Deleting a track is irreversible and removes its audio, lyrics, rating and playlist entries.")
                            .size(9)
                            .color(DANGER),
                    ]
                    .spacing(6),
                );
            } else {
                panel = panel.push(
                    column![
                        text("KEEP THE EXISTING OWNER; REMOVE ONLY THIS ARTIST'S CREDIT").size(8).color(ACCENT),
                        text("Audio, TrackId, album, owner, lyrics, ratings and playlist entries are preserved.")
                            .size(9)
                            .color(MUTED),
                        button(text("Remove this artist's credit only").size(10))
                            .style(accent_button_style)
                            .padding([5, 9])
                            .on_press(Message::ArtistDeletionSetRemoveCredit),
                    ]
                    .spacing(5),
                );
            }
            for issue in &track.issues {
                panel = panel.push(text(issue.clone()).size(9).color(DANGER));
            }
        }
        if !manager.plan.issues.is_empty() {
            panel = panel.push(
                text(format!(
                    "Blocked: {}",
                    manager
                        .plan
                        .issues
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · ")
                ))
                .size(9)
                .color(DANGER),
            );
        }
        if let Some(error) = &manager.error {
            panel = panel.push(text(error).size(10).color(DANGER));
        }
        if manager.stage == DeletionUiStage::Executing {
            panel = panel.push(
                text("Executing one canonical transaction (staging, verification, publication and cleanup)…")
                    .size(10)
                    .color(ACCENT),
            );
        }
        if manager.stage == DeletionUiStage::Failed {
            panel = panel.push(
                text("Deletion did not commit cleanly. Reopen/refresh the library to run recovery before retrying.")
                    .size(10)
                    .color(DANGER),
            );
        }
        if let Some(report) = &manager.report {
            panel = panel.push(
                text(format!(
                    "Committed: {} transferred, {} credit(s) removed, {} track(s) deleted, {} album(s) removed.",
                    report.transferred,
                    report.credits_removed,
                    report.deleted_tracks.len(),
                    report.deleted_albums.len()
                ))
                .size(10)
                .color(SUCCESS),
            );
        }
        panel = panel.push(
            row![
                text(format!(
                    "{} explicit deletion(s) · {} album(s) will be removed",
                    manager.plan.deleted_track_ids().len(),
                    manager.plan.albums.len()
                ))
                .size(9)
                .color(MUTED)
                .width(Length::Fill),
                button(text("Cancel / close").size(10))
                    .style(ghost_style)
                    .padding([6, 10])
                    .on_press(Message::ArtistDeletionCancel),
                button(text("Commit artist deletion").size(11))
                    .style(danger_button_style)
                    .padding([7, 11])
                    .on_press_maybe(
                        (manager.stage == DeletionUiStage::Review && manager.plan.is_ready())
                            .then_some(Message::ArtistDeletionExecute)
                    ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        let panel = container(scrollable(panel).height(Length::Fill))
            .padding(16)
            .width(Length::Fill)
            .max_width(1_040.0)
            .height(Length::Fill)
            .max_height(720.0)
            .style(import_panel_style);
        stack([
            container(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(modal_backdrop_style)
            .into(),
            container(panel)
                .padding(18)
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .into(),
        ])
        .height(Length::Fill)
        .into()
    }

    fn batch_overlay(&self) -> Element<'_, Message> {
        let Some(review) = self.batch_review.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let mut body = column![
            text("Review playlist change").size(22).color(TEXT),
            text(format!(
                "{} selected track(s) will be written in one canonical playlist transaction.",
                review.track_ids.len()
            ))
            .size(11)
            .color(MUTED),
        ]
        .spacing(8);
        if let Some(playlist_id) = review.playlist_id {
            body = body.push(
                text(format!(
                    "Target: {} · playlist {} · existing entries are retained and selected tracks append in selection order.",
                    review.playlist_label, playlist_id
                ))
                .size(10)
                .color(ACCENT),
            );
        } else {
            body = body.push(
                text("New playlist from the current ordered selection.")
                    .size(10)
                    .color(ACCENT),
            );
            body = body.push(
                text_input("Playlist name…", &review.new_playlist_name)
                    .on_input(Message::BatchPlaylistNameChanged)
                    .on_submit(Message::BatchReviewConfirm)
                    .style(input_style)
                    .width(Length::Fill),
            );
        }
        let mut tracks = column![].spacing(2);
        for id in review.track_ids.iter().take(6) {
            let label = self
                .controller
                .track(*id)
                .map(|track| format!("{} · {}", track.title(), display_artist(track)))
                .unwrap_or_else(|| format!("Track {id} is no longer available"));
            tracks = tracks.push(text(label).size(10).color(MUTED));
        }
        if review.track_ids.len() > 6 {
            tracks = tracks.push(
                text(format!("and {} more…", review.track_ids.len() - 6))
                    .size(9)
                    .color(MUTED),
            );
        }
        body = body.push(tracks);
        if let Some(error) = &review.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body = body.push(
            row![
                button(text("Confirm").size(12))
                    .style(accent_button_style)
                    .padding([8, 14])
                    .on_press(Message::BatchReviewConfirm),
                button(text("Cancel").size(11))
                    .style(ghost_style)
                    .padding([8, 12])
                    .on_press(Message::BatchReviewCancel),
            ]
            .spacing(8),
        );
        container(
            container(body)
                .padding(18)
                .width(Length::Fill)
                .max_width(580.0)
                .style(import_panel_style),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .style(modal_backdrop_style)
        .into()
    }

    fn import_overlay(&self) -> Element<'_, Message> {
        let Some(manager) = self.import_manager.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let mut panel = column![self.import_header(manager)].spacing(10);
        if manager.browser.is_some() {
            panel = panel.push(self.import_browser_body(manager));
            panel = panel.push(self.import_browser_footer(manager));
        } else {
            panel = panel.push(match manager.stage {
                ImportUiStage::Choose | ImportUiStage::Scanning => self.import_choose_body(manager),
                ImportUiStage::Review | ImportUiStage::Importing => {
                    self.import_review_body(manager)
                }
                ImportUiStage::Completed | ImportUiStage::Cancelled | ImportUiStage::Failed => {
                    self.import_result_body(manager)
                }
            });
            panel = panel.push(self.import_footer(manager));
        }

        let compact_panel = matches!(
            manager.stage,
            ImportUiStage::Completed | ImportUiStage::Cancelled | ImportUiStage::Failed
        ) && manager.browser.is_none();
        let panel = container(panel)
            .padding(16)
            .width(Length::Fill)
            .max_width(1_040.0)
            .height(if compact_panel {
                Length::Fixed(360.0)
            } else {
                Length::Fill
            })
            .max_height(680.0)
            .style(import_panel_style);
        let backdrop = mouse_area(
            container(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(modal_backdrop_style),
        )
        .on_press(Message::ImportNoop);
        stack([
            backdrop.into(),
            container(panel)
                .padding(18)
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .into(),
        ])
        .height(Length::Fill)
        .into()
    }

    fn import_header(&self, manager: &ImportManager) -> Element<'_, Message> {
        let (title, subtitle) = if manager.browser.is_some() {
            (
                "Flux Browser".to_owned(),
                "Choose files or a folder using Flux's built-in browser.".to_owned(),
            )
        } else {
            match manager.stage {
                ImportUiStage::Choose => (
                    "Import music".to_owned(),
                    "Choose files or folders, drop them here, then review before anything is written."
                        .to_owned(),
                ),
                ImportUiStage::Scanning => (
                    "Reading import…".to_owned(),
                    "Discovering files and reading metadata without changing the library."
                        .to_owned(),
                ),
                ImportUiStage::Review => (
                    "Review import".to_owned(),
                    format!(
                        "{} candidate(s) · edit metadata, resolve conflicts, and choose storage.",
                        manager.plan.as_ref().map_or(0, |plan| plan.candidates.len())
                    ),
                ),
                ImportUiStage::Importing => (
                    "Importing…".to_owned(),
                    "Copying files and publishing catalogue changes. Playback stays responsive."
                        .to_owned(),
                ),
                ImportUiStage::Completed => (
                    "Import complete".to_owned(),
                    "Only the categories you approved were written to the catalogue.".to_owned(),
                ),
                ImportUiStage::Cancelled => (
                    "Import cancelled".to_owned(),
                    "No catalogue changes were published for the cancelled stage.".to_owned(),
                ),
                ImportUiStage::Failed => (
                    "Import needs attention".to_owned(),
                    "The issue below prevented all or part of the import.".to_owned(),
                ),
            }
        };
        let close = button(text("×").size(18).color(MUTED))
            .style(ghost_style)
            .padding([2, 8])
            .on_press(Message::ImportClose);
        let close = tooltip(
            close,
            container(text("Close / cancel").size(10).color(TEXT))
                .padding([4, 7])
                .style(tooltip_style),
            tooltip::Position::Top,
        )
        .gap(6)
        .delay(Duration::from_millis(250));
        row![
            column![
                text(title).size(24).color(TEXT),
                text(subtitle).size(11).color(MUTED),
            ]
            .spacing(2)
            .width(Length::Fill),
            close,
        ]
        .align_y(Alignment::Start)
        .into()
    }

    fn import_choose_body<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        if manager.stage == ImportUiStage::Scanning {
            return self.import_progress_view(manager);
        }
        let portable = matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        );
        let library_label = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No portable library is active".to_owned());
        let mut body = column![].spacing(10);
        if let Some(target) = &manager.target_album {
            body = body.push(
                text(format!(
                    "FIXED TARGET · \"{}\" · AlbumId {} · owner {}",
                    target.title, target.album_id, target.owner_name
                ))
                .size(10)
                .color(ACCENT),
            );
        }
        if !portable {
            body = body.push(
                container(
                    column![
                        text("A canonical library is required").size(13).color(TEXT),
                        text(
                            "Imports, Flux metadata edits, stable artists, albums and resources \
                             live inside the canonical library. The legacy app index is a \
                             migration source only."
                        )
                        .size(10)
                        .color(MUTED),
                        button(text("Open Library setup").size(11))
                            .style(accent_button_style)
                            .padding([7, 12])
                            .on_press(Message::ImportOpenSetup),
                    ]
                    .spacing(6),
                )
                .padding(12)
                .width(Length::Fill)
                .style(card_style),
            );
        }

        let native_row = row![
            button(text("Choose files…").size(12))
                .style(accent_button_style)
                .padding([8, 14])
                .on_press_maybe(portable.then_some(Message::ImportNativeFiles)),
            button(text("Choose folder…").size(12))
                .style(ghost_style)
                .padding([8, 14])
                .on_press_maybe(portable.then_some(Message::ImportNativeFolder)),
            button(text("Flux Browser: files").size(11))
                .style(ghost_style)
                .padding([8, 12])
                .on_press_maybe(portable.then_some(Message::ImportBrowserOpenFiles)),
            button(text("Flux Browser: folder").size(11))
                .style(ghost_style)
                .padding([8, 12])
                .on_press_maybe(portable.then_some(Message::ImportBrowserOpenFolder)),
        ]
        .spacing(8)
        .wrap();
        body = body.push(native_row);

        let drop_zone =
            container(
                column![
                text("Drop audio files or folders here").size(16).color(TEXT),
                text(
                    "The native picker is the default path. Flux Browser is a working fallback; \
                     path entry is available for troubleshooting."
                )
                .size(10)
                .color(MUTED),
            ]
                .spacing(5)
                .align_x(Alignment::Center),
            )
            .padding(18)
            .width(Length::Fill)
            .height(Length::Fixed(108.0))
            .center(Length::Fill)
            .style(|_| {
                container::Style::default()
                    .background(if manager.drop_hover {
                        ACCENT_SOFT
                    } else {
                        SURFACE
                    })
                    .border(Border {
                        color: if manager.drop_hover { ACCENT } else { BORDER },
                        width: 1.0,
                        radius: 12.0.into(),
                    })
            });
        body = body.push(drop_zone);

        body = body.push(
            row![
                button(text(if manager.recursive {
                    "✓ Include subfolders"
                } else {
                    "Include subfolders"
                })
                .size(11))
                .style(nav_style(manager.recursive))
                .padding([6, 10])
                .on_press(Message::ImportRecursiveToggled),
                text("Folders are walked without following symbolic links; repeated paths are deduplicated.")
                    .size(10)
                    .color(MUTED),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        body = body.push(
            column![
                text("STORAGE").size(9).color(ACCENT),
                text("Every import is copied into the canonical library. External references are not supported; original sources are never moved or deleted.")
                    .size(10)
                    .color(MUTED),
                text(format!("Library: {library_label}")).size(10).color(MUTED),
            ]
            .spacing(5),
        );

        body = body.push(
            row![
                text_input(
                    "Troubleshooting fallback: audio file paths separated by ;",
                    &manager.manual_input,
                )
                .on_input(Message::ImportManualChanged)
                .on_submit(Message::ImportManualAdd)
                .style(input_style)
                .width(Length::Fill),
                button(text("Add paths").size(11))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe(
                        (!manager.manual_input.trim().is_empty())
                            .then_some(Message::ImportManualAdd)
                    ),
            ]
            .spacing(8),
        );

        if manager.stage == ImportUiStage::Scanning {
            body = body.push(self.import_progress_view(manager));
        }
        body = body.push(
            container(
                column![
                    text("REVIEW FIRST").size(9).color(ACCENT),
                    text("Nothing is written to the catalogue until you approve the reviewed list.")
                        .size(10)
                        .color(MUTED),
                    text("NON-DESTRUCTIVE").size(9).color(ACCENT),
                    text("Audio files are copied into the library; originals and embedded tags are never modified.")
                        .size(10)
                        .color(MUTED),
                    text("DUPLICATE-AWARE").size(9).color(ACCENT),
                    text("Exact paths, possible identical content, and similar metadata remain distinct decisions.")
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(4),
            )
            .padding(12)
            .width(Length::Fill)
            .style(card_style),
        );
        if let Some(error) = &manager.error {
            body = body.push(text(error).size(11).color(DANGER));
        }
        scrollable(body).height(Length::Fill).into()
    }

    fn import_browser_body<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let Some(browser) = manager.browser.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let mut body = column![
            row![
                button(text("↑ Up").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBrowserUp),
                text_input("Folder path…", &browser.path_input)
                    .on_input(Message::ImportBrowserPathChanged)
                    .on_submit(Message::ImportBrowserPathSubmit)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Go").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBrowserPathSubmit),
                button(text("Refresh").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBrowserRefresh),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(browser.current.display().to_string())
                .size(10)
                .color(MUTED),
            text(match browser.mode {
                BrowserMode::Files =>
                    "Click a folder to enter it. Click an audio file to select it; unsupported files are visibly disabled.",
                BrowserMode::Folder =>
                    "Navigate to the folder you want to import. The whole folder (including subfolders when enabled) becomes the source.",
            })
            .size(9)
            .color(MUTED),
        ]
        .spacing(6);

        if let Some(notice) = browser_omission_notice(browser.omitted, browser.symlinks_skipped) {
            body = body.push(text(notice).size(9).color(DANGER));
        }

        let mut rows = column![].spacing(2);
        for entry in &browser.entries {
            if entry.is_dir {
                rows = rows.push(
                    button(
                        row![
                            text("▸").size(11).color(ACCENT),
                            text(&entry.name).size(11).color(TEXT).width(Length::Fill),
                            text("folder").size(9).color(MUTED),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([5, 8])
                    .style(track_row_style(false))
                    .on_press(Message::ImportBrowserEnter(entry.path.clone())),
                );
            } else {
                let selected = browser.selected.contains(&entry.path);
                let color = if entry.supported { TEXT } else { MUTED };
                let row: Element<'_, Message> = row![
                    text(if selected { "✓" } else { " " })
                        .size(11)
                        .color(ACCENT)
                        .width(Length::Fixed(18.0)),
                    text(&entry.name).size(11).color(color).width(Length::Fill),
                    text(if entry.supported {
                        human_size(entry.size)
                    } else {
                        "unsupported".to_owned()
                    })
                    .size(9)
                    .color(MUTED),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into();
                let button = button(row)
                    .width(Length::Fill)
                    .padding([5, 8])
                    .style(track_row_style(selected));
                let item: Element<'_, Message> = if entry.supported {
                    button
                        .on_press(Message::ImportBrowserToggle(entry.path.clone()))
                        .into()
                } else {
                    button.into()
                };
                rows = rows.push(item);
            }
        }
        if browser.entries.is_empty() {
            rows = rows.push(text("This folder is empty.").size(11).color(MUTED));
        }
        body = body.push(
            container(scrollable(rows).height(Length::Fill))
                .padding(4)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(card_style),
        );
        if let Some(error) = &manager.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        if let Some(error) = &browser.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body.height(Length::Fill).into()
    }

    fn import_browser_footer<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let Some(browser) = manager.browser.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let summary = match browser.mode {
            BrowserMode::Files => format!("{} selected", browser.selected.len()),
            BrowserMode::Folder => browser.current.display().to_string(),
        };
        row![
            text(summary).size(10).color(MUTED).width(Length::Fill),
            button(text("Cancel").size(10))
                .style(ghost_style)
                .padding([6, 10])
                .on_press(Message::ImportBrowserCancel),
            button(
                text(if browser.mode == BrowserMode::Files {
                    "Select all audio"
                } else if manager.recursive {
                    "✓ Subfolders"
                } else {
                    "Include subfolders"
                })
                .size(10)
            )
            .style(ghost_style)
            .padding([6, 10])
            .on_press(if browser.mode == BrowserMode::Files {
                Message::ImportBrowserSelectAll
            } else {
                Message::ImportRecursiveToggled
            }),
            button(
                text(if browser.mode == BrowserMode::Files {
                    "Use selected"
                } else {
                    "Use this folder"
                })
                .size(11)
            )
            .style(accent_button_style)
            .padding([7, 12])
            .on_press_maybe(
                (browser.mode == BrowserMode::Folder || !browser.selected.is_empty())
                    .then_some(Message::ImportBrowserConfirm),
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    fn import_progress_view<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let progress = &manager.progress_view;
        let label = if progress.total > 0 {
            format!("{}  {}/{}", progress.message, progress.done, progress.total)
        } else {
            progress.message.clone()
        };
        let mut view = column![
            row![
                text(label).size(11).color(TEXT).width(Length::Fill),
                button(
                    text(if manager.cancel.load(Ordering::Acquire) {
                        "Cancelling…"
                    } else {
                        "Cancel"
                    })
                    .size(10)
                )
                .style(ghost_style)
                .padding([5, 8])
                .on_press_maybe(
                    (!manager.cancel.load(Ordering::Acquire)).then_some(Message::ImportCancel)
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(6);
        if progress.total > 0 {
            let value = (progress.done as f32 / progress.total as f32).clamp(0.0, 1.0);
            view = view.push(
                progress_bar(0.0..=1.0, value)
                    .girth(5)
                    .style(progress_style)
                    .length(Length::Fill),
            );
        }
        container(view)
            .padding(10)
            .width(Length::Fill)
            .style(card_style)
            .into()
    }

    fn import_review_body<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let Some(plan) = manager.plan.as_ref() else {
            return self.import_choose_body(manager);
        };
        let mut body = column![].spacing(8);
        if let Some(target) = &plan.target_album {
            body = body.push(
                text(format!(
                    "FIXED TARGET · \"{}\" · AlbumId {} · owner {}",
                    target.title, target.album_id, target.owner_name
                ))
                .size(10)
                .color(ACCENT),
            );
        }
        body = body.push(
            row![
                text(format!(
                    "{} new · {} update · {} skip · {} issue(s)",
                    plan.new_count(),
                    plan.update_count(),
                    plan.skip_count(),
                    plan.issues.len()
                ))
                .size(10)
                .color(MUTED)
                .width(Length::Fill),
                button(text("Skip all").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBulkSkip),
                button(text("Import new").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBulkImport),
                button(text("Update exact").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportBulkUpdate),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
        body = body.push(
            text("Keys: ↑/↓ select · s skip · i import · u update · Ctrl+Enter import · Esc close")
                .size(9)
                .color(MUTED),
        );
        body = body.push(
            row![
                text(format!(
                    "{} candidate(s) checked for bulk actions",
                    manager.checked.len()
                ))
                .size(9)
                .color(if manager.checked.is_empty() {
                    MUTED
                } else {
                    ACCENT
                }),
                button(text("Check all").size(9))
                    .style(ghost_style)
                    .padding([4, 7])
                    .on_press(Message::ImportCheckAll),
                button(text("Clear checks").size(9))
                    .style(ghost_style)
                    .padding([4, 7])
                    .on_press(Message::ImportClearChecked),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
        body = body.push(
            row![
                text("COPY-ONLY · managed canonical library")
                    .size(9)
                    .color(ACCENT),
                text(format!("Library: {}", plan.library_root.display()))
                    .size(9)
                    .color(MUTED),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        if !plan.issues.is_empty() {
            let mut issue_text = format!("{} skipped/unreadable path(s): ", plan.issues.len());
            for issue in plan.issues.iter().take(3) {
                issue_text.push_str(&format!(
                    "{} ({}); ",
                    issue.path().display(),
                    issue.reason()
                ));
            }
            body = body.push(text(issue_text).size(9).color(MUTED));
        }
        let mut rows = column![].spacing(3);
        for (index, candidate) in plan.candidates.iter().enumerate() {
            rows = rows.push(self.import_candidate_row(manager, index, candidate));
        }
        let list = scrollable(rows).height(Length::Fill);
        let details = self.import_candidate_details(manager);

        body = body.push(
            row![
                container(list)
                    .width(Length::FillPortion(3))
                    .height(Length::Fill)
                    .style(card_style)
                    .padding(6),
                container(details)
                    .width(Length::FillPortion(2))
                    .height(Length::Fill)
                    .style(card_style)
                    .padding(12),
            ]
            .spacing(10)
            .height(Length::Fill),
        );
        if manager.stage == ImportUiStage::Importing {
            body = body.push(self.import_progress_view(manager));
        }
        if let Some(error) = &manager.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body.height(Length::Fill).into()
    }

    fn import_candidate_row<'a>(
        &self,
        manager: &ImportManager,
        index: usize,
        candidate: &'a flux_app::ImportCandidate,
    ) -> Element<'a, Message> {
        let selected = manager.selected == Some(index);
        let (status, color) = match candidate.status() {
            CandidateStatus::New => ("New", SUCCESS),
            CandidateStatus::ExistingPath(_) => (
                "Exact audio · same path",
                Color::from_rgb8(0xff, 0xc2, 0x66),
            ),
            CandidateStatus::SameContent {
                track_id: Some(_), ..
            } if candidate.metadata_matches_existing() => {
                ("Exact audio duplicate", Color::from_rgb8(0xff, 0xc2, 0x66))
            }
            CandidateStatus::SameContent {
                track_id: Some(_), ..
            } => (
                "Same audio · metadata differs",
                Color::from_rgb8(0xff, 0xc2, 0x66),
            ),
            CandidateStatus::SameContent { track_id: None, .. } => (
                "Duplicate incoming audio",
                Color::from_rgb8(0xff, 0xc2, 0x66),
            ),
            CandidateStatus::SimilarMetadata(_) => ("Metadata collision", MUTED),
            CandidateStatus::SameTitleDifferentAudio(_) => ("Same title · different audio", MUTED),
            CandidateStatus::ProbableAudio(_) => ("Probable duplicate · uncertain", MUTED),
            CandidateStatus::AmbiguousAudio(_) => ("Ambiguous audio match", MUTED),
        };
        let decision = match candidate.decision() {
            ImportDecision::ImportNew => "import",
            ImportDecision::UpdateExisting => "update",
            ImportDecision::Skip => "skip",
        };
        let progress_state = manager
            .progress_view
            .items
            .get(index)
            .copied()
            .unwrap_or(ImportItemState::Pending);
        let state = match progress_state {
            ImportItemState::Scanning => "scanning",
            ImportItemState::Importing => "importing",
            ImportItemState::Imported => "imported",
            ImportItemState::Failed => "failed",
            _ => decision,
        };
        let mut title = row![
            text(format!("{}", index + 1))
                .size(9)
                .color(MUTED)
                .width(Length::Fixed(22.0)),
            column![
                text(candidate.title())
                    .size(12)
                    .color(if selected { ACCENT } else { TEXT }),
                text(candidate.artist_name().unwrap_or("Unknown artist"))
                    .size(9)
                    .color(MUTED),
            ]
            .spacing(1)
            .width(Length::Fill),
            column![
                text(status).size(9).color(color),
                text(format!(
                    "{} · {} · {}",
                    candidate
                        .source_track()
                        .duration()
                        .map(format_duration)
                        .unwrap_or_else(|| "--:--".to_owned()),
                    candidate.format(),
                    human_size(candidate.file_size())
                ))
                .size(8)
                .color(MUTED),
            ]
            .spacing(1),
            text(state).size(8).color(MUTED).width(Length::Fixed(58.0)),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        if candidate.metadata_incomplete() {
            title = title.push(text("incomplete").size(8).color(MUTED));
        }
        if !flux_app::is_track_playable(candidate.source_track()) {
            title = title.push(text("indexed, not playable").size(8).color(MUTED));
        }
        row![
            button(
                text(if manager.checked.contains(&index) {
                    "☑"
                } else {
                    "☐"
                })
                .size(13)
            )
            .style(ghost_style)
            .padding([5, 6])
            .on_press(Message::ImportToggleChecked(index)),
            button(title)
                .width(Length::Fill)
                .padding([7, 8])
                .style(track_row_style(selected))
                .on_press(Message::ImportSelect(index)),
        ]
        .spacing(3)
        .align_y(Alignment::Center)
        .into()
    }

    fn import_candidate_details<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let Some(plan) = manager.plan.as_ref() else {
            return text("No candidate selected.").size(11).color(MUTED).into();
        };
        let Some(index) = manager.selected else {
            return text("Select a candidate to edit.")
                .size(11)
                .color(MUTED)
                .into();
        };
        let Some(candidate) = plan.candidates.get(index) else {
            return text("Select a candidate to edit.")
                .size(11)
                .color(MUTED)
                .into();
        };
        let mut body = column![
            row![
                text(candidate.title()).size(16).color(TEXT),
                if candidate.is_edited() {
                    Element::from(text("edited").size(9).color(ACCENT))
                } else {
                    Element::from(iced::widget::Space::new().width(0))
                },
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(candidate.source_path().display().to_string())
                .size(9)
                .color(MUTED),
            text(format!(
                "Inferred from names: {} · {} · {}",
                candidate.source_track().title(),
                candidate.source_track().artist().unwrap_or("—"),
                candidate.source_track().album().unwrap_or("—")
            ))
            .size(9)
            .color(MUTED),
            text(format!(
                "{} · {} · {}",
                candidate
                    .source_track()
                    .duration()
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".to_owned()),
                candidate.format(),
                human_size(candidate.file_size())
            ))
            .size(9)
            .color(MUTED),
        ]
        .spacing(5);

        if !flux_app::is_track_playable(candidate.source_track()) {
            body = body.push(
                text("This format can be indexed but is not playable by the current engine.")
                    .size(9)
                    .color(Color::from_rgb8(0xff, 0xc2, 0x66)),
            );
        }
        if let Some(existing) = candidate.existing_match() {
            let target_relation = plan.target_album.as_ref().map_or(
                "general destination; the existing TrackId remains authoritative",
                |target| {
                    if existing.album_id == Some(target.album_id) {
                        "already in the fixed target album"
                    } else {
                        "in another album; the fixed target will never redirect it"
                    }
                },
            );
            body = body.push(
                text(format!(
                    "EXISTING · TrackId {} · {} · {} · {} · {}",
                    existing.track_id,
                    existing.title,
                    existing.artist_name.as_deref().unwrap_or("Unknown artist"),
                    existing.album_title.as_deref().unwrap_or("Unknown album"),
                    target_relation
                ))
                .size(9)
                .color(MUTED),
            );
            body = body.push(
                text(format!(
                    "Existing credits: {} · album id: {} · managed audio: {} · fingerprint recorded: {} · physical owner is preserved by metadata updates",
                    if existing.credited_artist_ids.is_empty() {
                        "unknown".to_owned()
                    } else {
                        existing
                            .credited_artist_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    existing
                        .album_id
                        .map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
                    existing.audio_path.display(),
                    if existing.fingerprint.is_some() {
                        "yes"
                    } else {
                        "no"
                    }
                ))
                .size(8)
                .color(MUTED),
            );
        }
        if let CandidateStatus::AmbiguousAudio(ids) = candidate.status() {
            body = body.push(
                text(format!(
                    "Ambiguous match candidates: {}",
                    ids.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .size(8)
                .color(MUTED),
            );
        }
        if candidate.status().has_conflict() {
            body = body.push(
                text("Audio replacement is unavailable in this build: it is not exposed through the transactional replacement workflow. Existing audio is never deleted or overwritten by duplicate resolution.")
                    .size(8)
                    .color(Color::from_rgb8(0xff, 0xc2, 0x66)),
            );
        }
        match candidate.status() {
            CandidateStatus::ExistingPath(_) => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                        button(text("Update metadata").size(10))
                            .style(nav_style(
                                candidate.decision() == ImportDecision::UpdateExisting
                            ))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::UpdateExisting,
                            )),
                    ]
                    .spacing(6),
                );
            }
            CandidateStatus::SameContent {
                track_id: Some(_), ..
            } => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                        button(text("Update metadata").size(10))
                            .style(nav_style(
                                candidate.decision() == ImportDecision::UpdateExisting
                            ))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::UpdateExisting,
                            )),
                        button(text("Import separately").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::ImportNew))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::ImportNew,
                            )),
                    ]
                    .spacing(6),
                );
            }
            CandidateStatus::SameContent { track_id: None, .. } => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                        button(text("Import separately").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::ImportNew))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::ImportNew,
                            )),
                    ]
                    .spacing(6),
                );
            }
            CandidateStatus::New
            | CandidateStatus::SimilarMetadata(_)
            | CandidateStatus::SameTitleDifferentAudio(_)
            | CandidateStatus::ProbableAudio(_)
            | CandidateStatus::AmbiguousAudio(_) => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                        button(text("Import separately").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::ImportNew))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::ImportNew,
                            )),
                    ]
                    .spacing(6),
                );
            }
        }

        let editable = manager.stage == ImportUiStage::Review;
        let bulk_artists = self.artist_choices();
        let mut bulk = column![
            text(format!(
                "BULK APPLY · {} checked",
                manager.checked.len()
            ))
            .size(9)
            .color(ACCENT),
            text("Bulk changes explicitly overwrite the selected candidates' Artist/Album fields. Per-candidate edits made afterward win; Track artwork overrides and lyrics remain individual.")
                .size(8)
                .color(MUTED),
            row![
                pick_list(bulk_artists, manager.bulk_artist.clone(), |choice| {
                    Message::ImportBulkArtistPicked(Some(choice))
                })
                .placeholder("Existing Artist for checked candidates…")
                .text_size(10)
                .padding([5, 7])
                .width(Length::Fill),
                button(text("Apply Artist").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkApplyArtist),
            ]
            .spacing(6),
            row![
                text_input("Or new Artist name for checked candidates…", &manager.bulk_new_artist_name)
                    .on_input(Message::ImportBulkNewArtistChanged)
                    .on_submit(Message::ImportBulkApplyNewArtist)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Apply new name").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkApplyNewArtist),
            ]
            .spacing(6),
            row![
                text_input("Album title for checked candidates…", &manager.bulk_album_title)
                    .on_input(Message::ImportBulkAlbumChanged)
                    .on_submit(Message::ImportBulkApplyAlbum)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Apply Album").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkApplyAlbum),
                button(text("Make standalone").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkMakeStandalone),
            ]
            .spacing(6),
            row![
                text_input("Album artwork path…", &manager.bulk_album_artwork_path)
                    .on_input(Message::ImportBulkAlbumArtworkChanged)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Choose…").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkAlbumArtworkChoose),
                button(text("Apply artwork").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkApplyAlbumArtwork),
                button(text("Clear artwork").size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::ImportBulkClearAlbumArtwork),
            ]
            .spacing(6),
        ]
        .spacing(5)
        .padding(8);
        if let Some(error) = &manager.asset_error {
            bulk = bulk.push(text(error).size(9).color(DANGER));
        }
        body = body.push(container(bulk).style(card_style));
        body = body.push(text("TITLE").size(8).color(MUTED));
        body = body.push(
            text_input("Title", &manager.edit_title)
                .on_input(Message::ImportTitleChanged)
                .style(input_style)
                .width(Length::Fill),
        );
        let targeted = plan.target_album.is_some();
        if let Some(target) = &plan.target_album {
            body = body.push(
                text(format!(
                    "FIXED TARGET ALBUM · {} · AlbumId {} · owner {}",
                    target.title, target.album_id, target.owner_name
                ))
                .size(9)
                .color(ACCENT),
            );
            body = body.push(
                text("The target album and physical owner cannot be changed inside this review. Close and start a new import to choose another destination.")
                    .size(8)
                    .color(MUTED),
            );
        } else {
            body = body.push(text("ARTIST").size(8).color(MUTED));
            body = body.push(
                text_input("Artist name", &manager.edit_artist)
                    .on_input(Message::ImportArtistChanged)
                    .style(input_style)
                    .width(Length::Fill),
            );
            let mut choices = self.editor_artist_choices();
            if let Some(choice) = choices.first_mut() {
                choice.label = "No stable artist (create/link from artist name)".to_owned();
            }
            let selected_artist = candidate
                .artist_id()
                .and_then(|id| choices.iter().find(|choice| choice.id == Some(id)).cloned())
                .or_else(|| choices.first().cloned());
            body = body.push(
                pick_list(choices, selected_artist, |choice| {
                    Message::ImportArtistPicked(Some(choice))
                })
                .text_size(11)
                .padding([6, 8])
                .width(Length::Fill),
            );
        }
        body = body.push(text("ORDERED CREDITS").size(8).color(MUTED));
        for (credit_index, credit) in candidate.credits().iter().enumerate() {
            let owner = credit_index == candidate.owner_index();
            body = body.push(
                row![
                    text(format!(
                        "{}. {} ({})",
                        credit_index + 1,
                        credit.name,
                        if credit.artist_id.is_some() {
                            "existing artist"
                        } else {
                            "proposed / unresolved"
                        }
                    ))
                    .size(10)
                    .color(if owner { ACCENT } else { TEXT })
                    .width(Length::Fill),
                    if owner {
                        Element::from(text("OWNER").size(8).color(ACCENT))
                    } else if targeted {
                        Element::from(text("credit").size(8).color(MUTED))
                    } else {
                        Element::from(
                            button(text("Set owner").size(9))
                                .style(ghost_style)
                                .padding([4, 7])
                                .on_press(Message::ImportCreditSetOwner(credit_index)),
                        )
                    },
                    button(text("↑").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press(Message::ImportCreditMove(credit_index, -1)),
                    button(text("↓").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press(Message::ImportCreditMove(credit_index, 1)),
                    button(text("×").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press_maybe(
                            (!owner && candidate.credits().len() > 1)
                                .then_some(Message::ImportCreditRemove(credit_index)),
                        ),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            );
        }
        body = body.push(
            row![
                text_input("Additional credited artist…", &manager.edit_credit)
                    .on_input(Message::ImportCreditNameChanged)
                    .on_submit(Message::ImportCreditAddNamed)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Add credit").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportCreditAddNamed),
            ]
            .spacing(6),
        );
        let mut add_choices = self.editor_artist_choices();
        add_choices.retain(|choice| choice.id.is_some());
        body = body.push(
            pick_list(
                add_choices,
                None::<ArtistChoice>,
                Message::ImportCreditAddExisting,
            )
            .placeholder("Add existing artist…")
            .text_size(10)
            .padding([5, 7])
            .width(Length::Fill),
        );
        body = body.push(
            text("The owner must be one of the ordered credits. Ejecting the owner or adding an invalid credit blocks import.")
                .size(9)
                .color(MUTED),
        );
        if let Some(target) = &plan.target_album {
            body = body.push(
                text(format!(
                    "Album: {} (fixed AlbumId {})",
                    target.title, target.album_id
                ))
                .size(9)
                .color(MUTED),
            );
        } else {
            body = body.push(text("ALBUM (OPTIONAL)").size(8).color(MUTED));
            body = body.push(
                row![
                    text_input(
                        "Album title, or leave empty for standalone…",
                        &manager.edit_album
                    )
                    .on_input(Message::ImportAlbumChanged)
                    .style(input_style)
                    .width(Length::Fill),
                    button(text("Make standalone").size(9))
                        .style(ghost_style)
                        .padding([5, 7])
                        .on_press(Message::ImportAlbumChanged(String::new())),
                ]
                .spacing(6),
            );
            let album_choices = self.import_album_choices(candidate);
            if candidate.artist_id().is_some() {
                let selected_album = album_choices
                    .iter()
                    .find(|choice| {
                        self.controller
                            .album(choice.id)
                            .is_some_and(|album| candidate.album() == Some(album.title.as_str()))
                    })
                    .cloned();
                body = body.push(
                    pick_list(album_choices, selected_album, |choice| {
                        Message::ImportAlbumPicked(Some(choice))
                    })
                    .placeholder("Create a new Album from this title, or select an existing Album…")
                    .text_size(10)
                    .padding([5, 7])
                    .width(Length::Fill),
                );
            } else {
                body = body.push(
                    text("Assign a stable Artist above before selecting an existing Album. Typing a title creates a new Album only when no exact match exists for that owner.")
                        .size(8)
                        .color(MUTED),
                );
            }
            if let (Some(owner), Some(title)) = (candidate.artist_id(), candidate.album()) {
                if let Some(existing) = self
                    .controller
                    .albums()
                    .into_iter()
                    .find(|album| album.owner_artist_id == owner && album.title == title)
                {
                    body = body.push(
                        text(format!(
                            "Reuses existing AlbumId {} · owner {}",
                            existing.id, existing.title
                        ))
                        .size(8)
                        .color(SUCCESS),
                    );
                } else {
                    body = body.push(
                        text("Creates a new Album inside the import transaction.")
                            .size(8)
                            .color(ACCENT),
                    );
                }
            } else if candidate.album().is_none() {
                body = body.push(
                    text("Standalone Track · no Album will be created or referenced.")
                        .size(8)
                        .color(ACCENT),
                );
            }
            if candidate.decision() == ImportDecision::UpdateExisting {
                body = body.push(
                    text("Metadata updates preserve the existing album membership and managed audio.")
                        .size(8)
                        .color(MUTED),
                );
            }
        }
        body = body.push(
            row![
                text_input("Track #", &manager.edit_track_number)
                    .on_input(Message::ImportTrackNumberChanged)
                    .style(input_style)
                    .width(Length::Fill),
                text_input("Disc #", &manager.edit_disc_number)
                    .on_input(Message::ImportDiscNumberChanged)
                    .style(input_style)
                    .width(Length::Fill),
            ]
            .spacing(6),
        );
        if let Some(error) = &manager.number_error {
            body = body.push(text(error).size(9).color(DANGER));
        }
        body = body.push(text("TRACK ARTWORK OVERRIDE").size(8).color(MUTED));
        body = body.push(
            text_input("Optional Track artwork image path…", &manager.edit_artwork)
                .on_input(Message::ImportArtworkChanged)
                .on_submit(Message::ImportArtworkApply)
                .style(input_style)
                .width(Length::Fill),
        );
        body = body.push(
            row![
                button(text("Choose image…").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportArtworkChoose),
                button(text("Attach path").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportArtworkApply),
                button(text("Use Album artwork").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportArtworkClear),
            ]
            .spacing(6),
        );
        body = body.push(
            text(if candidate.artwork_path().is_some() {
                "Track-specific artwork attached. It overrides Album artwork for this Track."
            } else {
                "No override attached; the Track inherits Album artwork when available."
            })
            .size(8)
            .color(MUTED),
        );
        body = body.push(text("ALBUM ARTWORK").size(8).color(MUTED));
        body = body.push(
            text_input(
                "Optional Album artwork image path…",
                &manager.edit_album_artwork,
            )
            .on_input(Message::ImportAlbumArtworkChanged)
            .on_submit(Message::ImportAlbumArtworkApply)
            .style(input_style)
            .width(Length::Fill),
        );
        body = body.push(
            row![
                button(text("Choose image…").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportAlbumArtworkChoose),
                button(text("Attach path").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportAlbumArtworkApply),
                button(text("Clear").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportAlbumArtworkClear),
            ]
            .spacing(6),
        );
        body = body.push(
            text("Album artwork is shared by all Tracks in that Album. Attaching it during import replaces existing Album artwork; Track overrides stay independent.")
                .size(8)
                .color(MUTED),
        );
        body = body.push(text("LYRICS FILE (LRC / TXT)").size(8).color(MUTED));
        body = body.push(
            text_input(
                "Optional .lrc or .txt lyrics path…",
                &manager.edit_lyrics_path,
            )
            .on_input(Message::ImportLyricsPathChanged)
            .on_submit(Message::ImportLyricsAttach)
            .style(input_style)
            .width(Length::Fill),
        );
        body = body.push(
            row![
                button(text("Attach lyrics").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportLyricsAttach),
                button(text("Clear").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ImportLyricsClear),
            ]
            .spacing(6),
        );
        if let Some(lyrics) = candidate.lyrics_text() {
            body = body.push(
                text(format!(
                    "Lyrics attached during review ({} lines). Managed by TrackId after commit.",
                    lyrics.lines().count()
                ))
                .size(8)
                .color(ACCENT),
            );
        }
        if let Some(error) = &manager.asset_error {
            body = body.push(text(error).size(9).color(DANGER));
        }
        if matches!(self.controller.storage_mode(), StorageMode::Canonical) {
            body = body.push(
                text("Destination: assigned inside the canonical album directory at commit.")
                    .size(9)
                    .color(MUTED),
            );
        } else if let Some(destination) = candidate.destination() {
            body = body.push(
                text(format!("Destination: {}", destination.display()))
                    .size(9)
                    .color(MUTED),
            );
        }
        if let Some(error) = candidate.storage_error() {
            body = body.push(text(error).size(9).color(DANGER));
        }
        body = body.push(
            text(
                "Metadata was inferred only from filenames and folders. Review edits, explicitly \
                 attached artwork, and lyrics files become canonical Flux data in one atomic \
                 commit; audio tags are never read or written. Artwork is not extracted from \
                 embedded tags.",
            )
            .size(9)
            .color(MUTED),
        );
        if !editable {
            body = body.push(
                text("Editing is disabled while files/catalogue changes are in progress.")
                    .size(9)
                    .color(MUTED),
            );
        }
        scrollable(body).height(Length::Fill).into()
    }

    fn import_footer<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let mut footer = row![].spacing(8).align_y(Alignment::Center);
        let summary = match manager.stage {
            ImportUiStage::Review => manager.plan.as_ref().map_or_else(
                || "No reviewed plan".to_owned(),
                |plan| {
                    format!(
                        "{} new · {} update · {} skip · {} issue(s)",
                        plan.new_count(),
                        plan.update_count(),
                        plan.skip_count(),
                        plan.issues.len()
                    )
                },
            ),
            ImportUiStage::Importing => manager.progress_view.message.clone(),
            ImportUiStage::Completed => manager.report.as_ref().map_or_else(
                || "Complete".to_owned(),
                |report| {
                    format!(
                        "{} imported · {} updated · {} skipped · {} failed",
                        report.imported, report.updated, report.skipped, report.failed
                    )
                },
            ),
            ImportUiStage::Cancelled => "Cancelled before catalogue publication".to_owned(),
            ImportUiStage::Failed => "Import could not complete".to_owned(),
            ImportUiStage::Choose | ImportUiStage::Scanning => String::new(),
        };
        footer = footer.push(text(summary).size(10).color(MUTED).width(Length::Fill));

        match manager.stage {
            ImportUiStage::Review => {
                footer = footer.push(
                    button(text("Cancel").size(10))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press(Message::ImportCancel),
                );
                let can_import = manager.plan.as_ref().is_some_and(ImportPlan::can_import);
                footer = footer.push(
                    button(
                        text(format!(
                            "Import {} item(s)",
                            manager
                                .plan
                                .as_ref()
                                .map_or(0, |plan| { plan.new_count() + plan.update_count() })
                        ))
                        .size(11),
                    )
                    .style(accent_button_style)
                    .padding([7, 12])
                    .on_press_maybe(can_import.then_some(Message::ImportApply)),
                );
            }
            ImportUiStage::Importing => {
                footer = footer.push(
                    button(text("Cancel").size(10))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press_maybe(
                            (!manager.cancel.load(Ordering::Acquire))
                                .then_some(Message::ImportCancel),
                        ),
                );
            }
            ImportUiStage::Completed | ImportUiStage::Cancelled | ImportUiStage::Failed => {
                footer = footer.push(
                    button(text("Choose again").size(10))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press(Message::ImportReset),
                );
                footer = footer.push(
                    button(text("Done").size(11))
                        .style(accent_button_style)
                        .padding([7, 12])
                        .on_press(Message::ImportClose),
                );
            }
            ImportUiStage::Choose | ImportUiStage::Scanning => {
                footer = footer.push(
                    button(text("Close").size(10))
                        .style(ghost_style)
                        .padding([6, 10])
                        .on_press(Message::ImportClose),
                );
            }
        }
        container(footer).padding([4, 2]).width(Length::Fill).into()
    }

    fn import_result_body<'a>(&'a self, manager: &'a ImportManager) -> Element<'a, Message> {
        let mut body = column![].spacing(10);
        if let Some(error) = &manager.error {
            body = body.push(
                container(text(error).size(11).color(DANGER))
                    .padding(12)
                    .width(Length::Fill)
                    .style(error_banner_style),
            );
        }
        if let Some(report) = &manager.report {
            body = body.push(
                text(format!(
                    "{} imported · {} updated · {} skipped · {} failed",
                    report.imported, report.updated, report.skipped, report.failed
                ))
                .size(16)
                .color(TEXT),
            );
            for item in &report.items {
                let (state, color) = match item.state {
                    ImportItemState::Imported => ("imported", SUCCESS),
                    ImportItemState::Skipped => ("skipped", MUTED),
                    ImportItemState::Failed => ("failed", DANGER),
                    _ => ("pending", MUTED),
                };
                let source = manager
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.candidates.get(item.index))
                    .map_or_else(
                        || "Source unavailable".to_owned(),
                        |candidate| candidate.source_path().display().to_string(),
                    );
                let detail = match (item.track_id, item.message.trim()) {
                    (Some(id), "") => format!("Track {id}"),
                    (Some(id), message) => format!("Track {id} · {message}"),
                    (None, "") => "No detail was recorded.".to_owned(),
                    (None, message) => message.to_owned(),
                };
                let source_label = tooltip(
                    text(truncate_label(&source, 96)).size(10).color(TEXT),
                    container(text(source).size(9).color(TEXT))
                        .padding([4, 7])
                        .style(tooltip_style),
                    tooltip::Position::Top,
                )
                .gap(6)
                .delay(Duration::from_millis(350));
                body = body.push(
                    row![
                        text(state).size(9).color(color).width(Length::Fixed(56.0)),
                        column![source_label, text(detail).size(9).color(MUTED)]
                            .spacing(1)
                            .width(Length::Fill),
                    ]
                    .spacing(8),
                );
            }
        }
        if let Some(plan) = &manager.plan
            && !plan.issues.is_empty()
        {
            body = body.push(text("Issues").size(10).color(ACCENT));
            for issue in plan.issues.iter().take(20) {
                body = body.push(
                    text(format!("{} — {}", issue.path().display(), issue.reason()))
                        .size(9)
                        .color(MUTED),
                );
            }
        }
        scrollable(body).height(Length::Fill).into()
    }

    fn drop_hint(&self) -> Element<'_, Message> {
        container(
            column![
                text("Drop to import").size(26).color(TEXT),
                text("Audio files and folders can be dropped anywhere in the window.")
                    .size(12)
                    .color(MUTED),
            ]
            .spacing(8)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .style(|_| {
            container::Style::default()
                .background(Color::from_rgba8(6, 8, 12, 0.90))
                .border(Border {
                    color: ACCENT_BORDER,
                    width: 2.0,
                    radius: 16.0.into(),
                })
        })
        .into()
    }

    fn nav_counts(&self) -> (usize, usize, usize, usize, usize, usize) {
        let mut artists: HashSet<String> = HashSet::new();
        let mut albums: HashSet<Option<&str>> = HashSet::new();
        let mut folders: HashSet<Option<&Path>> = HashSet::new();
        for (id, track) in self.controller.tracks() {
            let album = track
                .album()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            artists.insert(artist_group_key(
                self.controller.track_artist_id(*id),
                track,
            ));
            albums.insert(album);
            folders.insert(track.source().path().parent());
        }
        (
            self.controller.tracks().len(),
            artists.len(),
            albums.len(),
            folders.len(),
            self.controller.playlists().len(),
            self.controller
                .tracks()
                .iter()
                .filter(|(id, _)| self.controller.track_has_lyrics(*id))
                .count(),
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

        let count_for = |view: LibraryView| -> String {
            if view == LibraryView::Settings {
                return String::new();
            }
            match view {
                LibraryView::Tracks => counts.0,
                LibraryView::Artists => counts.1,
                LibraryView::Albums => counts.2,
                LibraryView::Folders => counts.3,
                LibraryView::Playlists => counts.4,
                LibraryView::Lyrics => counts.5,
                LibraryView::Dashboard => self.controller.tracks().len(),
                LibraryView::Collections => SmartCollection::ALL.len(),
                LibraryView::RecentlyAdded => self
                    .controller
                    .tracks()
                    .iter()
                    .filter(|(id, _)| {
                        self.controller
                            .track_created_unix_ms(*id)
                            .is_some_and(|created| created > 0)
                    })
                    .count(),
                LibraryView::RecentlyPlayed | LibraryView::MostPlayed => {
                    self.controller.playback_history().len()
                }
                LibraryView::Settings => 0,
            }
            .to_string()
        };

        let sections: [(&str, &[LibraryView]); 3] = [
            (
                "LIBRARY",
                &[
                    LibraryView::Tracks,
                    LibraryView::Artists,
                    LibraryView::Albums,
                    LibraryView::Folders,
                ],
            ),
            (
                "CURATED",
                &[
                    LibraryView::Playlists,
                    LibraryView::Collections,
                    LibraryView::RecentlyAdded,
                    LibraryView::RecentlyPlayed,
                    LibraryView::MostPlayed,
                ],
            ),
            (
                "TOOLS",
                &[
                    LibraryView::Lyrics,
                    LibraryView::Dashboard,
                    LibraryView::Settings,
                ],
            ),
        ];
        let mut nav_items = column![].spacing(2);
        for (index, (heading, views)) in sections.iter().enumerate() {
            if index > 0 {
                nav_items = nav_items.push(iced::widget::Space::new().height(4));
            }
            nav_items =
                nav_items.push(container(text(*heading).size(8).color(MUTED)).padding([3, 8]));
            for view in *views {
                nav_items = nav_items.push(
                    button(
                        row![
                            text(view.label()).size(12),
                            iced::widget::Space::new().width(Length::Fill),
                            text(count_for(*view)).size(9).color(MUTED),
                        ]
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([5, 8])
                    .style(nav_style(self.view == *view && !self.show_setup))
                    .on_press(Message::SelectView(*view)),
                );
            }
        }

        let library_state = match self.controller.storage_mode() {
            StorageMode::Legacy => "App index",
            StorageMode::Portable => "Portable (migration source)",
            StorageMode::Canonical => "Canonical library",
        };
        let library_label = format!(
            "{library_state} · {}",
            truncate_label(&self.active_library_label(), 16)
        );
        let library_detail = self.active_library_path().map_or_else(
            || "No persistent root".to_owned(),
            |path| path.display().to_string(),
        );
        let library_label = tooltip(
            text(library_label).size(9).color(TEXT),
            container(text(library_detail).size(9).color(TEXT))
                .padding([4, 7])
                .style(tooltip_style),
            tooltip::Position::Top,
        )
        .gap(6)
        .delay(Duration::from_millis(350));

        let mut nav = column![
            brand,
            iced::widget::Space::new().height(16),
            button(text("Import music").size(12))
                .width(Length::Fill)
                .padding([8, 10])
                .style(accent_button_style)
                .on_press(Message::ImportOpen),
            iced::widget::Space::new().height(5),
            button(text("Library setup").size(12))
                .width(Length::Fill)
                .padding([8, 10])
                .style(nav_style(self.show_setup))
                .on_press(Message::ToggleSetup),
            iced::widget::Space::new().height(10),
        ]
        .spacing(0);
        nav = nav.push(scrollable(nav_items).height(Length::Fill));
        nav = nav.push(iced::widget::Space::new().height(8));
        nav = nav.push(text("SPACE PLAY · N/P SKIP · Q QUEUE").size(8).color(MUTED));
        nav = nav.push(
            text("←/→ SEEK · SHIFT ±30S · / SEARCH")
                .size(8)
                .color(MUTED),
        );
        nav = nav.push(
            text("1-0 VIEWS · K SETTINGS · L SETUP")
                .size(8)
                .color(MUTED),
        );
        nav = nav.push(iced::widget::Space::new().height(10));
        nav = nav.push(text("ACTIVE LIBRARY").size(8).color(MUTED));
        nav = nav.push(library_label);

        container(nav)
            .width(Length::Fixed(200.0))
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
        if !self.show_setup
            && self.track_editor.is_none()
            && self.search.trim().is_empty()
            && !self.controller.tracks().is_empty()
            && (self.view == LibraryView::Tracks
                || self.selected_group.is_some()
                || self.collection.is_some()
                || matches!(
                    self.view,
                    LibraryView::RecentlyAdded
                        | LibraryView::RecentlyPlayed
                        | LibraryView::MostPlayed
                ))
        {
            content = content.push(self.library_controls());
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
        } else if let Some(editor) = &self.track_editor {
            (
                "Edit track metadata".to_owned(),
                format!(
                    "Track {} · Flux stores canonical metadata; the audio file and its tags stay untouched.",
                    editor.id
                ),
            )
        } else if !self.search.trim().is_empty() {
            (
                "Search results".to_owned(),
                "Artists, albums and tracks from the canonical catalogue.".to_owned(),
            )
        } else if self.view == LibraryView::Settings {
            (
                "Settings".to_owned(),
                "Playback preferences shared by the GUI and TUI.".to_owned(),
            )
        } else if self.view == LibraryView::Dashboard {
            (
                "Dashboard".to_owned(),
                "Canonical library summary and locally recorded listening statistics.".to_owned(),
            )
        } else if let Some(collection) = self.collection {
            (
                collection.label().to_owned(),
                format!(
                    "Smart collection · {} track(s) · {}",
                    self.filtered_tracks().len(),
                    collection.description()
                ),
            )
        } else if self.view == LibraryView::Collections {
            (
                "Smart Collections".to_owned(),
                "Dynamic catalogue queries; separate from manually ordered playlists.".to_owned(),
            )
        } else if matches!(
            self.view,
            LibraryView::RecentlyAdded | LibraryView::RecentlyPlayed | LibraryView::MostPlayed
        ) {
            (
                self.view.label().to_owned(),
                format!(
                    "{} track(s) · deterministic ordering",
                    self.filtered_tracks().len()
                ),
            )
        } else if let Some(track_id) = self.selected_lyrics {
            let title = self
                .controller
                .track(track_id)
                .map(|track| track.title().to_owned())
                .unwrap_or_else(|| "Lyrics".to_owned());
            (
                title,
                format!("Lyrics for track {track_id} · managed canonical text"),
            )
        } else if self.view == LibraryView::Lyrics {
            (
                "Lyrics".to_owned(),
                "Tracks with managed plain-text or synchronized lyrics.".to_owned(),
            )
        } else if let Some(playlist_id) = self.selected_playlist {
            (
                self.controller
                    .playlist(playlist_id)
                    .map(|playlist| playlist.name.clone())
                    .unwrap_or_else(|| "Playlist".to_owned()),
                format!(
                    "Playlist {playlist_id} · {count} entr(ies)",
                    count = self
                        .controller
                        .playlist(playlist_id)
                        .map_or(0, |playlist| playlist.track_ids.len())
                ),
            )
        } else if self.view == LibraryView::Playlists {
            (
                "Playlists".to_owned(),
                format!(
                    "{} canonical playlist(s)",
                    self.controller.playlists().len()
                ),
            )
        } else if let Some(artist_id) = self.selected_artist_id() {
            let name = self
                .controller
                .artist(artist_id)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {artist_id}"));
            let albums = self.controller.albums_owned_by(artist_id).len()
                + self.controller.albums_credited_for(artist_id).len();
            (
                name,
                format!("Artist detail · {albums} album association(s) · artist {artist_id}"),
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
        let compact = self.window_width < QUEUE_INLINE_MIN_WIDTH;
        let title = truncate_label(&title, if compact { 46 } else { 80 });
        let subtitle = truncate_label(&subtitle, if compact { 72 } else { 120 });

        let mut bar = row![].spacing(10).align_y(Alignment::Center);
        if self.selected_group.is_some() && !self.show_setup {
            bar = bar.push(
                button(text("‹ Back").size(11))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::SelectGroup(None)),
            );
        } else if self.collection.is_some() && self.view == LibraryView::Collections {
            bar = bar.push(
                button(text("‹ Collections").size(11))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::CollectionBack),
            );
        }
        bar = bar.push(
            column![
                text(title).size(24).color(TEXT),
                text(subtitle).size(11).color(MUTED)
            ]
            .spacing(2)
            .width(Length::Fill),
        );

        if !self.show_setup && self.track_editor.is_none() {
            bar = bar.push(
                text_input("Search artists, albums, tracks…", &self.search)
                    .id(iced::widget::Id::new("flux-search"))
                    .on_input(Message::SearchChanged)
                    .style(input_style)
                    .width(Length::Fixed(if compact { 190.0 } else { 250.0 })),
            );
            if !self.search.trim().is_empty() {
                bar = bar.push(
                    button(text("Clear").size(10))
                        .style(ghost_style)
                        .padding([5, 8])
                        .on_press(Message::ClearSearch),
                );
            }
        }
        if self.controller.current_id().is_some() && self.track_editor.is_none() {
            bar = bar.push(
                button(text("Lyrics").size(11))
                    .style(nav_style(self.view == LibraryView::Lyrics))
                    .padding([6, 10])
                    .on_press(Message::LyricsOpenCurrent),
            );
        }
        if self.track_editor.is_none() {
            bar = bar.push(
                button(text("Queue").size(11))
                    .style(nav_style(self.queue_placement() != QueuePlacement::Hidden))
                    .padding([6, 10])
                    .on_press(Message::ToggleQueue),
            );
        }
        if self.track_editor.is_none() {
            bar = bar.push(
                button(text(if self.show_setup { "Close" } else { "Setup" }).size(11))
                    .style(nav_style(self.show_setup))
                    .padding([6, 10])
                    .on_press(Message::ToggleSetup),
            );
        }

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
        if !self.controller.legacy_index_write_protected()
            && self.controller.portable_recovery().is_none()
        {
            line = line.push(
                button(text("Dismiss").size(10))
                    .style(ghost_style)
                    .padding([4, 8])
                    .on_press(Message::DismissNotice),
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

    fn settings_view(&self) -> Element<'_, Message> {
        let enabled = self.controller.crossfade_enabled();
        let configured = self.controller.crossfade_duration_ms();
        let shown = self.crossfade_preview.unwrap_or(configured);
        let seconds = shown as f32 / 1_000.0;
        let maximum = CROSSFADE_MAX_MS as f32 / 1_000.0;
        let configured_label = format!("{:.1} s", configured as f32 / 1_000.0);
        let shown_label = format!("{seconds:.1} s");

        let toggle = row![
            column![
                text("Crossfade").size(15).color(TEXT),
                text(if enabled {
                    "Natural transitions overlap using an equal-power envelope."
                } else {
                    "Natural transitions remain gapless."
                })
                .size(10)
                .color(MUTED),
            ]
            .spacing(3)
            .width(Length::Fill),
            toggler(enabled)
                .on_toggle(Message::CrossfadeEnabledChanged)
                .style(crossfade_toggler_style)
                .size(26.0),
        ]
        .spacing(18)
        .align_y(Alignment::Center);

        let duration: Element<'_, Message> = if enabled {
            let mut apply = button(text("Apply").size(10))
                .style(ghost_style)
                .padding([3, 8]);
            if self.crossfade_preview.is_some() {
                apply = apply.on_press(Message::CrossfadeDurationCommit);
            }
            column![
                row![
                    text("Duration").size(12).color(TEXT),
                    iced::widget::Space::new().width(Length::Fill),
                    text(shown_label).size(12).color(ACCENT),
                    apply,
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                slider(
                    0.0..=maximum,
                    seconds,
                    Message::CrossfadeDurationPreview,
                )
                .step(0.1_f32)
                .shift_step(0.5_f32)
                .width(Length::Fill)
                .style(seek_style)
                .on_release(Message::CrossfadeDurationCommit),
                text("0 s disables crossfade. Release the slider or press Apply to save. Changes apply to future sources; the current track keeps playing.")
                    .size(9)
                    .color(MUTED),
            ]
            .spacing(8)
            .into()
        } else {
            column![
                row![
                    text("Duration").size(12).color(MUTED),
                    iced::widget::Space::new().width(Length::Fill),
                    text(configured_label).size(12).color(MUTED),
                ]
                .align_y(Alignment::Center),
                text("Enable crossfade to edit the duration. The remembered value is retained.")
                    .size(9)
                    .color(MUTED),
            ]
            .spacing(8)
            .into()
        };

        let settings_path = self
            .controller
            .settings_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                "No shared settings file is configured; changes last for this session.".to_owned()
            });

        let card = container(
            column![
                toggle,
                iced::widget::Space::new().height(14),
                container(iced::widget::Space::new().height(1))
                    .width(Length::Fill)
                    .style(rule_style),
                iced::widget::Space::new().height(14),
                duration,
                iced::widget::Space::new().height(12),
                text("Crossfade applies only to eligible natural queue transitions. Short tracks, missing prepared decoders and unknown durations fall back to gapless playback. Exact skips, seeks and direct loads are unchanged.")
                    .size(10)
                    .color(MUTED),
                iced::widget::Space::new().height(10),
                text(format!("Shared setting: {settings_path}"))
                    .size(9)
                    .color(MUTED),
            ]
            .spacing(0),
        )
        .padding(20)
        .width(Length::Fill)
        .max_width(680.0)
        .style(card_style);

        column![text("PLAYBACK").size(9).color(MUTED), card,]
            .spacing(10)
            .width(Length::Fill)
            .into()
    }

    fn library_view(&self) -> Element<'_, Message> {
        if self.track_editor.is_some() {
            return self.track_editor_view();
        }
        if !self.search.trim().is_empty() {
            return self.search_results_view();
        }
        if self.view == LibraryView::Dashboard {
            return self.dashboard_view();
        }
        if self.view == LibraryView::Collections && self.collection.is_none() {
            return self.collections_view();
        }
        if self.view == LibraryView::Lyrics {
            return self.selected_lyrics.map_or_else(
                || self.lyrics_library_view(),
                |id| self.lyrics_detail_view_for_mode(id),
            );
        }
        if self.view == LibraryView::Playlists {
            return self.selected_playlist.map_or_else(
                || self.playlist_list_view(),
                |id| self.playlist_detail_view(id),
            );
        }
        if self.view == LibraryView::Settings {
            return self.settings_view();
        }
        if self.view == LibraryView::Artists
            && let Some(artist) = self.selected_artist_id()
        {
            return self.artist_detail_view(artist);
        }
        if self.controller.tracks().is_empty() {
            return self.empty_library();
        }

        if self.view == LibraryView::MostPlayed {
            column![
                self.most_played_breakdown(),
                self.track_list(self.filtered_tracks()),
            ]
            .spacing(8)
            .height(Length::Fill)
            .into()
        } else if matches!(
            self.view,
            LibraryView::RecentlyAdded | LibraryView::RecentlyPlayed | LibraryView::Collections
        ) {
            self.track_list(self.filtered_tracks())
        } else if self.view != LibraryView::Tracks && self.selected_group.is_none() {
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
                "Import music to begin. Flux infers metadata from filenames and folders, \
                 lets you review it, and copies every file into the canonical library."
            )
            .size(12)
            .color(MUTED)
            .width(Length::Fixed(420.0)),
            row![
                button(text("Import music").size(12))
                    .style(accent_button_style)
                    .padding([8, 14])
                    .on_press(Message::ImportOpen),
                button(text("Library setup").size(12))
                    .style(ghost_style)
                    .padding([8, 14])
                    .on_press(Message::ToggleSetup),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .align_x(Alignment::Center);

        container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into()
    }

    fn album_artwork<'a>(
        &'a self,
        album_id: Option<AlbumId>,
        size: f32,
        content_fit: ContentFit,
    ) -> Element<'a, Message> {
        let seed = album_id
            .and_then(|id| self.controller.album(id))
            .map(|album| album.title);
        let handle = album_id
            .and_then(|id| self.artwork.get(&id))
            .and_then(|entry| entry.handle.as_ref());
        match handle {
            Some(handle) => artwork_image(handle, size, content_fit),
            None => fallback_artwork(seed.as_deref(), size),
        }
    }

    fn artist_artwork<'a>(&'a self, artist_id: ArtistId, size: f32) -> Element<'a, Message> {
        let seed = self
            .controller
            .artist(artist_id)
            .map(|artist| artist.name().to_owned());
        let handle = self
            .artist_artwork
            .get(&artist_id)
            .and_then(|entry| entry.handle.as_ref());
        match handle {
            Some(handle) => artwork_image(handle, size, ContentFit::Cover),
            None => fallback_artwork(seed.as_deref(), size),
        }
    }

    fn track_artwork<'a>(
        &'a self,
        track_id: TrackId,
        album_id: Option<AlbumId>,
        size: f32,
        content_fit: ContentFit,
    ) -> Element<'a, Message> {
        if let Some(handle) = self
            .track_artwork
            .get(&track_id)
            .and_then(|entry| entry.handle.as_ref())
        {
            return artwork_image(handle, size, content_fit);
        }
        self.album_artwork(album_id, size, content_fit)
    }

    fn album_artwork_error(&self, album_id: AlbumId) -> Option<&str> {
        self.artwork
            .get(&album_id)
            .and_then(|entry| entry.error.as_deref())
    }

    fn artwork_targets(&self) -> Vec<AlbumId> {
        let mut targets = Vec::new();
        if let Some(artist_id) = self.selected_artist_id() {
            targets.extend(self.controller.albums_owned_by(artist_id));
            targets.extend(self.controller.albums_credited_for(artist_id));
        } else if let Some(album_id) = self.selected_album_id() {
            targets.push(album_id);
        } else if self.view == LibraryView::Albums && self.selected_group.is_none() {
            let groups = self.groups_for(LibraryView::Albums);
            let total = groups.len();
            let page = self.page.min(total.div_ceil(GROUP_PAGE_SIZE).max(1) - 1);
            let start = page * GROUP_PAGE_SIZE;
            let end = (start + GROUP_PAGE_SIZE).min(total);
            for (key, _) in &groups[start..end] {
                let (_, raw) = album_key_parts(key);
                if let Ok(id) = raw.parse::<u64>() {
                    targets.push(AlbumId::new(id));
                }
            }
        }
        if !self.search.trim().is_empty() {
            targets.extend(self.search_snapshot().albums.iter().map(|(id, _)| *id));
        }
        if let Some(album_id) = self
            .controller
            .current_id()
            .and_then(|id| self.controller.track_album_id(id))
        {
            targets.push(album_id);
        }
        targets.sort_by_key(|id| id.get());
        targets.dedup();
        targets
    }

    fn ensure_artwork_tasks(&mut self) -> Task<Message> {
        self.artwork
            .retain(|album_id, _| self.controller.album(*album_id).is_some());
        self.artist_artwork
            .retain(|artist_id, _| self.controller.artist(*artist_id).is_some());
        self.track_artwork
            .retain(|track_id, _| self.controller.track(*track_id).is_some());
        self.artwork_loading.retain(|key| match key.owner {
            ArtworkOwner::Album(album) => self.controller.album(album).is_some(),
            ArtworkOwner::Artist(artist) => self.controller.artist(artist).is_some(),
            ArtworkOwner::Track(track) => self.controller.track(track).is_some(),
        });
        let mut tasks = Vec::new();
        for album_id in self.artwork_targets() {
            let Some(path) = self.controller.album_cover_path(album_id) else {
                self.artwork.remove(&album_id);
                continue;
            };
            let key = artwork_key(ArtworkOwner::Album(album_id), path);
            if self
                .artwork
                .get(&album_id)
                .is_some_and(|entry| entry.key == key)
            {
                continue;
            }
            self.artwork.remove(&album_id);
            if !self.artwork_loading.insert(key.clone()) {
                continue;
            }
            let message_key = key.clone();
            let session = self.controller.library_session();
            tasks.push(Task::perform(
                async move { decode_artwork(&key) },
                move |result| Message::ArtworkLoaded(session, message_key, result),
            ));
            if tasks.len() >= 6 {
                break;
            }
        }
        let mut extra_targets: Vec<(ArtworkOwner, PathBuf)> = Vec::new();
        for track in self.visible_track_ids().into_iter().take(64) {
            if let Some(path) = self.controller.track_artwork_path(track) {
                extra_targets.push((ArtworkOwner::Track(track), path));
            }
        }
        if let Some(artist) = self.selected_artist_id()
            && let Some(path) = self.controller.artist_image_path(artist)
        {
            extra_targets.push((ArtworkOwner::Artist(artist), path));
        }
        if let Some(track) = self.controller.current_id()
            && let Some(path) = self.controller.track_artwork_path(track)
        {
            extra_targets.push((ArtworkOwner::Track(track), path));
        }
        if let Some(editor) = &self.track_editor
            && let Some(path) = self.controller.track_artwork_path(editor.id)
        {
            extra_targets.push((ArtworkOwner::Track(editor.id), path));
        }
        for (owner, path) in extra_targets {
            let key = artwork_key(owner, path);
            let cached = match owner {
                ArtworkOwner::Album(album) => self
                    .artwork
                    .get(&album)
                    .is_some_and(|entry| entry.key == key),
                ArtworkOwner::Artist(artist) => self
                    .artist_artwork
                    .get(&artist)
                    .is_some_and(|entry| entry.key == key),
                ArtworkOwner::Track(track) => self
                    .track_artwork
                    .get(&track)
                    .is_some_and(|entry| entry.key == key),
            };
            if cached {
                continue;
            }
            match owner {
                ArtworkOwner::Album(album) => {
                    self.artwork.remove(&album);
                }
                ArtworkOwner::Artist(artist) => {
                    self.artist_artwork.remove(&artist);
                }
                ArtworkOwner::Track(track) => {
                    self.track_artwork.remove(&track);
                }
            }
            if !self.artwork_loading.insert(key.clone()) {
                continue;
            }
            let message_key = key.clone();
            let session = self.controller.library_session();
            tasks.push(Task::perform(
                async move { decode_artwork(&key) },
                move |result| Message::ArtworkLoaded(session, message_key, result),
            ));
            if tasks.len() >= 8 {
                break;
            }
        }
        Task::batch(tasks)
    }

    fn track_editor_view(&self) -> Element<'_, Message> {
        let Some(editor) = &self.track_editor else {
            return self.empty_library();
        };
        let choices = self.editor_artist_choices();
        let selected = choices
            .iter()
            .find(|choice| choice.id == editor.artist_id)
            .cloned()
            .or_else(|| choices.first().cloned());
        let source_path = self
            .controller
            .track(editor.id)
            .map(|track| track.source().path().display().to_string())
            .unwrap_or_else(|| "track no longer in the library".to_owned());
        let audio_state = self.controller.track_audio_resource_state(editor.id);
        let missing = matches!(audio_state, Some(ResourceState::Missing));

        let artist_row = row![
            pick_list(choices, selected, |choice| Message::EditorArtistChanged(
                Some(choice)
            ))
            .text_size(12)
            .padding([7, 9])
            .width(Length::Fill),
        ]
        .spacing(8);
        let new_artist_row = row![
            text_input("New artist name…", &editor.new_artist)
                .on_input(Message::EditorNewArtistChanged)
                .on_submit(Message::EditorCreateArtist)
                .style(input_style)
                .width(Length::Fill),
            button(text("Create & select").size(11))
                .style(ghost_style)
                .padding([7, 10])
                .on_press_maybe(
                    (!editor.new_artist.trim().is_empty()).then_some(Message::EditorCreateArtist)
                ),
        ]
        .spacing(8);

        let credit_choices = self.artist_choices();
        let credit_selected = editor.credit_artist.and_then(|id| {
            credit_choices
                .iter()
                .find(|choice| choice.id == Some(id))
                .cloned()
        });
        let move_choices = self.editor_move_album_choices(editor.id);
        let move_selected = editor.move_destination.as_ref().and_then(|choice| {
            move_choices
                .iter()
                .find(|entry| entry.id == choice.id)
                .cloned()
        });

        let mut body = column![
            text("Flux-owned overrides").size(9).color(ACCENT),
            text("These values are stored in the canonical library. The audio file and its embedded tags are never rewritten.")
                .size(10)
                .color(MUTED),
            text(source_path).size(10).color(MUTED),
        ]
        .spacing(6);
        if missing {
            body = body.push(
                text("This managed audio file is missing. Metadata can still be edited; use Choose replacement / Review replacement below to repair it while preserving the TrackId and all references.")
                    .size(10)
                    .color(DANGER),
            );
        } else if let Some(ResourceState::Unavailable(reason)) = &audio_state {
            body = body.push(
                text(format!(
                    "This audio path exists but cannot be used ({reason}). Flux will not overwrite or delete an inaccessible resource; repair permissions or relink it first."
                ))
                .size(10)
                .color(DANGER),
            );
        }
        body = body.push(
            text_input("Track title…", &editor.title)
                .on_input(Message::EditorTitleChanged)
                .style(input_style)
                .width(Length::Fill),
        );
        if self.controller.track_album_id(editor.id).is_some() {
            body = body.push(
                text_input("Album…", &editor.album)
                    .on_input(Message::EditorAlbumChanged)
                    .style(input_style)
                    .width(Length::Fill),
            );
        } else {
            body = body.push(
                text("Standalone Track · no Album membership. Use Album membership below to add it to an Album.")
                    .size(9)
                    .color(ACCENT),
            );
        }
        body = body.push(
            row![
                text_input("Track number", &editor.track_number)
                    .on_input(Message::EditorTrackNumberChanged)
                    .style(input_style)
                    .width(Length::Fill),
                text_input("Disc number", &editor.disc_number)
                    .on_input(Message::EditorDiscNumberChanged)
                    .style(input_style)
                    .width(Length::Fill),
            ]
            .spacing(8),
        );
        body = body.push(text("ORDERED CREDITS").size(9).color(ACCENT));
        body = body.push(
            text(format!(
                "Physical owner: {} — ownership is separate from credits and cannot be changed here.",
                self.controller
                    .track_artist_id(editor.id)
                    .and_then(|artist| self.controller.artist(artist))
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned())
            ))
            .size(9)
            .color(MUTED),
        );
        let owner = self.controller.track_artist_id(editor.id);
        for (credit_index, artist_id) in editor.credits.iter().enumerate() {
            let name = self
                .controller
                .artist(*artist_id)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {artist_id}"));
            body = body.push(
                row![
                    text(format!(
                        "{}. {}{}",
                        credit_index + 1,
                        name,
                        if owner == Some(*artist_id) {
                            " · OWNER"
                        } else {
                            ""
                        }
                    ))
                    .size(10)
                    .color(if owner == Some(*artist_id) {
                        ACCENT
                    } else {
                        TEXT
                    })
                    .width(Length::Fill),
                    button(text("↑").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press(Message::EditorCreditMove(credit_index, -1)),
                    button(text("↓").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press(Message::EditorCreditMove(credit_index, 1)),
                    button(text("×").size(10))
                        .style(ghost_style)
                        .padding([4, 6])
                        .on_press_maybe(
                            (editor.credits.len() > 1 && owner != Some(*artist_id))
                                .then_some(Message::EditorCreditRemove(credit_index))
                        ),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            );
        }
        body = body.push(
            row![
                pick_list(credit_choices, credit_selected, |choice| {
                    Message::EditorCreditSelected(Some(choice))
                })
                .text_size(11)
                .padding([6, 8])
                .width(Length::Fill),
                button(text("Add credit").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::EditorCreditAdd),
            ]
            .spacing(8),
        );
        body = body.push(
            text("Transfer owner is a separate reviewed operation; changing this picker does not change metadata ownership.")
                .size(9)
                .color(MUTED),
        );
        body = body.push(artist_row);
        body = body.push(new_artist_row);
        body = body.push(
            text("Clear album or number fields to fall back to the current canonical values. The title always has a filename fallback.")
                .size(9)
                .color(MUTED),
        );
        body = body.push(text("REPLACE MANAGED AUDIO").size(9).color(ACCENT));
        body = body.push(
            text("The replacement is staged and verified; the old managed audio remains available until the catalogue commit. The source file is never modified. Save metadata changes before confirming; replacement closes the editor and does not apply unsaved fields.")
                .size(9)
                .color(MUTED),
        );
        body = body.push(
            text_input("Replacement audio path…", &editor.replacement_source)
                .on_input(Message::EditorReplacementChanged)
                .style(input_style)
                .width(Length::Fill),
        );
        body = body.push(
            row![
                button(text("Choose replacement…").size(10))
                    .style(accent_button_style)
                    .padding([6, 9])
                    .on_press(Message::EditorReplacementChoose),
                button(text("Review replacement").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::EditorPlanReplacement),
                button(text("Confirm replacement").size(10))
                    .style(accent_button_style)
                    .padding([6, 9])
                    .on_press_maybe(
                        editor
                            .replacement_plan
                            .is_some()
                            .then_some(Message::EditorConfirmReplacement)
                    ),
            ]
            .spacing(8),
        );
        if let Some(plan) = &editor.replacement_plan {
            body = body.push(
                text(format!(
                    "REVIEWED: Track {} · source {} · new managed path {} · {}",
                    plan.track_id(),
                    plan.source().display(),
                    plan.destination_audio_path().display_lossy(),
                    if plan.restores_missing_audio() {
                        "missing managed audio will be repaired; TrackId and references preserved"
                    } else {
                        "old audio fingerprinted and preserved until commit"
                    }
                ))
                .size(9)
                .color(Color::from_rgb8(0xff, 0xc2, 0x66)),
            );
        } else {
            body = body.push(
                text("Replacement is unavailable until a source is reviewed. No preview writes to the library.")
                    .size(9)
                    .color(MUTED),
            );
        }
        body = body.push(text("TRACK ARTWORK").size(9).color(ACCENT));
        let has_track_art = self.controller.track_artwork_path(editor.id).is_some();
        body = body.push(
            row![
                self.track_artwork(
                    editor.id,
                    self.controller.track_album_id(editor.id),
                    86.0,
                    ContentFit::Cover
                ),
                column![
                    text(if has_track_art {
                        "Using a Track-specific artwork override."
                    } else {
                        "Using Album artwork fallback (no Track override)."
                    })
                    .size(10)
                    .color(if has_track_art { TEXT } else { MUTED }),
                    text("Managed PNG/JPEG/WebP override. Removing it restores Album artwork without changing album metadata.")
                        .size(9)
                        .color(MUTED),
                ]
                .spacing(3)
                .width(Length::Fill),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        body = body.push(
            row![
                text_input("Track artwork path…", &editor.artwork_input)
                    .on_input(Message::TrackArtworkChanged)
                    .on_submit(Message::TrackArtworkApply)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Choose image…").size(10))
                    .style(accent_button_style)
                    .padding([6, 9])
                    .on_press(Message::TrackArtworkChoose),
                button(text("Assign / replace").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::TrackArtworkApply),
                button(
                    text(if editor.artwork_remove_confirm {
                        "Confirm restore album artwork"
                    } else {
                        "Remove override"
                    })
                    .size(10),
                )
                .style(danger_button_style)
                .padding([6, 9])
                .on_press_maybe(has_track_art.then_some(Message::TrackArtworkRemove)),
            ]
            .spacing(6),
        );
        body = body.push(text("DELETE THIS TRACK").size(9).color(DANGER));
        body = body.push(
            text("Removes this TrackId, its managed audio, metadata record, lyrics, Track artwork override, rating and queue entries. The album, artist and inherited album artwork are retained even when empty. Other tracks and playlist order are preserved. This is not a bulk delete.")
                .size(10)
                .color(MUTED),
        );
        if let Some(plan) = &editor.deletion_plan {
            body = body.push(
                text(format!(
                    "REVIEWED DELETE: {} · {} retained · {} · {}{}",
                    plan.title(),
                    plan.album_id().map_or_else(
                        || "standalone state".to_owned(),
                        |album| format!("album {album}")
                    ),
                    if plan.audio_present() {
                        "managed audio will be deleted after the catalogue commit"
                    } else {
                        "managed audio is already missing; no audio file will be removed"
                    },
                    if plan.playlists().is_empty() {
                        "no playlist references".to_owned()
                    } else {
                        format!("playlists updated: {}", plan.playlists().join(", "))
                    },
                    if matches!(audio_state, Some(ResourceState::Unavailable(_))) {
                        " · inaccessible files are kept and reported"
                    } else {
                        ""
                    }
                ))
                .size(9)
                .color(DANGER),
            );
            body = body.push(
                row![
                    button(text("Cancel deletion review").size(10))
                        .style(ghost_style)
                        .padding([6, 9])
                        .on_press(Message::EditorCancelDeletion),
                    button(text("Confirm delete track").size(10))
                        .style(danger_button_style)
                        .padding([6, 9])
                        .on_press(Message::EditorConfirmDeletion),
                ]
                .spacing(8),
            );
        } else {
            body = body.push(
                button(text("Review track deletion…").size(10))
                    .style(danger_button_style)
                    .padding([6, 9])
                    .on_press(Message::EditorPlanDeletion),
            );
        }
        body = body.push(text("ALBUM MEMBERSHIP").size(9).color(ACCENT));
        body = body.push(
            text("Save metadata changes before changing membership. The source album remains even if it becomes empty. Track artwork and lyrics follow the TrackId.")
                .size(9)
                .color(MUTED),
        );
        if self.controller.track_album_id(editor.id).is_some() {
            body = body.push(
                button(text("Review conversion to standalone Track…").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::EditorPlanDetach),
            );
        }
        if move_choices.is_empty() {
            body = body.push(
                text("No Album owned by this Track's physical owner can be selected. Ownership transfer is a separate operation.")
                    .size(9)
                    .color(MUTED),
            );
        } else {
            body = body.push(
                pick_list(move_choices, move_selected, |choice| {
                    Message::EditorMoveDestination(Some(choice))
                })
                .text_size(11)
                .padding([6, 8])
                .width(Length::Fill),
            );
            body = body.push(
                row![
                    text_input("Insertion position", &editor.move_position)
                        .on_input(Message::EditorMovePositionChanged)
                        .style(input_style)
                        .width(Length::Fill),
                    button(text("Review move").size(10))
                        .style(ghost_style)
                        .padding([6, 9])
                        .on_press(Message::EditorPlanMove),
                    button(text("Confirm move").size(10))
                        .style(accent_button_style)
                        .padding([6, 9])
                        .on_press_maybe(
                            editor
                                .move_plan
                                .is_some()
                                .then_some(Message::EditorConfirmMove)
                        ),
                ]
                .spacing(8),
            );
            if let Some(plan) = &editor.move_plan {
                body = body.push(
                    text(format!(
                        "REVIEWED: {} → {} at position {} · final order {} · {}",
                        plan.from_album_title.as_deref().unwrap_or("Standalone"),
                        plan.to_album_title.as_deref().unwrap_or("Standalone"),
                        plan.insertion_index + 1,
                        plan.destination_order
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                        if plan.to_album_id.is_some() {
                            "source album remains even if empty"
                        } else {
                            "source album becomes empty and is retained; Album artwork inheritance stops"
                        }
                    ))
                    .size(9)
                    .color(Color::from_rgb8(0xff, 0xc2, 0x66)),
                );
            } else {
                body = body.push(
                    text("The move is unavailable until reviewed. It never redirects to a different owner or deletes the source album.")
                        .size(9)
                        .color(MUTED),
                );
            }
        }
        if let Some(error) = &editor.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body = body.push(
            row![
                button(text("Save metadata").size(12))
                    .style(accent_button_style)
                    .padding([8, 14])
                    .on_press(Message::EditorSave),
                button(text("Transfer owner").size(11))
                    .style(ghost_style)
                    .padding([8, 12])
                    .on_press_maybe(
                        matches!(self.controller.storage_mode(), StorageMode::Canonical)
                            .then_some(Message::EditorTransferOwner)
                    ),
                button(text("Cancel").size(11))
                    .style(ghost_style)
                    .padding([8, 12])
                    .on_press(Message::EditorCancel),
            ]
            .spacing(8),
        );

        container(scrollable(body).height(Length::Fill))
            .padding(18)
            .width(Length::Fill)
            .style(card_style)
            .into()
    }

    fn credit_display(&self, id: TrackId) -> String {
        let credits = self.controller.track_credits(id);
        if credits.is_empty() {
            return self
                .controller
                .track(id)
                .map(display_artist)
                .unwrap_or_else(|| "Unknown artist".to_owned());
        }
        let names = credits
            .iter()
            .map(|artist| {
                self.controller
                    .artist(*artist)
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_else(|| format!("Artist {artist}"))
            })
            .collect::<Vec<_>>();
        let owner = self.controller.track_artist_id(id);
        let owner_name = owner
            .and_then(|artist| self.controller.artist(artist))
            .map(|artist| artist.name().to_owned());
        if owner_name.as_deref() != names.first().map(String::as_str) {
            format!(
                "{} · owner {}",
                names.join(" · "),
                owner_name.unwrap_or_default()
            )
        } else {
            names.join(" · ")
        }
    }

    fn group_keys_for(&self, view: LibraryView, id: TrackId, track: &Track) -> Vec<String> {
        match view {
            LibraryView::Artists => {
                let credits = self.controller.track_credits(id);
                if credits.is_empty() {
                    return vec![artist_group_key(self.controller.track_artist_id(id), track)];
                }
                let mut keys = Vec::with_capacity(credits.len());
                for artist in credits {
                    let name = self
                        .controller
                        .artist(artist)
                        .map(|value| value.name().to_owned())
                        .unwrap_or_else(|| format!("Artist {artist}"));
                    keys.push(format!("{name}{ARTIST_KEY_SEPARATOR}{artist}"));
                }
                keys
            }
            LibraryView::Albums => vec![album_group_key(self.controller.track_album_id(id), track)],
            _ => group_key(view, track).into_iter().collect(),
        }
    }

    fn groups_for(&self, view: LibraryView) -> Vec<(String, usize)> {
        if view == LibraryView::Albums {
            let mut groups = self
                .controller
                .albums()
                .into_iter()
                .map(|album| {
                    (
                        album_key_for_title(album.id, &album.title),
                        album.track_order.len(),
                    )
                })
                .collect::<Vec<_>>();
            let standalone = self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| self.controller.track_album_id(*id).is_none())
                .count();
            if standalone > 0 {
                groups.push((
                    format!("Standalone tracks{ALBUM_KEY_SEPARATOR}none"),
                    standalone,
                ));
            }
            return groups;
        }
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        for (id, track) in self.controller.tracks() {
            for key in self.group_keys_for(view, *id, track) {
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
            } else if self.view == LibraryView::Artists {
                let (_, artist_id) = artist_key_parts(key);
                if artist_id == "none" {
                    format!("{count} track(s) · no stable artist")
                } else {
                    format!("{count} track(s) · artist {artist_id}")
                }
            } else if self.view == LibraryView::Albums {
                let (_, album_id) = album_key_parts(key);
                if album_id == "none" {
                    format!("{count} standalone track(s) · no Album membership")
                } else {
                    format!("{count} track(s) · album {album_id}")
                }
            } else {
                format!("{count} track(s)")
            };
            let initial = title
                .chars()
                .find(|value| value.is_alphanumeric())
                .map(|value| value.to_uppercase().collect::<String>())
                .unwrap_or_else(|| "•".to_owned());
            let leading = if self.view == LibraryView::Albums {
                let (_, raw) = album_key_parts(key);
                raw.parse::<u64>().ok().map_or_else(
                    || fallback_artwork(Some(key), 38.0),
                    |id| self.album_artwork(Some(AlbumId::new(id)), 38.0, ContentFit::Cover),
                )
            } else {
                container(text(initial).size(15).color(TEXT))
                    .width(38)
                    .height(38)
                    .center(38)
                    .style(artwork_style(artwork_color(key)))
                    .into()
            };

            let row = row![
                leading,
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

    fn search_snapshot(&self) -> std::cell::RefMut<'_, SearchCache> {
        let generation = self.controller.generation();
        let mut cache = self.search_cache.borrow_mut();
        if cache.query != self.search || cache.generation != generation {
            let query = self.search.trim();
            if query.is_empty() {
                cache.artists.clear();
                cache.albums.clear();
                cache.track_ids.clear();
            } else {
                cache.artists = self.controller.search_artists(query);
                cache.albums = self.controller.search_albums(query);
                cache.track_ids = self
                    .controller
                    .search(query)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
            }
            cache.query = self.search.clone();
            cache.generation = generation;
        }
        cache
    }

    fn invalidate_search_cache(&mut self) {
        self.search_cache.borrow_mut().generation = u64::MAX;
    }

    fn filters_active(&self) -> bool {
        self.filter_favorites
            || self.filter_lyrics != TriFilter::Any
            || self.filter_artwork != TriFilter::Any
            || self.filter_release != ReleaseFilter::Any
            || self.filter_rating != RatingFilter::Any
            || self.sort_tracks != TrackSort::Default
            || self.sort_descending
    }

    fn smart_collection_ids(&self, collection: SmartCollection) -> Vec<TrackId> {
        match collection {
            SmartCollection::Favorites => self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| {
                    self.controller
                        .rating(*id)
                        .is_some_and(|rating| rating.favorite)
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            SmartCollection::HighlyRated => self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| {
                    self.controller
                        .rating(*id)
                        .and_then(|rating| rating.rating)
                        .is_some_and(|rating| rating >= 4)
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            SmartCollection::RecentlyAdded => {
                let mut entries = self
                    .controller
                    .tracks()
                    .iter()
                    .filter_map(|(id, _)| {
                        self.controller
                            .track_created_unix_ms(*id)
                            .filter(|created| *created > 0)
                            .map(|created| (*id, created))
                    })
                    .collect::<Vec<_>>();
                entries.sort_by(|(left_id, left), (right_id, right)| {
                    right.cmp(left).then_with(|| left_id.cmp(right_id))
                });
                entries.into_iter().map(|(id, _)| id).collect()
            }
            SmartCollection::RecentlyPlayed => {
                let mut entries = self
                    .controller
                    .playback_history()
                    .entries()
                    .collect::<Vec<_>>();
                entries.retain(|(id, _)| self.controller.track(*id).is_some());
                entries.sort_by(|(left_id, left), (right_id, right)| {
                    right
                        .last_played_unix_ms
                        .cmp(&left.last_played_unix_ms)
                        .then_with(|| left_id.cmp(right_id))
                });
                entries.into_iter().map(|(id, _)| id).collect()
            }
            SmartCollection::MostPlayed => {
                let mut entries = self
                    .controller
                    .playback_history()
                    .entries()
                    .collect::<Vec<_>>();
                entries.retain(|(id, _)| self.controller.track(*id).is_some());
                entries.sort_by(|(left_id, left), (right_id, right)| {
                    right
                        .play_count
                        .cmp(&left.play_count)
                        .then_with(|| right.last_played_unix_ms.cmp(&left.last_played_unix_ms))
                        .then_with(|| left_id.cmp(right_id))
                });
                entries.into_iter().map(|(id, _)| id).collect()
            }
            SmartCollection::NoLyrics => self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| !self.controller.track_has_lyrics(*id))
                .map(|(id, _)| *id)
                .collect(),
            SmartCollection::SyncedLyrics => self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| self.lyrics_kind_for(*id) == LyricsKind::Synced)
                .map(|(id, _)| *id)
                .collect(),
            SmartCollection::NoArtwork => self
                .controller
                .tracks()
                .iter()
                .filter(|(id, _)| {
                    self.controller
                        .track_album_id(*id)
                        .and_then(|album| self.controller.album(album))
                        .is_none_or(|album| album.cover.is_none())
                })
                .map(|(id, _)| *id)
                .collect(),
        }
    }

    fn filtered_tracks(&self) -> Vec<(TrackId, &Track)> {
        let album_mode = self.selected_album_id();
        let special_ids = match self.view {
            LibraryView::Collections => self
                .collection
                .map(|collection| self.smart_collection_ids(collection)),
            LibraryView::RecentlyAdded => {
                Some(self.smart_collection_ids(SmartCollection::RecentlyAdded))
            }
            LibraryView::RecentlyPlayed => {
                Some(self.smart_collection_ids(SmartCollection::RecentlyPlayed))
            }
            LibraryView::MostPlayed => Some(self.smart_collection_ids(SmartCollection::MostPlayed)),
            _ => None,
        };
        let mut rows = if let Some(ids) = special_ids {
            ids.into_iter()
                .filter_map(|id| self.controller.track(id).map(|track| (id, track)))
                .collect::<Vec<_>>()
        } else if let Some(album_id) = album_mode {
            self.controller
                .album_tracks(album_id)
                .into_iter()
                .filter_map(|id| self.controller.track(id).map(|track| (id, track)))
                .collect::<Vec<_>>()
        } else {
            self.controller
                .tracks()
                .iter()
                .filter(|(id, track)| {
                    self.selected_group.as_ref().is_none_or(|group| {
                        self.group_keys_for(self.view, *id, track)
                            .iter()
                            .any(|key| key == group)
                    })
                })
                .map(|(id, track)| (*id, track))
                .collect::<Vec<_>>()
        };
        rows.retain(|(id, _)| {
            self.controller
                .rating(*id)
                .is_some_and(|rating| rating.favorite)
                || !self.filter_favorites
        });
        rows.retain(|(id, _)| {
            self.filter_lyrics == TriFilter::Any
                || (self.filter_lyrics == TriFilter::With) == self.controller.track_has_lyrics(*id)
        });
        rows.retain(|(id, _)| {
            let album = self
                .controller
                .track_album_id(*id)
                .and_then(|album_id| self.controller.album(album_id));
            if !self.filter_release.matches(
                album
                    .as_ref()
                    .map_or(ReleaseType::Other, |a| a.release_type),
            ) {
                return false;
            }
            self.filter_artwork == TriFilter::Any
                || (self.filter_artwork == TriFilter::With)
                    == album.as_ref().is_some_and(|album| album.cover.is_some())
        });
        if let Some(minimum) = self.filter_rating.minimum() {
            rows.retain(|(id, _)| {
                self.controller
                    .rating(*id)
                    .and_then(|rating| rating.rating)
                    .is_some_and(|rating| rating >= minimum)
            });
        }

        if album_mode.is_none() && self.sort_tracks != TrackSort::Default {
            rows.sort_by(|(left_id, left), (right_id, right)| {
                let mut ordering = match self.sort_tracks {
                    TrackSort::Default => std::cmp::Ordering::Equal,
                    TrackSort::Title => left
                        .title()
                        .to_lowercase()
                        .cmp(&right.title().to_lowercase()),
                    TrackSort::Artist => display_artist(left)
                        .to_lowercase()
                        .cmp(&display_artist(right).to_lowercase()),
                    TrackSort::Album => display_album(left)
                        .to_lowercase()
                        .cmp(&display_album(right).to_lowercase()),
                    TrackSort::TrackNumber => (left.disc_number(), left.track_number())
                        .cmp(&(right.disc_number(), right.track_number())),
                    TrackSort::Favorites => {
                        let left_rating = self.controller.rating(*left_id);
                        let right_rating = self.controller.rating(*right_id);
                        right_rating
                            .is_some_and(|rating| rating.favorite)
                            .cmp(&left_rating.is_some_and(|rating| rating.favorite))
                            .then_with(|| {
                                right_rating
                                    .and_then(|rating| rating.rating)
                                    .cmp(&left_rating.and_then(|rating| rating.rating))
                            })
                    }
                    TrackSort::Rating => self
                        .controller
                        .rating(*left_id)
                        .and_then(|rating| rating.rating)
                        .cmp(
                            &self
                                .controller
                                .rating(*right_id)
                                .and_then(|rating| rating.rating),
                        ),
                    TrackSort::PlayCount => self
                        .controller
                        .track_play_count(*left_id)
                        .cmp(&self.controller.track_play_count(*right_id)),
                    TrackSort::LastPlayed => self
                        .controller
                        .track_last_played_unix_ms(*left_id)
                        .cmp(&self.controller.track_last_played_unix_ms(*right_id)),
                    TrackSort::DateAdded => self
                        .controller
                        .track_created_unix_ms(*left_id)
                        .cmp(&self.controller.track_created_unix_ms(*right_id)),
                    TrackSort::Duration => left.duration().cmp(&right.duration()),
                };
                if self.sort_descending {
                    ordering = ordering.reverse();
                }
                ordering.then_with(|| left_id.cmp(right_id))
            });
        }
        rows
    }

    fn visible_track_ids(&self) -> Vec<TrackId> {
        if !self.search.trim().is_empty() {
            return self
                .search_snapshot()
                .track_ids
                .iter()
                .take(SEARCH_RESULT_LIMIT)
                .copied()
                .collect();
        }
        let rows = self.filtered_tracks();
        let pages = rows.len().div_ceil(TRACK_PAGE_SIZE).max(1);
        let page = self.page.min(pages - 1);
        let start = page * TRACK_PAGE_SIZE;
        let end = (start + TRACK_PAGE_SIZE).min(rows.len());
        rows[start..end].iter().map(|(id, _)| *id).collect()
    }

    fn playlist_choices(&self) -> Vec<PlaylistChoice> {
        self.controller
            .playlists()
            .into_iter()
            .map(|playlist| PlaylistChoice {
                id: playlist.id,
                label: format!("{} · {} track(s)", playlist.name, playlist.track_ids.len()),
            })
            .collect()
    }

    fn confirm_batch_review(&mut self) {
        let Some(mut review) = self.batch_review.take() else {
            return;
        };
        if review.track_ids.is_empty() {
            review.error = Some("No tracks remain selected.".to_owned());
            self.batch_review = Some(review);
            return;
        }
        let result = if let Some(playlist_id) = review.playlist_id {
            self.controller
                .add_tracks_to_playlist(
                    playlist_id,
                    &review.track_ids,
                    review
                        .expected_playlist_track_ids
                        .as_deref()
                        .unwrap_or_default(),
                )
                .map(|count| format!("Added {count} track(s) to {}.", review.playlist_label))
        } else {
            let name = review.new_playlist_name.trim();
            if name.is_empty() {
                review.error = Some("Playlist name must not be empty.".to_owned());
                self.batch_review = Some(review);
                return;
            }
            self.controller
                .create_playlist_from_tracks(name, &review.track_ids)
                .map(|id| {
                    self.view = LibraryView::Playlists;
                    self.selected_playlist = Some(id);
                    self.playlist_name_input = name.to_owned();
                    self.selected_group = None;
                    self.search.clear();
                    format!(
                        "Created playlist {id} with {} track(s).",
                        review.track_ids.len()
                    )
                })
        };
        match result {
            Ok(message) => {
                self.selected_tracks.clear();
                self.lifecycle_msg = Some(message);
            }
            Err(error) => {
                review.error = Some(error);
                self.batch_review = Some(review);
            }
        }
    }

    fn batch_action_bar(&self) -> Option<Element<'_, Message>> {
        if self.selected_tracks.is_empty() {
            return None;
        }
        let playlists = self.playlist_choices();
        let mut row = row![
            text(format!("{} selected", self.selected_tracks.len()))
                .size(10)
                .color(ACCENT),
            button(text("Select all visible").size(10))
                .style(ghost_style)
                .padding([5, 8])
                .on_press(Message::TrackSelectionSelectAll),
            button(text("Clear").size(10))
                .style(ghost_style)
                .padding([5, 8])
                .on_press(Message::TrackSelectionClear),
            button(text("Favorite").size(10))
                .style(ghost_style)
                .padding([5, 8])
                .on_press(Message::BatchFavorites(true)),
            button(text("Unfavorite").size(10))
                .style(ghost_style)
                .padding([5, 8])
                .on_press(Message::BatchFavorites(false)),
            text("RATE").size(8).color(MUTED),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        for rating in 1..=5u8 {
            row = row.push(
                button(text(format!("{rating}★")).size(9))
                    .style(ghost_style)
                    .padding([5, 7])
                    .on_press(Message::BatchRating(Some(rating))),
            );
        }
        row = row.push(
            button(text("Clear").size(9))
                .style(ghost_style)
                .padding([5, 7])
                .on_press(Message::BatchRating(None)),
        );
        if playlists.is_empty() {
            row = row.push(
                button(text("Create playlist…").size(10))
                    .style(accent_button_style)
                    .padding([5, 8])
                    .on_press(Message::BatchPlaylistCreateOpen),
            );
        } else {
            row = row.push(
                pick_list(playlists, None::<PlaylistChoice>, |choice| {
                    Message::BatchPlaylistAdd(choice.id)
                })
                .placeholder("Add to playlist…")
                .text_size(10)
                .padding([5, 7])
                .width(Length::Fixed(190.0)),
            );
            row = row.push(
                button(text("Create playlist…").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::BatchPlaylistCreateOpen),
            );
        }
        Some(
            container(row.wrap())
                .padding([7, 9])
                .width(Length::Fill)
                .style(card_style)
                .into(),
        )
    }

    fn library_controls(&self) -> Element<'_, Message> {
        let album_mode = self.selected_album_id().is_some();
        let mut controls = row![
            text("FILTER").size(8).color(MUTED),
            button(text("Favorites").size(10))
                .style(nav_style(self.filter_favorites))
                .padding([5, 8])
                .on_press(Message::FilterFavoritesToggled),
            button(text(format!("Lyrics: {}", self.filter_lyrics)).size(10))
                .style(nav_style(self.filter_lyrics != TriFilter::Any))
                .padding([5, 8])
                .on_press(Message::FilterLyricsCycle),
            button(text(format!("Artwork: {}", self.filter_artwork)).size(10))
                .style(nav_style(self.filter_artwork != TriFilter::Any))
                .padding([5, 8])
                .on_press(Message::FilterArtworkCycle),
            pick_list(
                vec![
                    ReleaseFilter::Any,
                    ReleaseFilter::Album,
                    ReleaseFilter::Ep,
                    ReleaseFilter::Single,
                    ReleaseFilter::Compilation,
                    ReleaseFilter::Other,
                ],
                Some(self.filter_release),
                Message::FilterReleaseChanged,
            )
            .text_size(10)
            .padding([5, 7]),
            pick_list(
                vec![
                    RatingFilter::Any,
                    RatingFilter::One,
                    RatingFilter::Two,
                    RatingFilter::Three,
                    RatingFilter::Four,
                    RatingFilter::Five,
                ],
                Some(self.filter_rating),
                Message::FilterRatingChanged,
            )
            .text_size(10)
            .padding([5, 7]),
            text("SORT").size(8).color(MUTED),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if album_mode {
            controls = controls.push(text("Album order preserved").size(9).color(MUTED));
        } else {
            controls = controls.push(
                pick_list(
                    vec![
                        TrackSort::Default,
                        TrackSort::Title,
                        TrackSort::Artist,
                        TrackSort::Album,
                        TrackSort::TrackNumber,
                        TrackSort::Favorites,
                        TrackSort::Rating,
                        TrackSort::PlayCount,
                        TrackSort::LastPlayed,
                        TrackSort::DateAdded,
                        TrackSort::Duration,
                    ],
                    Some(self.sort_tracks),
                    Message::SortTracksChanged,
                )
                .text_size(10)
                .padding([5, 7]),
            );
            controls = controls.push(
                button(text(if self.sort_descending { "Desc" } else { "Asc" }).size(9))
                    .style(nav_style(self.sort_descending))
                    .padding([5, 7])
                    .on_press(Message::SortDirectionToggled),
            );
        }
        if self.filters_active() {
            controls = controls.push(
                button(text("Reset filters").size(10))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::ResetLibraryFilters),
            );
        }
        container(controls.wrap())
            .padding([6, 8])
            .width(Length::Fill)
            .style(card_style)
            .into()
    }

    fn search_results_view(&self) -> Element<'_, Message> {
        let cache = self.search_snapshot().clone();
        if cache.artists.is_empty() && cache.albums.is_empty() && cache.track_ids.is_empty() {
            return container(
                column![
                    text(format!("No catalogue matches for “{}”.", self.search.trim()))
                        .size(15)
                        .color(TEXT),
                    text("Search is case-insensitive and matches artist names, album titles and track titles.")
                        .size(10)
                        .color(MUTED),
                    button(text("Clear search").size(11))
                        .style(accent_button_style)
                        .padding([7, 11])
                        .on_press(Message::ClearSearch),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into();
        }

        let mut body = column![
            row![
                column![
                    text(format!("Search results for “{}”", self.search.trim()))
                        .size(20)
                        .color(TEXT),
                    text(format!(
                        "{} artist(s) · {} album(s) · {} track(s)",
                        cache.artists.len(),
                        cache.albums.len(),
                        cache.track_ids.len()
                    ))
                    .size(10)
                    .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Clear search").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::ClearSearch),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if let Some(batch) = self.batch_action_bar() {
            body = body.push(batch);
        }

        if !cache.artists.is_empty() {
            body = body.push(text("ARTISTS").size(9).color(ACCENT));
            let mut list = column![].spacing(4);
            for artist in cache.artists.iter().take(SEARCH_RESULT_LIMIT) {
                list = list.push(
                    button(
                        row![
                            fallback_artwork(Some(artist.name()), 32.0),
                            column![
                                text(artist.name().to_owned()).size(13).color(TEXT),
                                text(format!("Artist {}", artist.id())).size(9).color(MUTED),
                            ]
                            .spacing(1),
                        ]
                        .spacing(9)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([5, 7])
                    .style(group_style)
                    .on_press(Message::SearchNavigateArtist(artist.id())),
                );
            }
            body = body.push(list);
        }
        if !cache.albums.is_empty() {
            body = body.push(text("ALBUMS").size(9).color(ACCENT));
            let mut list = column![].spacing(4);
            for (album_id, title) in cache.albums.iter().take(SEARCH_RESULT_LIMIT) {
                let owner = self
                    .controller
                    .album_owner(*album_id)
                    .and_then(|id| self.controller.artist(id))
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_else(|| "unknown owner".to_owned());
                list = list.push(
                    button(
                        row![
                            self.album_artwork(Some(*album_id), 32.0, ContentFit::Cover),
                            column![
                                text(title.clone()).size(13).color(TEXT),
                                text(format!("Album {} · {owner}", album_id))
                                    .size(9)
                                    .color(MUTED),
                            ]
                            .spacing(1),
                        ]
                        .spacing(9)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([5, 7])
                    .style(group_style)
                    .on_press(Message::SearchNavigateAlbum(*album_id)),
                );
            }
            body = body.push(list);
        }
        if !cache.track_ids.is_empty() {
            body = body.push(text("TRACKS").size(9).color(ACCENT));
            let mut list = column![].spacing(2);
            let current = self.controller.current_id();
            let editable = matches!(
                self.controller.storage_mode(),
                StorageMode::Portable | StorageMode::Canonical
            );
            for (index, id) in cache.track_ids.iter().take(SEARCH_RESULT_LIMIT).enumerate() {
                let Some(track) = self.controller.track(*id) else {
                    continue;
                };
                let resource = resource_badge(self.controller.track_audio_resource_state(*id));
                list = list.push(
                    row![
                        track_row(
                            index + 1,
                            *id,
                            track,
                            &self.credit_display(*id),
                            TrackRowState {
                                current,
                                resource,
                                editable,
                                checked: self.selected_tracks.contains(id),
                                focused: self.focused_track == Some(*id),
                                rating: self
                                    .controller
                                    .rating(*id)
                                    .and_then(|rating| rating.rating),
                                favorite: self
                                    .controller
                                    .rating(*id)
                                    .is_some_and(|rating| rating.favorite),
                            },
                        ),
                        button(text("Locate").size(9))
                            .style(ghost_style)
                            .padding([5, 7])
                            .on_press(Message::SearchNavigateTrack(*id)),
                    ]
                    .spacing(2)
                    .align_y(Alignment::Center),
                );
            }
            body = body.push(list);
        }
        body = body.push(
            text(format!(
                "Showing at most {SEARCH_RESULT_LIMIT} results per type. Refine the query to narrow matches."
            ))
            .size(9)
            .color(MUTED),
        );
        scrollable(body).height(Length::Fill).into()
    }

    fn playlist_list_view(&self) -> Element<'_, Message> {
        let playlists = self.controller.playlists();
        let mut body = column![
            row![
                column![
                    text("Playlists").size(22).color(TEXT),
                    text(format!("{} canonical playlist(s)", playlists.len()))
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("New playlist").size(11))
                    .style(accent_button_style)
                    .padding([7, 11])
                    .on_press(Message::PlaylistCreateOpen),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if let Some(error) = &self.playlist_error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        if playlists.is_empty() {
            body = body.push(
                container(
                    column![
                        text("No playlists yet").size(18).color(TEXT),
                        text("Create an empty playlist or select tracks from the library and use Create playlist… in the selection bar.")
                            .size(11)
                            .color(MUTED),
                        button(text("Create playlist").size(11))
                            .style(accent_button_style)
                            .padding([7, 11])
                            .on_press(Message::PlaylistCreateOpen),
                    ]
                    .spacing(8)
                    .align_x(Alignment::Center),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .style(card_style),
            );
            return body.into();
        }
        let mut rows = column![].spacing(5);
        for playlist in playlists {
            let preview = playlist
                .track_ids
                .iter()
                .take(3)
                .filter_map(|id| self.controller.track(*id).map(Track::title))
                .collect::<Vec<_>>()
                .join(" · ");
            let subtitle = if preview.is_empty() {
                "Empty playlist".to_owned()
            } else {
                format!("{} track(s) · {preview}", playlist.track_ids.len())
            };
            rows = rows.push(
                button(
                    row![
                        fallback_artwork(Some(&playlist.name), 38.0),
                        column![
                            text(playlist.name.clone()).size(14).color(TEXT),
                            text(subtitle).size(10).color(MUTED),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        text("›").size(20).color(MUTED),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding(8)
                .style(group_style)
                .on_press(Message::PlaylistSelect(playlist.id)),
            );
        }
        body = body.push(scrollable(rows).height(Length::Fill));
        body.height(Length::Fill).into()
    }

    fn playlist_detail_view(&self, id: PlaylistId) -> Element<'_, Message> {
        let Some(playlist) = self.controller.playlist(id).cloned() else {
            return text("Playlist is no longer in the library.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let count = playlist.track_ids.len();
        let selected_count = self.playlist_entry_selection.len();
        let mut body = column![
            row![
                button(text("‹ Back").size(11))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::PlaylistBack),
                column![
                    text(playlist.name.clone()).size(22).color(TEXT),
                    text(format!(
                        "Playlist {} · {count} entr(ies) · stable TrackIds · duplicates preserved",
                        playlist.id
                    ))
                    .size(10)
                    .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Play").size(11))
                    .style(accent_button_style)
                    .padding([7, 10])
                    .on_press_maybe((count > 0).then_some(Message::PlaylistPlay)),
                button(text("Shuffle").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe((count > 0).then_some(Message::PlaylistShuffle)),
                button(text("Add to queue").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe((count > 0).then_some(Message::PlaylistEnqueue)),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if let Some(error) = &self.playlist_error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body = body.push(
            container(
                column![
                    text("PLAYLIST DETAILS").size(9).color(ACCENT),
                    row![
                        text_input("Playlist name…", &self.playlist_name_input)
                            .on_input(Message::PlaylistRenameChanged)
                            .on_submit(Message::PlaylistRenameSave)
                            .style(input_style)
                            .width(Length::Fill),
                        button(text("Rename").size(10))
                            .style(accent_button_style)
                            .padding([6, 9])
                            .on_press(Message::PlaylistRenameSave),
                        button(
                            text(if self.playlist_clear_confirm {
                                "Confirm clear"
                            } else {
                                "Clear playlist"
                            })
                            .size(10),
                        )
                        .style(danger_button_style)
                        .padding([6, 9])
                        .on_press_maybe((count > 0).then_some(Message::PlaylistClear)),
                        button(
                            text(if self.playlist_delete_confirm {
                                "Confirm delete playlist"
                            } else {
                                "Delete playlist"
                            })
                            .size(10),
                        )
                        .style(danger_button_style)
                        .padding([6, 9])
                        .on_press(Message::PlaylistDelete),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                    text("Renaming and clearing preserve the PlaylistId. Clearing never deletes canonical tracks.")
                        .size(9)
                        .color(MUTED),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
        );
        if selected_count > 0 {
            body = body.push(
                container(
                    row![
                        text(format!("{selected_count} entr(ies) selected"))
                            .size(10)
                            .color(ACCENT),
                        button(text("Select all").size(10))
                            .style(ghost_style)
                            .padding([5, 8])
                            .on_press(Message::PlaylistEntrySelectAll),
                        button(text("Clear selection").size(10))
                            .style(ghost_style)
                            .padding([5, 8])
                            .on_press(Message::PlaylistEntryClearSelection),
                        button(text("Add selected to queue").size(10))
                            .style(ghost_style)
                            .padding([5, 8])
                            .on_press(Message::PlaylistEnqueueSelected),
                        button(
                            text(if self.playlist_remove_confirm {
                                "Confirm remove selected"
                            } else {
                                "Remove selected"
                            })
                            .size(10),
                        )
                        .style(danger_button_style)
                        .padding([5, 8])
                        .on_press(Message::PlaylistRemoveSelected),
                    ]
                    .spacing(6)
                    .wrap(),
                )
                .padding([7, 9])
                .width(Length::Fill)
                .style(card_style),
            );
        } else if count > 0 {
            body = body.push(
                row![
                    text("Drag rows to reorder · select entries for batch actions")
                        .size(9)
                        .color(MUTED),
                    iced::widget::Space::new().width(Length::Fill),
                    button(text("Select all").size(10))
                        .style(ghost_style)
                        .padding([5, 8])
                        .on_press(Message::PlaylistEntrySelectAll),
                ]
                .align_y(Alignment::Center),
            );
        }

        if count == 0 {
            body = body.push(
                container(
                    column![
                        text("This playlist is empty.").size(15).color(TEXT),
                        text("Add tracks through the library selection bar, album detail, or global search. Playlist entries never duplicate audio files.")
                            .size(10)
                            .color(MUTED),
                    ]
                    .spacing(5),
                )
                .padding(12)
                .width(Length::Fill)
                .style(card_style),
            );
            return body.into();
        }

        let current = self.controller.current_id();
        let dragged = self.playlist_drag.as_ref().map(|drag| drag.from);
        let target_gap = self.playlist_drag.as_ref().and_then(|drag| drag.target_gap);
        let pages = count.div_ceil(PLAYLIST_PAGE_SIZE).max(1);
        let page = self.page.min(pages - 1);
        let start = page * PLAYLIST_PAGE_SIZE;
        let end = (start + PLAYLIST_PAGE_SIZE).min(count);
        let track_lookup = self
            .controller
            .tracks()
            .iter()
            .map(|(id, track)| (*id, track))
            .collect::<std::collections::HashMap<_, _>>();
        let mut rows = column![].spacing(0);
        for position in start..end {
            let track_id = playlist.track_ids[position];
            rows = rows.push(queue_gap_line(target_gap == Some(position)));
            let Some(track) = track_lookup.get(&track_id).copied() else {
                rows = rows.push(
                    row![
                        text(format!("{}", position + 1)).size(10).color(MUTED),
                        text(format!("Missing track {track_id}"))
                            .size(12)
                            .color(DANGER),
                        iced::widget::Space::new().width(Length::Fill),
                        button(text("Remove entry").size(10))
                            .style(danger_button_style)
                            .padding([5, 8])
                            .on_press(Message::PlaylistEntryRemove(position)),
                    ]
                    .spacing(8)
                    .padding([6, 8]),
                );
                continue;
            };
            rows = rows.push(playlist_entry_row(
                position,
                track_id,
                track,
                current,
                self.playlist_entry_selection.contains(&position),
                dragged == Some(position),
                resource_badge(self.controller.track_audio_resource_state(track_id)),
            ));
        }
        rows = rows.push(queue_gap_line(target_gap == Some(end)));
        body = body.push(
            mouse_area(scrollable(rows).height(Length::Fill)).on_exit(Message::PlaylistDragLeave),
        );
        body = body.push(pagination_footer(page, pages, start, end, count));
        body.height(Length::Fill).into()
    }

    fn playlist_create_overlay(&self) -> Element<'_, Message> {
        if !self.playlist_create_open {
            return iced::widget::Space::new().into();
        }
        let mut content = column![
            text("Create playlist").size(22).color(TEXT),
            text("Creates an empty playlist with a stable PlaylistId. Add tracks from the library selection bar or playlist detail later.")
                .size(10)
                .color(MUTED),
            text_input("Playlist name…", &self.playlist_create_name)
                .on_input(Message::PlaylistCreateNameChanged)
                .on_submit(Message::PlaylistCreateConfirm)
                .style(input_style)
                .width(Length::Fill),
        ]
        .spacing(8);
        if let Some(error) = &self.playlist_error {
            content = content.push(text(error).size(10).color(DANGER));
        }
        content = content.push(
            row![
                button(text("Create").size(12))
                    .style(accent_button_style)
                    .padding([8, 14])
                    .on_press(Message::PlaylistCreateConfirm),
                button(text("Cancel").size(11))
                    .style(ghost_style)
                    .padding([8, 12])
                    .on_press(Message::PlaylistCreateCancel),
            ]
            .spacing(8),
        );
        container(
            container(content)
                .padding(18)
                .width(Length::Fill)
                .max_width(520.0)
                .style(import_panel_style),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .style(modal_backdrop_style)
        .into()
    }

    fn open_lyrics_editor(&mut self) {
        let Some(id) = self.selected_lyrics else {
            self.lyrics_error = Some("Select a track first.".to_owned());
            return;
        };
        let Some(_) = self.controller.track(id) else {
            self.lyrics_error = Some("The lyrics track is no longer in the library.".to_owned());
            return;
        };
        let expected_text = self.controller.track_lyrics(id);
        let document = match expected_text.as_deref() {
            Some(raw) => match LyricsDocument::parse(raw) {
                Ok(document) => document,
                Err(error) => {
                    self.lyrics_error = Some(format!(
                        "Existing lyrics are not valid LRC/TXT: {error}. Import valid lyrics or remove them before editing."
                    ));
                    return;
                }
            },
            None => LyricsDocument::default(),
        };
        let original_synced = document.kind() == LyricsKind::Synced;
        let mut lines = document
            .lines
            .into_iter()
            .map(|line| LyricsEditLine {
                text: line.text,
                timestamp: line
                    .timestamp_ms
                    .map(format_timestamp_ms)
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(LyricsEditLine {
                text: String::new(),
                timestamp: String::new(),
            });
        }
        self.lyrics_editor = Some(LyricsEditorState {
            track_id: id,
            expected_text,
            metadata: document.metadata,
            lines,
            original_synced,
            allow_plain_conversion: false,
            dirty: false,
            discard_confirm: false,
            remove_confirm: false,
            error: None,
        });
        self.lyrics_mode = LyricsMode::Edit;
        self.lyrics_follow_current = false;
        self.lyrics_error = None;
    }

    fn save_lyrics_editor(&mut self) {
        let Some(editor) = self.lyrics_editor.as_ref().cloned() else {
            return;
        };
        let mut lines = Vec::with_capacity(editor.lines.len());
        let mut last_timestamp = None;
        for (index, edit_line) in editor.lines.iter().enumerate() {
            let timestamp = match parse_editor_timestamp(&edit_line.timestamp) {
                Ok(timestamp) => timestamp,
                Err(error) => {
                    if let Some(editor) = self.lyrics_editor.as_mut() {
                        editor.error = Some(format!("Line {}: {error}", index + 1));
                    }
                    return;
                }
            };
            if let Some(timestamp) = timestamp {
                if last_timestamp.is_some_and(|previous| timestamp < previous) {
                    if let Some(editor) = self.lyrics_editor.as_mut() {
                        editor.error = Some(format!(
                            "Line {} is earlier than the preceding timestamp. Reorder the line or adjust its timestamp.",
                            index + 1
                        ));
                    }
                    return;
                }
                last_timestamp = Some(timestamp);
            }
            lines.push(LyricLine {
                text: edit_line.text.clone(),
                timestamp_ms: timestamp,
            });
        }
        let synced = lines.iter().any(|line| line.timestamp_ms.is_some());
        if editor.original_synced && !synced && !editor.allow_plain_conversion {
            if let Some(editor) = self.lyrics_editor.as_mut() {
                editor.allow_plain_conversion = true;
                editor.error = Some(
                    "Every timestamp is gone. Save again to explicitly convert this synchronized document to plain text."
                        .to_owned(),
                );
            }
            return;
        }
        let document = LyricsDocument {
            metadata: editor.metadata.clone(),
            lines,
        };
        let serialized = if synced || !document.metadata.is_empty() {
            document.to_lrc()
        } else {
            document.to_plain_text()
        };
        match self.controller.save_lyrics_checked(
            editor.track_id,
            editor.expected_text.as_deref(),
            Some(&serialized),
        ) {
            Ok(()) => {
                self.invalidate_lyrics_cache(editor.track_id);
                self.lyrics_editor = None;
                self.lyrics_mode = LyricsMode::Read;
                self.lyrics_error = None;
                self.lifecycle_msg = Some(format!("Saved lyrics for track {}.", editor.track_id));
            }
            Err(error) => {
                if let Some(editor) = self.lyrics_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
    }

    fn cancel_lyrics_editor(&mut self) {
        let Some(editor) = self.lyrics_editor.as_mut() else {
            self.lyrics_mode = LyricsMode::Read;
            return;
        };
        if editor.dirty && !editor.discard_confirm {
            editor.discard_confirm = true;
            editor.error = Some(
                "Unsaved lyrics changes will be discarded. Press Cancel again to discard them."
                    .to_owned(),
            );
            return;
        }
        self.lyrics_editor = None;
        self.lyrics_mode = LyricsMode::Read;
    }

    fn remove_lyrics_from_editor(&mut self) {
        let Some(editor) = self.lyrics_editor.as_mut() else {
            return;
        };
        if !editor.remove_confirm {
            editor.remove_confirm = true;
            editor.error = Some(
                "Press “Confirm remove lyrics” again to delete only the managed lyrics file."
                    .to_owned(),
            );
            return;
        }
        let editor = editor.clone();
        match self.controller.save_lyrics_checked(
            editor.track_id,
            editor.expected_text.as_deref(),
            None,
        ) {
            Ok(()) => {
                self.invalidate_lyrics_cache(editor.track_id);
                self.lyrics_editor = None;
                self.lyrics_mode = LyricsMode::Read;
                self.lyrics_error = None;
                self.lifecycle_msg = Some(format!(
                    "Removed lyrics for track {}; the track and audio were not changed.",
                    editor.track_id
                ));
            }
            Err(error) => {
                if let Some(editor) = self.lyrics_editor.as_mut() {
                    editor.error = Some(error);
                }
            }
        }
    }

    fn begin_lyrics_import(&mut self) -> Task<Message> {
        let Some(id) = self.selected_lyrics else {
            self.lyrics_error = Some("Select a track before importing lyrics.".to_owned());
            return Task::none();
        };
        let start = self
            .controller
            .track_lyrics_path(id)
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| self.controller.canonical_root().map(Path::to_path_buf));
        let session = self.controller.library_session();
        Task::perform(
            async move { flux_platform::pick_lyrics_files_in(start.as_deref()) },
            move |result| Message::LyricsImportPicked(session, result),
        )
    }

    fn set_lyrics_import_preview(
        &mut self,
        path: PathBuf,
        raw: String,
        expected_text: Option<String>,
    ) {
        let (document, error) = match LyricsDocument::parse(&raw) {
            Ok(document) => (document, None),
            Err(error) => (LyricsDocument::default(), Some(error.to_string())),
        };
        let Some(id) = self.selected_lyrics else {
            self.lyrics_error = Some("Select a track before importing lyrics.".to_owned());
            return;
        };
        self.lyrics_import = Some(LyricsImportPreview {
            track_id: id,
            path,
            raw,
            document,
            expected_text,
            error,
            overwrite_confirm: false,
        });
    }

    fn confirm_lyrics_import(&mut self) {
        let Some(mut preview) = self.lyrics_import.take() else {
            return;
        };
        if preview.error.is_some() {
            self.lyrics_import = Some(preview);
            return;
        }
        if preview.expected_text.is_some() && !preview.overwrite_confirm {
            preview.overwrite_confirm = true;
            self.lyrics_import = Some(preview);
            return;
        }
        match self.controller.save_lyrics_checked(
            preview.track_id,
            preview.expected_text.as_deref(),
            Some(&preview.raw),
        ) {
            Ok(()) => {
                self.invalidate_lyrics_cache(preview.track_id);
                self.selected_lyrics = Some(preview.track_id);
                self.lyrics_mode = LyricsMode::Read;
                self.lyrics_import = None;
                self.lyrics_error = None;
                self.lifecycle_msg = Some(format!(
                    "Imported lyrics for track {} from {}.",
                    preview.track_id,
                    preview.path.display()
                ));
            }
            Err(error) => {
                preview.error = Some(error);
                self.lyrics_import = Some(preview);
            }
        }
    }

    fn export_lyrics(&mut self, format: &str) {
        let Some(id) = self.selected_lyrics else {
            self.lyrics_error = Some("Select a track before exporting lyrics.".to_owned());
            return;
        };
        let Some(raw) = self.controller.track_lyrics(id) else {
            self.lyrics_error = Some("This track has no managed lyrics to export.".to_owned());
            return;
        };
        let document = match LyricsDocument::parse(&raw) {
            Ok(document) => document,
            Err(error) => {
                self.lyrics_error = Some(format!("Lyrics cannot be exported: {error}"));
                return;
            }
        };
        let expected_extension = if format == "lrc" { "lrc" } else { "txt" };
        let mut path = PathBuf::from(self.lyrics_export_path.trim());
        if self.lyrics_export_path.trim().is_empty() {
            self.lyrics_error = Some("Enter an export destination path.".to_owned());
            return;
        }
        match path.extension().and_then(|extension| extension.to_str()) {
            None => {
                path.set_extension(expected_extension);
            }
            Some(extension) if extension.eq_ignore_ascii_case(expected_extension) => {}
            Some(_) => {
                self.lyrics_error = Some(format!(
                    "Export .{format} requires a .{expected_extension} destination."
                ));
                return;
            }
        }
        let pending =
            self.lyrics_export_pending
                .as_ref()
                .is_some_and(|(pending_path, pending_format)| {
                    pending_path == &path && pending_format == format
                });
        if path.exists() && !pending {
            self.lyrics_export_pending = Some((path.clone(), format.to_owned()));
            self.lyrics_error = Some(
                "The destination exists. Click the same export action again to overwrite it."
                    .to_owned(),
            );
            return;
        }
        let content = if format == "lrc" {
            document.to_lrc()
        } else {
            document.to_plain_text()
        };
        match std::fs::write(&path, content.as_bytes()) {
            Ok(()) => {
                self.lyrics_export_pending = None;
                self.lyrics_error = None;
                self.lifecycle_msg = Some(format!("Exported lyrics to {}.", path.display()));
            }
            Err(error) => self.lyrics_error = Some(format!("Could not export lyrics: {error}")),
        }
    }

    fn lyrics_cache_entry(&self, id: TrackId) -> Option<LyricsCacheEntry> {
        let path = self.controller.track_lyrics_path(id)?;
        let metadata = std::fs::metadata(&path).ok();
        let modified_ns = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        let len = metadata.map_or(0, |metadata| metadata.len());
        {
            let cache = self.lyrics_cache.borrow();
            if let Some(entry) = cache.get(&id)
                && entry.path == path
                && entry.modified_ns == modified_ns
                && entry.len == len
            {
                return Some(entry.clone());
            }
        }
        let (document, error) = match std::fs::read_to_string(&path) {
            Ok(raw) => match LyricsDocument::parse(&raw) {
                Ok(document) => (Some(document), None),
                Err(error) => (None, Some(error.to_string())),
            },
            Err(error) => (None, Some(format!("lyrics file cannot be read: {error}"))),
        };
        let entry = LyricsCacheEntry {
            path,
            modified_ns,
            len,
            document,
            error,
        };
        self.lyrics_cache.borrow_mut().insert(id, entry.clone());
        Some(entry)
    }

    fn invalidate_lyrics_cache(&self, id: TrackId) {
        self.lyrics_cache.borrow_mut().remove(&id);
    }

    fn lyrics_kind_for(&self, id: TrackId) -> LyricsKind {
        self.lyrics_cache_entry(id)
            .and_then(|entry| entry.document)
            .map_or(LyricsKind::Plain, |document| document.kind())
    }

    fn filtered_lyrics_tracks(&self) -> Vec<(TrackId, &Track)> {
        let needle = self.lyrics_filter.trim().to_lowercase();
        self.controller
            .tracks()
            .iter()
            .filter(|(id, track)| {
                self.controller.track_has_lyrics(*id)
                    && (needle.is_empty()
                        || track.title().to_lowercase().contains(&needle)
                        || track
                            .artist()
                            .is_some_and(|artist| artist.to_lowercase().contains(&needle))
                        || track
                            .album()
                            .is_some_and(|album| album.to_lowercase().contains(&needle)))
            })
            .map(|(id, track)| (*id, track))
            .collect()
    }

    fn lyrics_library_view(&self) -> Element<'_, Message> {
        let rows = self.filtered_lyrics_tracks();
        let total = rows.len();
        let mut body = column![
            row![
                column![
                    text("Lyrics").size(22).color(TEXT),
                    text(format!("{total} track(s) with managed lyrics"))
                        .size(10)
                        .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Open current lyrics").size(10))
                    .style(accent_button_style)
                    .padding([7, 10])
                    .on_press_maybe(
                        self.controller
                            .current_id()
                            .map(|_| Message::LyricsOpenCurrent)
                    ),
            ]
            .align_y(Alignment::Center),
            text_input(
                "Filter lyrics by title, artist or album…",
                &self.lyrics_filter
            )
            .on_input(Message::LyricsFilterChanged)
            .style(input_style)
            .width(Length::Fill),
        ]
        .spacing(9);
        if let Some(error) = &self.lyrics_error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        if rows.is_empty() {
            body = body.push(
                container(
                    column![
                        text(if self.lyrics_filter.trim().is_empty() {
                            "No tracks have lyrics yet."
                        } else {
                            "No lyrics tracks match this filter."
                        })
                        .size(16)
                        .color(TEXT),
                        text("Open a track and use Edit or Import lyrics to create managed plain-text or synchronized lyrics.")
                            .size(10)
                            .color(MUTED),
                        button(text("Browse tracks").size(11))
                            .style(accent_button_style)
                            .padding([7, 11])
                            .on_press(Message::SelectView(LibraryView::Tracks)),
                    ]
                    .spacing(8)
                    .align_x(Alignment::Center),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .center(Length::Fill)
                .style(card_style),
            );
            return body.into();
        }
        let pages = total.div_ceil(TRACK_PAGE_SIZE).max(1);
        let page = self.page.min(pages - 1);
        let start = page * TRACK_PAGE_SIZE;
        let end = (start + TRACK_PAGE_SIZE).min(total);
        let current = self.controller.current_id();
        let mut list = column![].spacing(4);
        for (id, track) in &rows[start..end] {
            let entry = self.lyrics_cache_entry(*id);
            let kind = entry
                .as_ref()
                .and_then(|entry| entry.document.as_ref())
                .map_or(LyricsKind::Plain, LyricsDocument::kind);
            let invalid = entry.as_ref().and_then(|entry| entry.error.as_ref());
            let preview = entry
                .as_ref()
                .and_then(|entry| entry.document.as_ref())
                .and_then(|document| {
                    document
                        .lines
                        .iter()
                        .map(|line| line.text.trim())
                        .find(|line| !line.is_empty())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "No readable text preview".to_owned());
            list = list.push(
                button(
                    row![
                        container(text("♪").size(16).color(ACCENT))
                            .width(38)
                            .height(38)
                            .center(38)
                            .style(artwork_style(artwork_color(track.title()))),
                        column![
                            row![
                                text(track.title().to_owned())
                                    .size(13)
                                    .color(if current == Some(*id) { ACCENT } else { TEXT }),
                                if current == Some(*id) {
                                    Element::from(text("▶ playing").size(9).color(ACCENT))
                                } else {
                                    Element::from(iced::widget::Space::new().width(0))
                                },
                            ]
                            .spacing(7),
                            text(format!(
                                "{} · {} · {}",
                                display_artist(track),
                                display_album(track),
                                match kind {
                                    LyricsKind::Synced => "Synchronized",
                                    LyricsKind::Plain => "Plain text",
                                }
                            ))
                            .size(9)
                            .color(MUTED),
                            text(preview).size(9).color(MUTED),
                        ]
                        .spacing(1)
                        .width(Length::Fill),
                        text(if invalid.is_some() {
                            "INVALID"
                        } else {
                            match kind {
                                LyricsKind::Synced => "SYNC",
                                LyricsKind::Plain => "TEXT",
                            }
                        })
                        .size(8)
                        .color(if invalid.is_some() {
                            DANGER
                        } else if kind == LyricsKind::Synced {
                            ACCENT
                        } else {
                            MUTED
                        }),
                        text("›").size(18).color(MUTED),
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding(7)
                .style(group_style)
                .on_press(Message::LyricsSelect(*id)),
            );
        }
        body = body.push(scrollable(list).height(Length::Fill));
        body = body.push(pagination_footer(page, pages, start, end, total));
        body.height(Length::Fill).into()
    }

    fn lyrics_detail_view_for_mode(&self, id: TrackId) -> Element<'_, Message> {
        match self.lyrics_mode {
            LyricsMode::Read => self.lyrics_read_view(id),
            LyricsMode::Edit => self.lyrics_editor_view(),
            LyricsMode::Karaoke => self.lyrics_karaoke_view(id),
        }
    }

    fn lyrics_read_view(&self, id: TrackId) -> Element<'_, Message> {
        let Some(track) = self.controller.track(id) else {
            return text("The lyrics track is no longer in the library.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let entry = self.lyrics_cache_entry(id);
        let document = entry.as_ref().and_then(|entry| entry.document.clone());
        let status = document
            .as_ref()
            .map_or(LyricsKind::Plain, LyricsDocument::kind);
        let current_track = self.controller.current_id() == Some(id);
        let position_ms = if current_track {
            self.seek_preview
                .map(|seconds| (f64::from(seconds) * 1000.0) as u64)
                .unwrap_or_else(|| self.controller.position().as_millis() as u64)
        } else {
            0
        };
        let album = self.controller.track_album_id(id);
        let owner = self.controller.track_artist_id(id);
        let mut header = row![
            button(text("‹ Back").size(11))
                .style(ghost_style)
                .padding([6, 9])
                .on_press(Message::LyricsBack),
            column![
                text(track.title().to_owned()).size(22).color(TEXT),
                text(format!(
                    "{} · {}",
                    display_artist(track),
                    display_album(track)
                ))
                .size(10)
                .color(MUTED),
                text(match status {
                    LyricsKind::Synced => "Synchronized lyrics",
                    LyricsKind::Plain => "Plain-text lyrics",
                })
                .size(9)
                .color(if status == LyricsKind::Synced {
                    ACCENT
                } else {
                    MUTED
                }),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text("Play").size(10))
                .style(accent_button_style)
                .padding([7, 10])
                .on_press(Message::PlayTrack(id)),
            button(text("Edit").size(10))
                .style(ghost_style)
                .padding([7, 10])
                .on_press(Message::LyricsEditOpen),
            button(text("Import…").size(10))
                .style(ghost_style)
                .padding([7, 10])
                .on_press(Message::LyricsImportOpen),
            button(text("Karaoke").size(10))
                .style(ghost_style)
                .padding([7, 10])
                .on_press(Message::LyricsModeChanged(LyricsMode::Karaoke)),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        if let Some(album_id) = album {
            header = header.push(
                button(text("Album").size(9))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::LyricsOpenAlbum(album_id)),
            );
        }
        if let Some(artist_id) = owner {
            header = header.push(
                button(text("Artist").size(9))
                    .style(ghost_style)
                    .padding([5, 8])
                    .on_press(Message::LyricsOpenArtist(artist_id)),
            );
        }
        header = header.push(
            button(text("Track").size(9))
                .style(ghost_style)
                .padding([5, 8])
                .on_press(Message::SearchNavigateTrack(id)),
        );
        let mut body = column![header].spacing(10);
        if let Some(error) = &self.lyrics_error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        if let Some(error) = entry.as_ref().and_then(|entry| entry.error.as_ref()) {
            body = body.push(
                text(format!("Lyrics cannot be parsed: {error}"))
                    .size(10)
                    .color(DANGER),
            );
        }
        if let Some(document) = &document {
            if !document.metadata.is_empty() {
                body = body.push(
                    text(format!(
                        "{} LRC metadata tag(s) preserved; they never override canonical track metadata.",
                        document.metadata.len()
                    ))
                    .size(9)
                    .color(MUTED),
                );
            }
            if status == LyricsKind::Synced {
                let events = document.sync_events();
                let current_event = current_track
                    .then(|| document.current_event_index(position_ms))
                    .flatten();
                let mut lines = column![].spacing(3);
                for (index, event) in events.iter().enumerate() {
                    let color = if current_event == Some(index) {
                        ACCENT
                    } else if current_event.is_some_and(|current| index < current) {
                        MUTED
                    } else {
                        TEXT
                    };
                    lines = lines.push(
                        row![
                            text(format_timestamp_ms(event.time_ms))
                                .size(9)
                                .color(MUTED)
                                .width(Length::Fixed(64.0)),
                            text(event.text.clone())
                                .size(if current_event == Some(index) { 15 } else { 12 })
                                .color(color),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center),
                    );
                }
                let untimed = document
                    .lines
                    .iter()
                    .filter(|line| line.timestamp_ms.is_none())
                    .collect::<Vec<_>>();
                if !untimed.is_empty() {
                    lines = lines.push(text("UNTIMED").size(8).color(MUTED));
                    for line in untimed {
                        lines = lines.push(text(line.text.clone()).size(11).color(TEXT));
                    }
                }
                body = body.push(scrollable(lines).height(Length::Fill));
            } else {
                let lines = document
                    .lines
                    .iter()
                    .map(|line| Element::from(text(line.text.clone()).size(12).color(TEXT)))
                    .collect::<Vec<_>>();
                body = body.push(scrollable(column![].extend(lines)).height(Length::Fill));
            }
            body = body.push(
                text("Playback highlighting uses the actual player position; seek, pause, resume and track changes update it without changing the canonical text.")
                    .size(9)
                    .color(MUTED),
            );
        } else {
            body = body.push(
                container(
                    column![
                        text("No managed lyrics for this track.").size(16).color(TEXT),
                        text("Create plain or synchronized lyrics manually, or import an LRC/TXT file. Existing lyrics are preserved until an explicit save.")
                            .size(10)
                            .color(MUTED),
                        row![
                            button(text("Create lyrics").size(11))
                                .style(accent_button_style)
                                .padding([7, 10])
                                .on_press(Message::LyricsEditOpen),
                            button(text("Import lyrics…").size(10))
                                .style(ghost_style)
                                .padding([7, 10])
                                .on_press(Message::LyricsImportOpen),
                        ]
                        .spacing(7),
                    ]
                    .spacing(7),
                )
                .padding(12)
                .width(Length::Fill)
                .style(card_style),
            );
        }
        body = body.push(
            container(
                column![
                    text("EXPORT").size(9).color(ACCENT),
                    row![
                        text_input("Destination path (.lrc or .txt)…", &self.lyrics_export_path)
                            .on_input(Message::LyricsExportPathChanged)
                            .style(input_style)
                            .width(Length::Fill),
                        button(text(if self.lyrics_export_pending.as_ref().is_some_and(|(_, format)| format == "lrc") { "Confirm overwrite .lrc" } else { "Export .lrc" }).size(10))
                            .style(ghost_style)
                            .padding([6, 8])
                            .on_press_maybe(document.is_some().then_some(Message::LyricsExport("lrc".to_owned()))),
                        button(text(if self.lyrics_export_pending.as_ref().is_some_and(|(_, format)| format == "txt") { "Confirm overwrite .txt" } else { "Export .txt" }).size(10))
                            .style(ghost_style)
                            .padding([6, 8])
                            .on_press_maybe(document.is_some().then_some(Message::LyricsExport("txt".to_owned()))),
                    ]
                    .spacing(6),
                    text("Export is UTF-8 and never writes into audio files. Existing destination files require a second explicit confirmation.")
                        .size(9)
                        .color(MUTED),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
        );
        body.height(Length::Fill).into()
    }

    fn lyrics_editor_view(&self) -> Element<'_, Message> {
        let Some(editor) = self.lyrics_editor.as_ref() else {
            return text("No lyrics editor is open.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let track_title = self
            .controller
            .track(editor.track_id)
            .map(|track| track.title().to_owned())
            .unwrap_or_else(|| format!("Track {}", editor.track_id));
        let current_playback = self.controller.current_id() == Some(editor.track_id);
        let mut body = column![
            row![
                button(text("‹ Cancel").size(10))
                    .style(ghost_style)
                    .padding([6, 8])
                    .on_press(Message::LyricsEditorCancel),
                column![
                    text(format!("Edit lyrics · {track_title}")).size(20).color(TEXT),
                    text(if editor.dirty { "Unsaved changes" } else { "No unsaved changes" })
                        .size(9)
                        .color(if editor.dirty { ACCENT } else { MUTED }),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Save lyrics").size(11))
                    .style(accent_button_style)
                    .padding([7, 11])
                    .on_press(Message::LyricsEditorSave),
                button(
                    text(if editor.remove_confirm {
                        "Confirm remove lyrics"
                    } else {
                        "Remove lyrics"
                    })
                    .size(10),
                )
                .style(danger_button_style)
                .padding([7, 10])
                .on_press(Message::LyricsEditorRemove),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
            text("Edit text and timestamps. Leave a timestamp empty for an untimed line. Timestamps use mm:ss.xxx and must be non-decreasing in line order.")
                .size(9)
                .color(MUTED),
        ]
        .spacing(8);
        if let Some(error) = &editor.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        let mut rows = column![].spacing(3);
        for (index, line) in editor.lines.iter().enumerate() {
            let mut actions = row![
                text(format!("{}", index + 1))
                    .size(9)
                    .color(MUTED)
                    .width(Length::Fixed(24.0)),
                text_input("Lyric line…", &line.text)
                    .on_input(move |value| Message::LyricsEditorTextChanged(index, value))
                    .style(input_style)
                    .width(Length::FillPortion(5)),
                text_input("mm:ss.xxx", &line.timestamp)
                    .on_input(move |value| Message::LyricsEditorTimestampChanged(index, value))
                    .style(input_style)
                    .width(Length::Fixed(112.0)),
            ]
            .spacing(5)
            .align_y(Alignment::Center);
            actions = actions.push(
                button(text("Now").size(9))
                    .style(ghost_style)
                    .padding([4, 6])
                    .on_press_maybe(
                        current_playback.then_some(Message::LyricsEditorSetCurrentTime(index)),
                    ),
            );
            for (label, delta) in [
                ("-1s", -1000i64),
                ("-100", -100),
                ("+100", 100),
                ("+1s", 1000),
            ] {
                actions = actions.push(
                    button(text(label).size(8))
                        .style(ghost_style)
                        .padding([4, 5])
                        .on_press(Message::LyricsEditorNudge(index, delta)),
                );
            }
            actions = actions.push(
                button(text("Clear").size(8))
                    .style(ghost_style)
                    .padding([4, 5])
                    .on_press(Message::LyricsEditorClearTimestamp(index)),
            );
            actions = actions.push(
                button(text("↑").size(9))
                    .style(ghost_style)
                    .padding([4, 5])
                    .on_press_maybe(
                        (index > 0).then_some(Message::LyricsEditorMoveLine(index, -1)),
                    ),
            );
            actions = actions.push(
                button(text("↓").size(9))
                    .style(ghost_style)
                    .padding([4, 5])
                    .on_press_maybe(
                        (index + 1 < editor.lines.len())
                            .then_some(Message::LyricsEditorMoveLine(index, 1)),
                    ),
            );
            actions = actions.push(
                button(text("×").size(9))
                    .style(danger_button_style)
                    .padding([4, 5])
                    .on_press(Message::LyricsEditorRemoveLine(index)),
            );
            rows = rows.push(actions);
        }
        body = body.push(scrollable(rows).height(Length::Fill));
        body = body.push(
            row![
                button(text("Add line").size(10))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::LyricsEditorAddLine),
                text(if editor.original_synced {
                    "Removing every timestamp requires an explicit second Save to convert to plain text."
                } else {
                    "Any saved timestamp converts this document to synchronized LRC."
                })
                .size(9)
                .color(MUTED),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        body.height(Length::Fill).into()
    }

    fn lyrics_karaoke_view(&self, id: TrackId) -> Element<'_, Message> {
        let Some(track) = self.controller.track(id) else {
            return text("The karaoke track is no longer in the library.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let entry = self.lyrics_cache_entry(id);
        let document = entry.as_ref().and_then(|entry| entry.document.clone());
        let current_track = self.controller.current_id() == Some(id);
        let position_ms = if current_track {
            self.seek_preview
                .map(|seconds| (f64::from(seconds) * 1000.0) as u64)
                .unwrap_or_else(|| self.controller.position().as_millis() as u64)
        } else {
            0
        };
        let mut body = column![
            row![
                button(text("‹ Back").size(11))
                    .style(ghost_style)
                    .padding([6, 9])
                    .on_press(Message::LyricsModeChanged(LyricsMode::Read)),
                column![
                    text(track.title().to_owned()).size(20).color(TEXT),
                    text(format!(
                        "{} · {}",
                        display_artist(track),
                        display_album(track)
                    ))
                    .size(10)
                    .color(MUTED),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Play").size(10))
                    .style(accent_button_style)
                    .padding([7, 10])
                    .on_press(Message::PlayTrack(id)),
                button(text("Edit").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::LyricsEditOpen),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if let Some(document) = &document {
            let events = document.sync_events();
            if events.is_empty() {
                body = body.push(
                    text("This is plain-text lyrics; karaoke needs synchronized timestamps.")
                        .size(12)
                        .color(MUTED),
                );
                let lines = document
                    .lines
                    .iter()
                    .map(|line| Element::from(text(line.text.clone()).size(13).color(TEXT)))
                    .collect::<Vec<_>>();
                body = body.push(scrollable(column![].extend(lines)).height(Length::Fill));
            } else {
                let current_event = current_track
                    .then(|| document.current_event_index(position_ms))
                    .flatten();
                let current_line = current_event
                    .and_then(|index| events.get(index))
                    .map(|event| event.text.clone())
                    .unwrap_or_else(|| {
                        if current_track {
                            "Waiting for the first line…".to_owned()
                        } else {
                            "Play this track to follow karaoke.".to_owned()
                        }
                    });
                body = body.push(
                    container(
                        text(current_line)
                            .size(30)
                            .color(ACCENT)
                            .width(Length::Fill),
                    )
                    .padding([18, 12])
                    .width(Length::Fill)
                    .style(card_style),
                );
                if let Some(current) = current_event {
                    if let Some(previous) =
                        current.checked_sub(1).and_then(|index| events.get(index))
                    {
                        body = body.push(text(previous.text.clone()).size(12).color(MUTED));
                    }
                    let upcoming = events
                        .iter()
                        .skip(current + 1)
                        .take(4)
                        .map(|event| Element::from(text(event.text.clone()).size(14).color(TEXT)))
                        .collect::<Vec<_>>();
                    body = body.push(column![].extend(upcoming).spacing(5));
                } else {
                    let upcoming = events
                        .iter()
                        .take(4)
                        .map(|event| Element::from(text(event.text.clone()).size(14).color(TEXT)))
                        .collect::<Vec<_>>();
                    body = body.push(column![].extend(upcoming).spacing(5));
                }
                body = body.push(
                    text("Karaoke follows the actual playback clock. Pausing, seeking and track changes do not alter the canonical lyrics.")
                        .size(9)
                        .color(MUTED),
                );
            }
        } else {
            body = body.push(
                text("No managed lyrics for this track.")
                    .size(13)
                    .color(MUTED),
            );
        }
        body.height(Length::Fill).into()
    }

    fn lyrics_import_overlay(&self) -> Element<'_, Message> {
        let Some(preview) = self.lyrics_import.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let mut body = column![
            text("Preview imported lyrics").size(22).color(TEXT),
            text(format!(
                "Track {} · {}",
                preview.track_id,
                preview.path.display()
            ))
            .size(10)
            .color(MUTED),
            text(format!(
                "Detected: {} · {} line(s)",
                match preview.document.kind() {
                    LyricsKind::Synced => "synchronized LRC",
                    LyricsKind::Plain => "plain text",
                },
                preview.document.lines.len()
            ))
            .size(10)
            .color(ACCENT),
        ]
        .spacing(8);
        if preview.expected_text.is_some() {
            body = body.push(
                text("This track already has lyrics. Saving requires a second explicit confirmation and will not happen automatically.")
                    .size(9)
                    .color(Color::from_rgb8(0xff, 0xc2, 0x66)),
            );
        }
        if let Some(error) = &preview.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        let preview_lines = preview
            .document
            .lines
            .iter()
            .take(12)
            .map(|line| {
                let timestamp = line
                    .timestamp_ms
                    .map(format_timestamp_ms)
                    .unwrap_or_else(|| "--:--.---".to_owned());
                Element::from(
                    text(format!("{timestamp}  {}", line.text))
                        .size(10)
                        .color(MUTED),
                )
            })
            .collect::<Vec<_>>();
        body = body.push(scrollable(column![].extend(preview_lines)).height(Length::Fixed(260.0)));
        body = body.push(
            row![
                button(
                    text(
                        if preview.expected_text.is_some() && !preview.overwrite_confirm {
                            "Confirm overwrite existing lyrics"
                        } else {
                            "Save imported lyrics"
                        }
                    )
                    .size(11),
                )
                .style(accent_button_style)
                .padding([7, 12])
                .on_press_maybe(
                    preview
                        .error
                        .is_none()
                        .then_some(Message::LyricsImportConfirm)
                ),
                button(text("Cancel").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::LyricsImportCancel),
            ]
            .spacing(8),
        );
        container(
            container(body)
                .padding(18)
                .width(Length::Fill)
                .max_width(720.0)
                .max_height(560.0)
                .style(import_panel_style),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .style(modal_backdrop_style)
        .into()
    }

    fn most_played_breakdown(&self) -> Element<'_, Message> {
        let (artists, albums) = history_entity_breakdown(&self.controller);
        let artist_text = artists
            .iter()
            .take(5)
            .map(|(name, count)| format!("{name} ({count})"))
            .collect::<Vec<_>>()
            .join(" · ");
        let album_text = albums
            .iter()
            .take(5)
            .map(|(name, count)| format!("{name} ({count})"))
            .collect::<Vec<_>>()
            .join(" · ");
        column![
            text(if artist_text.is_empty() {
                "Top artists: no meaningful plays recorded yet."
            } else {
                "Top artists"
            })
            .size(9)
            .color(ACCENT),
            text(artist_text).size(9).color(MUTED),
            text(if album_text.is_empty() {
                "Top albums: no meaningful plays recorded yet."
            } else {
                "Top albums"
            })
            .size(9)
            .color(ACCENT),
            text(album_text).size(9).color(MUTED),
        ]
        .spacing(3)
        .into()
    }

    fn dashboard_summary(&self) -> DashboardSummary {
        let generation = self.controller.generation();
        let history_generation = self.controller.history_generation();
        if let Some(cached) = self.dashboard_cache.borrow().as_ref()
            && cached.generation == generation
            && cached.history_generation == history_generation
        {
            return cached.clone();
        }
        let tracks = self.controller.tracks();
        let mut favorites = 0;
        let mut rated = 0;
        let mut with_lyrics = 0;
        let mut synced_lyrics = 0;
        let mut with_artwork = 0;
        for (id, track) in tracks {
            if let Some(rating) = self.controller.rating(*id) {
                if rating.favorite {
                    favorites += 1;
                }
                if rating.rating.is_some() {
                    rated += 1;
                }
            }
            if self.controller.track_has_lyrics(*id) {
                with_lyrics += 1;
                if self.lyrics_kind_for(*id) == LyricsKind::Synced {
                    synced_lyrics += 1;
                }
            }
            if self
                .controller
                .track_album_id(*id)
                .and_then(|album| self.controller.album(album))
                .is_some_and(|album| album.cover.is_some())
            {
                with_artwork += 1;
            }
            let _ = track;
        }
        let summary = DashboardSummary {
            generation,
            history_generation,
            total_tracks: tracks.len(),
            total_artists: self.controller.artists().len(),
            total_albums: self.controller.albums().len(),
            favorites,
            rated,
            with_lyrics,
            synced_lyrics,
            with_artwork,
            recently_added: self
                .smart_collection_ids(SmartCollection::RecentlyAdded)
                .into_iter()
                .take(6)
                .collect(),
            recently_played: self
                .smart_collection_ids(SmartCollection::RecentlyPlayed)
                .into_iter()
                .take(6)
                .collect(),
            most_played: self
                .smart_collection_ids(SmartCollection::MostPlayed)
                .into_iter()
                .take(6)
                .collect(),
            highly_rated: self
                .smart_collection_ids(SmartCollection::HighlyRated)
                .into_iter()
                .take(6)
                .collect(),
        };
        *self.dashboard_cache.borrow_mut() = Some(summary.clone());
        summary
    }

    fn dashboard_track_list(
        &self,
        title: &'static str,
        ids: &[TrackId],
        collection: SmartCollection,
    ) -> Element<'_, Message> {
        let mut rows = column![
            row![
                text(title).size(11).color(ACCENT),
                iced::widget::Space::new().width(Length::Fill),
                button(text("View all").size(9))
                    .style(ghost_style)
                    .padding([4, 7])
                    .on_press(Message::CollectionSelect(collection)),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(3);
        if ids.is_empty() {
            rows = rows.push(text("No tracked entries yet.").size(9).color(MUTED));
        } else {
            for id in ids {
                let Some(track) = self.controller.track(*id) else {
                    continue;
                };
                rows = rows.push(
                    button(
                        row![
                                text(track.title().to_owned())
                                    .size(11)
                                    .color(TEXT)
                                    .width(Length::Fill),
                                text(format!(
                                    "{} · {}",
                                    display_artist(track),
                                    match collection {
                                        SmartCollection::MostPlayed => format!(
                                            "{} play(s)",
                                            self.controller.track_play_count(*id)
                                        ),
                                        SmartCollection::RecentlyPlayed => self
                                            .controller
                                            .track_last_played_unix_ms(*id)
                                            .map_or_else(
                                                || "played".to_owned(),
                                                |ms| format!("last {ms}")
                                            ),
                                        _ => display_album(track),
                                    }
                                ))
                                .size(9)
                                .color(MUTED),
                            ]
                        .spacing(8),
                    )
                    .width(Length::Fill)
                    .padding([4, 6])
                    .style(group_style)
                    .on_press(Message::PlayTrack(*id)),
                );
            }
        }
        rows.into()
    }

    fn dashboard_view(&self) -> Element<'_, Message> {
        let summary = self.dashboard_summary();
        if summary.total_tracks == 0 {
            return container(
                column![
                    text("Your library is empty").size(20).color(TEXT),
                    text("Import music to populate the dashboard and smart collections.")
                        .size(11)
                        .color(MUTED),
                    button(text("Import music").size(11))
                        .style(accent_button_style)
                        .padding([7, 11])
                        .on_press(Message::ImportOpen),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center(Length::Fill)
            .into();
        }
        let stat = |label: &'static str, value: String, hint: &str| -> Element<'_, Message> {
            container(
                column![
                    text(value).size(22).color(ACCENT),
                    text(label).size(10).color(TEXT),
                    text(hint.to_owned()).size(8).color(MUTED),
                ]
                .spacing(2),
            )
            .padding(10)
            .width(Length::FillPortion(1))
            .style(card_style)
            .into()
        };
        let mut body = column![
            text("Library dashboard").size(22).color(TEXT),
            text("Canonical library totals and locally collected listening history. History begins when this feature is first used; no past plays are fabricated.")
                .size(9)
                .color(MUTED),
            row![
                stat("Tracks", summary.total_tracks.to_string(), "catalogue entries"),
                stat("Artists", summary.total_artists.to_string(), "stable identities"),
                stat("Albums", summary.total_albums.to_string(), "releases"),
            ]
            .spacing(7),
            row![
                stat("Favorites", summary.favorites.to_string(), "marked favorite"),
                stat("Rated", summary.rated.to_string(), "have a rating"),
                stat("With lyrics", summary.with_lyrics.to_string(), &format!("{} synchronized", summary.synced_lyrics)),
            ]
            .spacing(7),
            row![
                stat("With artwork", summary.with_artwork.to_string(), "album cover reference"),
                stat("Plays recorded", self.controller.playback_history().total_plays().to_string(), "meaningful plays"),
                stat("History tracks", self.controller.playback_history().len().to_string(), "distinct TrackIds"),
            ]
            .spacing(7),
            row![
                container(
                    self.dashboard_track_list("RECENTLY ADDED", &summary.recently_added, SmartCollection::RecentlyAdded)
                ).padding(10).width(Length::FillPortion(1)).style(card_style),
                container(
                    self.dashboard_track_list("RECENTLY PLAYED", &summary.recently_played, SmartCollection::RecentlyPlayed)
                ).padding(10).width(Length::FillPortion(1)).style(card_style),
            ].spacing(7),
            row![
                container(
                    self.dashboard_track_list("MOST PLAYED", &summary.most_played, SmartCollection::MostPlayed)
                ).padding(10).width(Length::FillPortion(1)).style(card_style),
                container(
                    self.dashboard_track_list("HIGHLY RATED", &summary.highly_rated, SmartCollection::HighlyRated)
                ).padding(10).width(Length::FillPortion(1)).style(card_style),
            ].spacing(7),
            row![
                button(text("Browse smart collections").size(11))
                    .style(accent_button_style)
                    .padding([7, 10])
                    .on_press(Message::SelectView(LibraryView::Collections)),
                button(text("Recently played").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::SelectView(LibraryView::RecentlyPlayed)),
                button(text("Most played").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::SelectView(LibraryView::MostPlayed)),
            ]
            .spacing(7),
        ]
        .spacing(10);
        if self.controller.playback_history().is_empty() {
            body = body.push(
                text("Play a track for at least 5 seconds or half its duration (up to 30 seconds) to record a meaningful play. Seeking, pausing and restart notifications alone do not count.")
                    .size(9)
                    .color(MUTED),
            );
        }
        scrollable(body).height(Length::Fill).into()
    }

    fn collections_view(&self) -> Element<'_, Message> {
        let mut body = column![
            text("Smart Collections").size(22).color(TEXT),
            text("Dynamic canonical queries. Membership updates automatically; these are not editable playlists.")
                .size(10)
                .color(MUTED),
        ]
        .spacing(9);
        let mut list = column![].spacing(6);
        for collection in SmartCollection::ALL {
            let ids = self.smart_collection_ids(collection);
            let preview = ids
                .iter()
                .take(3)
                .filter_map(|id| self.controller.track(*id).map(Track::title))
                .collect::<Vec<_>>()
                .join(" · ");
            let subtitle = if preview.is_empty() {
                collection.description().to_owned()
            } else {
                format!("{} track(s) · {preview}", ids.len())
            };
            list = list.push(
                button(
                    row![
                        fallback_artwork(Some(collection.label()), 38.0),
                        column![
                            text(collection.label()).size(14).color(TEXT),
                            text(subtitle).size(9).color(MUTED),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        text(format!("{}", ids.len())).size(12).color(ACCENT),
                        text("›").size(18).color(MUTED),
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding(8)
                .style(group_style)
                .on_press(Message::CollectionSelect(collection)),
            );
        }
        body = body.push(scrollable(list).height(Length::Fill));
        body.height(Length::Fill).into()
    }

    fn artist_detail_view(&self, artist_id: ArtistId) -> Element<'_, Message> {
        let Some(artist) = self.controller.artist(artist_id) else {
            return text("Artist is no longer in the library.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let mut album_ids = self.controller.albums_owned_by(artist_id);
        album_ids.extend(self.controller.albums_credited_for(artist_id));
        album_ids.sort_by_key(|id| id.get());
        album_ids.dedup();
        let tracks = self
            .controller
            .tracks()
            .iter()
            .filter(|(id, _)| {
                self.controller.track_artist_id(*id) == Some(artist_id)
                    || self.controller.track_credits(*id).contains(&artist_id)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();

        let mut body = column![
            row![
                self.artist_artwork(artist_id, 112.0),
                column![
                    text(artist.name().to_owned()).size(22).color(TEXT),
                    text(format!(
                        "ArtistId {} · {} album(s) · {} track(s) · image {}",
                        artist.id(),
                        album_ids.len(),
                        tracks.len(),
                        if self.controller.artist_image_path(artist_id).is_some() {
                            "managed"
                        } else {
                            "none"
                        }
                    ))
                    .size(10)
                    .color(MUTED),
                    text(
                        self.controller
                            .artist_description(artist_id)
                            .filter(|description| !description.is_empty())
                            .unwrap_or("No description yet.")
                    )
                    .size(10)
                    .color(MUTED),
                ]
                .spacing(3)
                .width(Length::Fill),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            container(
                column![
                    text("ARTIST METADATA").size(9).color(ACCENT),
                    text_input("Artist display name…", &self.artist_detail_name_input)
                        .on_input(Message::ArtistDetailNameChanged)
                        .on_submit(Message::ArtistMetadataSave)
                        .style(input_style)
                        .width(Length::Fill),
                    text_input("Description (optional)…", &self.artist_description_input)
                        .on_input(Message::ArtistDescriptionChanged)
                        .on_submit(Message::ArtistMetadataSave)
                        .style(input_style)
                        .width(Length::Fill),
                    button(text("Save artist metadata").size(10))
                        .style(accent_button_style)
                        .padding([6, 9])
                        .on_press(Message::ArtistMetadataSave),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
            container(
                column![
                    text("ARTIST IMAGE").size(9).color(ACCENT),
                    text("Managed PNG/JPEG/WebP image. It is validated before publication.")
                        .size(9)
                        .color(MUTED),
                    row![
                        text_input("Artist image path…", &self.artist_image_input)
                            .on_input(Message::ArtistImageChanged)
                            .on_submit(Message::ArtistImageApply)
                            .style(input_style)
                            .width(Length::Fill),
                        button(text("Choose image…").size(10))
                            .style(accent_button_style)
                            .padding([6, 9])
                            .on_press(Message::ArtistImageChoose),
                        button(text("Assign / replace").size(10))
                            .style(ghost_style)
                            .padding([6, 9])
                            .on_press(Message::ArtistImageApply),
                        button(
                            text(if self.artist_image_remove_confirm {
                                "Confirm remove image"
                            } else {
                                "Remove image"
                            })
                            .size(10),
                        )
                        .style(danger_button_style)
                        .padding([6, 9])
                        .on_press_maybe(
                            self.controller
                                .artist_image_path(artist_id)
                                .is_some()
                                .then_some(Message::ArtistImageRemove),
                        ),
                    ]
                    .spacing(6),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
            text("ALBUMS").size(9).color(ACCENT),
        ];
        if album_ids.is_empty() {
            body = body.push(
                text("No albums associated with this artist yet.")
                    .size(11)
                    .color(MUTED),
            );
        } else {
            for album_id in album_ids {
                let Some(album) = self.controller.album(album_id) else {
                    continue;
                };
                body = body.push(
                    button(
                        row![
                            self.album_artwork(Some(album_id), 34.0, ContentFit::Cover),
                            column![
                                text(album.title.clone()).size(12).color(TEXT),
                                text(format!(
                                    "{} · {} track(s) · owner {}",
                                    release_type_label(album.release_type),
                                    album.track_order.len(),
                                    self.controller
                                        .album_owner(album_id)
                                        .and_then(|owner| self.controller.artist(owner))
                                        .map(|owner| owner.name().to_owned())
                                        .unwrap_or_else(|| "unknown".to_owned())
                                ))
                                .size(9)
                                .color(MUTED),
                            ]
                            .spacing(1)
                            .width(Length::Fill),
                            text("›").size(18).color(MUTED),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding(7)
                    .style(group_style)
                    .on_press(Message::OpenAlbum(album_id)),
                );
            }
        }
        body = body.push(text("TRACKS").size(9).color(ACCENT));
        if tracks.is_empty() {
            body = body.push(
                text("No tracks associated with this artist yet.")
                    .size(11)
                    .color(MUTED),
            );
        } else {
            for track_id in tracks {
                let Some(track) = self.controller.track(track_id) else {
                    continue;
                };
                let album = self
                    .controller
                    .track_album_id(track_id)
                    .and_then(|album| self.controller.album(album))
                    .map(|album| album.title.clone())
                    .unwrap_or_else(|| "Standalone".to_owned());
                body = body.push(
                    row![
                        self.track_artwork(
                            track_id,
                            self.controller.track_album_id(track_id),
                            30.0,
                            ContentFit::Cover
                        ),
                        column![
                            text(track.title()).size(12).color(TEXT),
                            text(format!("{} · {album}", self.credit_display(track_id)))
                                .size(9)
                                .color(MUTED),
                        ]
                        .spacing(1)
                        .width(Length::Fill),
                        button(text("Play").size(9))
                            .style(ghost_style)
                            .padding([5, 8])
                            .on_press(Message::PlayTrack(track_id)),
                        button(text("Edit").size(9))
                            .style(ghost_style)
                            .padding([5, 8])
                            .on_press(Message::EditTrack(track_id)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .padding([5, 7]),
                );
            }
        }
        if let Some(message) = &self.lifecycle_msg {
            body = body.push(text(message.clone()).size(9).color(MUTED));
        }
        container(scrollable(body).height(Length::Fill))
            .padding(16)
            .width(Length::Fill)
            .into()
    }

    fn album_detail_view(&self, album_id: AlbumId) -> Element<'_, Message> {
        let Some(album) = self.controller.album(album_id) else {
            return text("Album is no longer in the library.")
                .size(13)
                .color(MUTED)
                .into();
        };
        let owner = self
            .controller
            .artist(album.owner_artist_id)
            .map(|artist| artist.name().to_owned())
            .unwrap_or_else(|| format!("Artist {}", album.owner_artist_id));
        let mut body = column![
            row![
                self.album_artwork(Some(album_id), 112.0, ContentFit::Contain),
                column![
                    text(album.title.clone()).size(22).color(TEXT),
                    text(format!(
                        "AlbumId {} · owner {} · {} · {} track(s) · cover {}",
                        album.id,
                        owner,
                        release_type_label(album.release_type),
                        album.track_order.len(),
                        if album.cover.is_some() {
                            "managed"
                        } else {
                            "none"
                        }
                    ))
                    .size(10)
                    .color(MUTED),
                ]
                .spacing(3)
                .width(Length::Fill),
                button(text("Open artist").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::OpenArtist(album.owner_artist_id)),
                button(text("Add tracks…").size(10))
                    .style(accent_button_style)
                    .padding([7, 10])
                    .on_press(Message::AlbumAddTracks),
                button(text("Transfer album…").size(10))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press(Message::AlbumTransferOpen),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ];
        if let Some(error) = self.album_artwork_error(album_id) {
            body = body.push(
                text(format!("Artwork could not be displayed: {error}. The managed file is unchanged; choose another image or remove the cover reference."))
                    .size(9)
                    .color(DANGER),
            );
        } else if album.cover.is_some()
            && !self.artwork.contains_key(&album_id)
            && self
                .controller
                .album_cover_path(album_id)
                .is_some_and(|path| path.is_file())
        {
            body = body.push(text("Loading artwork…").size(9).color(MUTED));
        }
        body = body.push(
            container(
                column![
                    text("METADATA").size(9).color(ACCENT),
                    row![
                        text_input("Album title…", &self.album_title_input)
                            .on_input(Message::AlbumTitleChanged)
                            .on_submit(Message::AlbumMetadataSave)
                            .style(input_style)
                            .width(Length::Fill),
                        button(text("Save metadata").size(10))
                            .style(accent_button_style)
                            .padding([6, 9])
                            .on_press(Message::AlbumMetadataSave),
                    ]
                    .spacing(6),
                    row![
                        text("RELEASE TYPE").size(8).color(MUTED),
                        release_type_buttons(self.album_type, Message::AlbumType),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
        );
        body = body.push(
            container(
                column![
                    text("COVER ARTWORK").size(9).color(ACCENT),
                    text("Local image path; PNG/JPEG/WebP bytes are preserved. No online fetch or tag extraction.")
                        .size(9)
                        .color(MUTED),
                    row![
                        text_input("Cover image path…", &self.album_cover_input)
                            .on_input(Message::AlbumCoverChanged)
                            .on_submit(Message::AlbumCoverApply)
                            .style(input_style)
                            .width(Length::Fill),
                        button(text("Choose image…").size(10))
                            .style(accent_button_style)
                            .padding([6, 9])
                            .on_press(Message::AlbumCoverChoose),
                        button(text("Assign / replace").size(10))
                            .style(ghost_style)
                            .padding([6, 9])
                            .on_press(Message::AlbumCoverApply),
                        button(
                            text(if self.album_cover_remove_confirm {
                                "Confirm remove cover"
                            } else {
                                "Remove cover"
                            })
                            .size(10),
                        )
                        .style(danger_button_style)
                        .padding([6, 9])
                        .on_press_maybe(album.cover.is_some().then_some(Message::AlbumCoverRemove)),
                    ]
                    .spacing(6),
                ]
                .spacing(6),
            )
            .padding(10)
            .style(card_style),
        );
        match self.controller.album_cover_resource_state(album_id) {
            Some(ResourceState::Missing) => body = body.push(
                text("The catalogue references a cover file that is missing. Assign a new image or remove the reference.")
                    .size(10)
                    .color(DANGER),
            ),
            Some(ResourceState::Unavailable(reason)) => body = body.push(
                text(format!(
                    "The catalogue references a cover file that cannot be inspected ({reason}). Flux will not overwrite or remove an inaccessible resource; repair permissions first."
                ))
                .size(10)
                .color(DANGER),
            ),
            _ => {}
        }
        if let Some(batch) = self.batch_action_bar() {
            body = body.push(batch);
        }
        if album.track_order.is_empty() {
            body = body.push(
                container(
                    column![
                        text("This album is intentionally empty.").size(14).color(TEXT),
                        text("It is a first-class catalogue album: metadata, artwork, transfer and safe deletion are available without fake tracks. Add audio through the Import Manager with the same owner and album title, or keep it as an independent release.")
                            .size(10)
                            .color(MUTED),
                        row![
                            button(text("Add tracks…").size(10))
                                .style(accent_button_style)
                                .padding([6, 9])
                                .on_press(Message::AlbumAddTracks),
                            button(text("Import music").size(10))
                                .style(ghost_style)
                                .padding([6, 9])
                                .on_press(Message::ImportOpen),
                            if self.album_delete_confirm {
                                button(text("Confirm delete empty album").size(10))
                                    .style(danger_button_style)
                                    .padding([6, 9])
                                    .on_press(Message::AlbumDeleteConfirm)
                            } else {
                                button(text("Delete empty album…").size(10))
                                    .style(danger_button_style)
                                    .padding([6, 9])
                                    .on_press(Message::AlbumDeletePressed)
                            },
                        ]
                        .spacing(6),
                    ]
                    .spacing(6),
                )
                .padding(14)
                .style(card_style),
            );
        } else {
            let mut list = column![].spacing(2);
            for (index, track_id) in album.track_order.iter().enumerate() {
                let Some(track) = self.controller.track(*track_id) else {
                    continue;
                };
                let resource =
                    resource_badge(self.controller.track_audio_resource_state(*track_id));
                let mut title_row = row![text(track.title()).size(12).color(TEXT)].spacing(6);
                match resource {
                    Some(TrackResourceBadge::Missing) => {
                        title_row = title_row.push(text("missing").size(8).color(DANGER));
                    }
                    Some(TrackResourceBadge::Inaccessible) => {
                        title_row = title_row.push(text("inaccessible").size(8).color(DANGER));
                    }
                    None => {}
                }
                if self.controller.track_artwork_path(*track_id).is_some() {
                    title_row = title_row.push(text("artwork").size(8).color(ACCENT));
                }
                list = list.push(
                    container(
                        row![
                            text(format!("{}", index + 1))
                                .size(10)
                                .color(MUTED)
                                .width(Length::Fixed(24.0)),
                            self.track_artwork(*track_id, Some(album_id), 30.0, ContentFit::Cover),
                            column![
                                title_row,
                                text(self.credit_display(*track_id)).size(9).color(MUTED),
                            ]
                            .spacing(1)
                            .width(Length::Fill),
                            text(
                                track
                                    .duration()
                                    .map(format_duration)
                                    .unwrap_or_else(|| "--:--".to_owned())
                            )
                            .size(9)
                            .color(MUTED)
                            .width(Length::Fixed(46.0)),
                            button(text("↑").size(10))
                                .style(ghost_style)
                                .padding([4, 6])
                                .on_press(Message::AlbumTrackMove(*track_id, -1)),
                            button(text("↓").size(10))
                                .style(ghost_style)
                                .padding([4, 6])
                                .on_press(Message::AlbumTrackMove(*track_id, 1)),
                            button(text("✎").size(10))
                                .style(ghost_style)
                                .padding([4, 6])
                                .on_press(Message::EditTrack(*track_id)),
                        ]
                        .spacing(5)
                        .align_y(Alignment::Center),
                    )
                    .padding(6)
                    .style(card_style),
                );
            }
            body = body.push(
                container(scrollable(list).height(Length::Fill))
                    .padding(6)
                    .style(card_style),
            );
        }
        if let Some(message) = &self.lifecycle_msg {
            body = body.push(text(message.clone()).size(9).color(MUTED));
        }
        container(scrollable(body).height(Length::Fill))
            .padding(16)
            .width(Length::Fill)
            .into()
    }

    fn track_list<'a>(&'a self, rows: Vec<(TrackId, &'a Track)>) -> Element<'a, Message> {
        if let Some(album_id) = self.selected_album_id() {
            return self.album_detail_view(album_id);
        }
        let total = rows.len();
        if total == 0 {
            let (message, hint) = track_list_empty_copy(
                self.view,
                self.filters_active(),
                self.selected_group.is_some(),
            );
            return container(
                column![
                    text(message).size(15).color(TEXT),
                    text(hint).size(11).color(MUTED),
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
        let editable = matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        );

        let header = row![
            text("SEL").size(8).color(MUTED).width(Length::Fixed(22.0)),
            text("#").size(9).color(MUTED).width(Length::Fixed(28.0)),
            text("TITLE")
                .size(9)
                .color(MUTED)
                .width(Length::FillPortion(5)),
            text("ALBUM")
                .size(9)
                .color(MUTED)
                .width(Length::FillPortion(4)),
            text("RATE").size(8).color(MUTED).width(Length::Fixed(58.0)),
            text("TIME").size(9).color(MUTED).width(Length::Fixed(48.0)),
            text("+").size(9).color(MUTED).width(Length::Fixed(26.0)),
            text("NEXT").size(9).color(MUTED).width(Length::Fixed(34.0)),
            text("EDIT").size(9).color(MUTED).width(Length::Fixed(36.0)),
        ]
        .spacing(8)
        .padding([2, 10]);

        let mut list = column![].spacing(1);
        for (offset, (id, track)) in rows[start..end].iter().enumerate() {
            let resource = resource_badge(self.controller.track_audio_resource_state(*id));
            list = list.push(track_row(
                start + offset + 1,
                *id,
                track,
                &self.credit_display(*id),
                TrackRowState {
                    current,
                    resource,
                    editable,
                    checked: self.selected_tracks.contains(id),
                    focused: self.focused_track == Some(*id),
                    rating: self.controller.rating(*id).and_then(|rating| rating.rating),
                    favorite: self
                        .controller
                        .rating(*id)
                        .is_some_and(|rating| rating.favorite),
                },
            ));
        }

        let mut content = column![].spacing(8);
        if let Some(album_id) = self.selected_album_id()
            && let Some(album) = self
                .controller
                .albums()
                .into_iter()
                .find(|album| album.id == album_id)
        {
            let owner = self
                .controller
                .artist(album.owner_artist_id)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {}", album.owner_artist_id));
            content = content.push(
                container(
                    row![
                        column![
                            text(format!("Album · {}", album.title))
                                .size(12)
                                .color(TEXT),
                            text(format!(
                                "owner {owner} · {} track(s) · AlbumId {}",
                                album.track_order.len(),
                                album.id
                            ))
                            .size(9)
                            .color(MUTED),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        button(text("Transfer album…").size(10))
                            .style(accent_button_style)
                            .padding([6, 10])
                            .on_press(Message::AlbumTransferOpen),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding(9)
                .style(card_style),
            );
        }
        if let Some(batch) = self.batch_action_bar() {
            content = content.push(batch);
        }
        content = content.push(header);
        content = content.push(horizontal_rule());
        content = content.push(scrollable(list).height(Length::Fill));
        content = content.push(pagination_footer(page, pages, start, end, total));
        content.height(Length::Fill).into()
    }

    fn setup_view(&self) -> Element<'_, Message> {
        let scanning = self.controller.is_scanning();
        let protected = self.controller.legacy_index_write_protected();
        let lib_ready = !scanning && !self.lib_input.trim().is_empty();

        let mut setup = column![].spacing(12);

        let (audio_title, audio_detail, audio_color) =
            audio_summary(self.controller.audio_status());
        let mut audio_status = column![
            text(audio_title).size(12).color(audio_color),
            text(audio_detail).size(10).color(MUTED),
        ]
        .spacing(4);
        if self.controller.can_retry_audio_output() {
            audio_status = audio_status.push(
                row![
                    button(text("Retry audio output").size(11))
                        .style(accent_button_style)
                        .padding([7, 12])
                        .on_press(Message::RetryAudioOutput),
                    text(
                        "Automatic recovery attempts were exhausted. Retry reopens the \
                         current default device; Play is not required.",
                    )
                    .size(9)
                    .color(MUTED),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        setup = setup.push(setup_card(
            "AUDIO OUTPUT",
            "The actual result of initializing this session's output backend. This is \
             information about the build or device, not a library error.",
            audio_status,
        ));

        setup = setup.push(setup_card(
            "IMPORT LOCAL MUSIC",
            "Import through the review-first manager. Every audio file is copied into the \
             canonical library; filenames and folders provide provisional metadata and \
             audio tags are never read.",
            column![
                row![
                    button(text("Choose folder…").size(11))
                        .style(accent_button_style)
                        .padding([7, 12])
                        .on_press(Message::ImportNativeFolder),
                    button(text("Choose files…").size(11))
                        .style(ghost_style)
                        .padding([7, 12])
                        .on_press(Message::ImportNativeFiles),
                    button(text("Refresh").size(11))
                        .style(ghost_style)
                        .padding([7, 12])
                        .on_press_maybe(
                            (!scanning && !protected).then_some(Message::RefreshPressed)
                        ),
                ]
                .spacing(8),
                text(if scanning {
                    "Scanning… the library updates when the worker finishes."
                } else {
                    "Metadata is reviewed before commit; external references are not supported."
                })
                .size(10)
                .color(MUTED),
            ]
            .spacing(8),
        ));

        let portable = matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        );

        let library_label = match self.controller.storage_mode() {
            StorageMode::Legacy => match self.controller.legacy_index_path() {
                Some(path) => format!("Active: app index — {}", path.display()),
                None => "Active: session-only legacy library".to_owned(),
            },
            StorageMode::Portable => match self.controller.portable_root() {
                Some(root) => format!("Active: portable library — {}", root.display()),
                None => "Active: portable library".to_owned(),
            },
            StorageMode::Canonical => match self.controller.canonical_root() {
                Some(root) => format!("Active: canonical library — {}", root.display()),
                None => "Active: canonical library".to_owned(),
            },
        };

        let mut portable_body = column![text(library_label).size(10).color(MUTED)].spacing(8);
        portable_body = portable_body.push(
            row![
                text_input("Canonical library folder…", &self.lib_input)
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
                button(text("Use app index").size(11))
                    .style(ghost_style)
                    .padding([6, 10])
                    .on_press_maybe((!scanning).then_some(Message::UseLegacyIndexPressed)),
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
            "CANONICAL LIBRARY",
            "A canonical library keeps managed audio, catalogue and metadata together in \
             one portable root. Every persisted resource path is relative to that root.",
            portable_body,
        ));

        let choices = self.artist_choices();
        let selected_artist = self
            .artist_selected
            .and_then(|id| choices.iter().find(|choice| choice.id == Some(id)).cloned());
        let artist_editable = portable && !scanning;
        let mut artist_body = column![
            text(
                "Stable artist identities are catalogue data. Renaming one updates every \
                 related track without changing its overrides or audio; equal display names \
                 remain distinct identities."
            )
            .size(10)
            .color(MUTED),
            pick_list(choices, selected_artist, |choice| Message::ArtistSelected(
                Some(choice)
            ))
            .text_size(12)
            .padding([7, 9])
            .width(Length::Fill),
            row![
                text_input("New artist name…", &self.artist_name_input)
                    .on_input(Message::ArtistNameChanged)
                    .on_submit(Message::ArtistCreatePressed)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Create artist").size(11))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe(
                        (artist_editable && !self.artist_name_input.trim().is_empty())
                            .then_some(Message::ArtistCreatePressed)
                    ),
            ]
            .spacing(8),
            row![
                text_input("Rename selected artist…", &self.artist_rename_input)
                    .on_input(Message::ArtistRenameNameChanged)
                    .on_submit(Message::ArtistRenamePressed)
                    .style(input_style)
                    .width(Length::Fill),
                button(text("Rename").size(11))
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe(
                        (artist_editable
                            && self.artist_selected.is_some()
                            && !self.artist_rename_input.trim().is_empty())
                        .then_some(Message::ArtistRenamePressed)
                    ),
            ]
            .spacing(8),
            button(text("Delete artist…").size(11))
                .style(danger_button_style)
                .padding([7, 10])
                .on_press_maybe(
                    (artist_editable && self.artist_selected.is_some())
                        .then_some(Message::ArtistDeleteOpen)
                ),
            text("CREATE EMPTY ALBUM").size(8).color(ACCENT),
            text_input("New album title…", &self.new_album_title)
                .on_input(Message::NewAlbumTitleChanged)
                .on_submit(Message::AlbumCreatePressed)
                .style(input_style)
                .width(Length::Fill),
            row![
                text("RELEASE TYPE").size(8).color(MUTED),
                release_type_buttons(self.new_album_type, Message::NewAlbumType),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            button(text("Create empty album").size(11))
                .style(accent_button_style)
                .padding([7, 10])
                .on_press_maybe(
                    (artist_editable
                        && self.artist_selected.is_some()
                        && !self.new_album_title.trim().is_empty())
                    .then_some(Message::AlbumCreatePressed)
                ),
        ]
        .spacing(8);
        if !portable {
            artist_body = artist_body.push(
                text("Artist management requires a canonical library.")
                    .size(10)
                    .color(DANGER),
            );
        }
        setup = setup.push(setup_card(
            "ARTISTS",
            "Create, select, and rename stable canonical artist identities.",
            artist_body,
        ));

        let mut health_body = column![
            row![
                button(
                    text(if self.controller.storage_mode() == StorageMode::Canonical {
                        "Refresh & validate"
                    } else {
                        "Reconcile library"
                    })
                    .size(11)
                )
                    .style(ghost_style)
                    .padding([7, 10])
                    .on_press_maybe(
                        (portable && !scanning && self.controller.portable_recovery().is_none())
                            .then_some(Message::ReconcilePressed)
                    ),
                text(if self.controller.storage_mode() == StorageMode::Canonical {
                    "Reopens the canonical library and validates its manifest, catalogue and managed resource paths."
                } else {
                    "Checks every recorded file for changes, missing files, and safe move candidates. Nothing is moved automatically."
                })
                    .size(10)
                    .color(MUTED),
            ]
            .spacing(8),
        ]
        .spacing(8);
        if let Some(report) = &self.last_reconcile {
            health_body = health_body.push(text(reconcile_summary(report)).size(10).color(MUTED));
            for candidates in &report.relink_candidates {
                if let [candidate] = candidates.candidates.as_slice() {
                    let title = self
                        .controller
                        .track(candidates.id)
                        .map(|track| track.title().to_owned())
                        .unwrap_or_else(|| format!("Track {}", candidates.id));
                    health_body = health_body.push(
                        row![
                            column![
                                text(title).size(11).color(TEXT),
                                text(candidate.display_lossy()).size(9).color(MUTED),
                            ]
                            .spacing(2)
                            .width(Length::Fill),
                            button(text("Relink").size(10))
                                .style(ghost_style)
                                .padding([5, 9])
                                .on_press(Message::RelinkTrack(candidates.id, candidate.clone())),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    );
                } else {
                    health_body = health_body.push(
                        text(format!(
                            "Track {} has {} possible locations; resolve the ambiguity outside Flux first.",
                            candidates.id,
                            candidates.candidates.len()
                        ))
                        .size(9)
                        .color(DANGER),
                    );
                }
            }
        }
        setup = setup.push(setup_card(
            "LIBRARY HEALTH",
            "Missing records are retained with their IDs and metadata.",
            health_body,
        ));

        let mut migration_body = column![
            row![
                text_input(
                    "Legacy index file or portable library folder…",
                    &self.migrate_source
                )
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
            "MIGRATE TO CANONICAL",
            "Inventory a legacy index or portable schema-v3 library and build a separate \
             canonical library. The source is never modified or activated automatically.",
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

    fn queue_overlay(&self) -> Element<'_, Message> {
        let backdrop = mouse_area(
            container(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(modal_backdrop_style),
        )
        .on_press(Message::ToggleQueue);
        stack([
            backdrop.into(),
            container(
                row![
                    iced::widget::Space::new().width(Length::Fill),
                    self.queue_panel(true),
                ]
                .height(Length::Fill),
            )
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        ])
        .height(Length::Fill)
        .into()
    }

    fn queue_panel(&self, overlay: bool) -> Element<'_, Message> {
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

        let current_album =
            current_entry.and_then(|entry| self.controller.track_album_id(entry.track()));
        let mut heading =
            row![text("NOW PLAYING").size(9).color(ACCENT)].align_y(Alignment::Center);
        if overlay {
            heading = heading.push(iced::widget::Space::new().width(Length::Fill));
            heading = heading.push(
                button(text("Close").size(9))
                    .style(ghost_style)
                    .padding([4, 7])
                    .on_press(Message::ToggleQueue),
            );
        }
        let now_art = current_entry.map_or_else(
            || self.album_artwork(current_album, 176.0, ContentFit::Cover),
            |entry| self.track_artwork(entry.track(), current_album, 176.0, ContentFit::Cover),
        );
        let mut panel = column![
            heading,
            container(now_art)
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
                    resource_badge(self.controller.track_audio_resource_state(entry.track())),
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
        let audio_ready = self.controller.audio_status().is_ready();
        let can_attempt = self.controller.can_attempt_playback();
        let playable = current.is_some_and(is_track_playable);
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

        let total = self.controller.current_duration();
        let total_secs = total.map_or(0.0, |duration| duration.as_secs_f32());
        let position = self
            .seek_preview
            .unwrap_or_else(|| self.controller.position().as_secs_f32())
            .clamp(0.0, total_secs.max(0.0));
        let can_seek = can_attempt && playable && total.is_some_and(|d| !d.is_zero());

        let transport_enabled = can_attempt && current.is_some();
        let play_enabled = can_attempt && playable;
        let skip_enabled = transport_enabled && self.controller.queue_len() > 0;
        let stop_enabled = transport_enabled && state != PlaybackState::Stopped;

        let transport = row![
            transport_button("|<", "Previous", false, skip_enabled, Message::Previous),
            transport_button(
                if state == PlaybackState::Playing {
                    "II"
                } else {
                    "▶"
                },
                if state == PlaybackState::Playing {
                    "Pause"
                } else {
                    "Play"
                },
                true,
                play_enabled,
                Message::TogglePlayPause,
            ),
            transport_button(">|", "Next", false, skip_enabled, Message::Next),
            transport_button("■", "Stop", false, stop_enabled, Message::Stop),
            repeat_button(self.controller.playback_mode()),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let controls = container(transport)
            .width(Length::Fill)
            .center_x(Length::Fill);

        let total_label = total
            .filter(|duration| !duration.is_zero())
            .map_or_else(|| "--:--".to_owned(), format_duration);
        let elapsed = text(format_duration(Duration::from_secs_f32(position)))
            .size(9)
            .color(if self.seek_preview.is_some() {
                ACCENT
            } else {
                MUTED
            });

        let progress_bar_element: Element<'_, Message> = if can_seek {
            let slider = slider(0.0..=total_secs, position, Message::SeekPreview)
                .step(0.1_f32)
                .shift_step(5.0_f32)
                .height(18.0)
                .width(Length::Fill)
                .style(seek_style)
                .on_release(Message::SeekCommit);
            slider.into()
        } else {
            let progress = if total_secs > 0.0 {
                (position / total_secs).clamp(0.0, 1.0)
            } else {
                0.0
            };
            progress_bar(0.0..=1.0, progress)
                .girth(4)
                .style(progress_style)
                .length(Length::Fill)
                .into()
        };

        let progress_row = row![
            elapsed,
            progress_bar_element,
            text(total_label).size(9).color(MUTED),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut info = column![
            text(title)
                .size(15)
                .color(if current.is_some() { TEXT } else { MUTED }),
            text(subtitle).size(10).color(MUTED),
        ]
        .spacing(2)
        .width(Length::Fill);
        if !audio_ready {
            let (message, color) = match self.controller.audio_status() {
                AudioStatus::Recovering(_) => (
                    "Audio recovering · playback follows the current state when the device returns"
                        .to_owned(),
                    ACCENT,
                ),
                AudioStatus::DeviceUnavailable(_) if self.controller.can_retry_audio_output() => (
                    "Audio unavailable · retry in Library setup, or press Play to retry".to_owned(),
                    DANGER,
                ),
                AudioStatus::DeviceUnavailable(_) => (
                    "Audio unavailable · check the output device in Library setup".to_owned(),
                    DANGER,
                ),
                AudioStatus::BuildWithoutBackend => (
                    "Audio unavailable · this build has no output backend".to_owned(),
                    MUTED,
                ),
                AudioStatus::Ready => (String::new(), MUTED),
            };
            info = info.push(text(message).size(9).color(color));
        }
        let compact = self.window_width < 1_080.0;
        let current_album =
            current_entry.and_then(|entry| self.controller.track_album_id(entry.track()));
        let player_art = current_entry.map_or_else(
            || self.album_artwork(current_album, 54.0, ContentFit::Cover),
            |entry| self.track_artwork(entry.track(), current_album, 54.0, ContentFit::Cover),
        );
        let left = container(
            row![player_art, info]
                .spacing(12)
                .align_y(Alignment::Center),
        )
        .width(Length::Fixed(if compact { 220.0 } else { 260.0 }));

        let center = column![controls, progress_row]
            .spacing(4)
            .width(Length::Fill);

        let volume_width = if compact { 78.0 } else { 105.0 };
        let right = container(
            row![
                text("VOL").size(9).color(MUTED),
                slider(0.0..=1.0, self.controller.volume(), Message::VolumeChanged)
                    .step(0.01_f32)
                    .style(volume_style)
                    .width(volume_width),
                text(format!(
                    "{:>3}%",
                    (self.controller.volume() * 100.0).round() as u32
                ))
                .size(9)
                .color(MUTED),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Length::Fixed(if compact { 170.0 } else { 190.0 }))
        .align_x(Alignment::End);

        let top = row![left, center, right]
            .spacing(18)
            .align_y(Alignment::Center);

        container(
            column![top, self.oscilloscope()]
                .spacing(8)
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

#[derive(Debug, Clone, Copy)]
struct TrackRowState {
    current: Option<TrackId>,
    resource: Option<TrackResourceBadge>,
    editable: bool,
    checked: bool,
    focused: bool,
    rating: Option<u8>,
    favorite: bool,
}

fn track_row<'a>(
    index: usize,
    id: TrackId,
    track: &'a Track,
    credits: &str,
    state: TrackRowState,
) -> Element<'a, Message> {
    let selected = state.current == Some(id) || state.focused;
    let marker = if selected {
        "▶".to_owned()
    } else {
        index.to_string()
    };
    let title_color = if selected { ACCENT } else { TEXT };
    let duration = track.duration().map(format_duration).unwrap_or_default();
    let playable = is_track_playable(track);

    let mut title = row![text(track.title()).size(13).color(title_color)]
        .spacing(8)
        .align_y(Alignment::Center);
    match state.resource {
        Some(TrackResourceBadge::Missing) => {
            title = title.push(text("missing").size(9).color(DANGER));
        }
        Some(TrackResourceBadge::Inaccessible) => {
            title = title.push(text("inaccessible").size(9).color(DANGER));
        }
        None => {}
    }
    if !playable {
        title = title.push(text("indexed, not playable").size(9).color(MUTED));
    }

    let content = row![
        text(marker)
            .size(10)
            .color(if selected { ACCENT } else { MUTED })
            .width(Length::Fixed(28.0)),
        column![title, text(credits.to_owned()).size(10).color(MUTED),]
            .spacing(2)
            .width(Length::FillPortion(5)),
        text(display_album(track))
            .size(10)
            .color(MUTED)
            .width(Length::FillPortion(4)),
        button(text(if state.favorite { "♥" } else { "♡" }).size(12))
            .style(ghost_style)
            .padding([4, 5])
            .on_press(Message::TrackFavoriteToggle(id)),
        button(
            text(
                state
                    .rating
                    .map_or_else(|| "☆".to_owned(), |rating| format!("{rating}★")),
            )
            .size(10),
        )
        .style(ghost_style)
        .padding([4, 5])
        .on_press(Message::TrackRatingSet(
            id,
            match state.rating {
                None => Some(1),
                Some(rating) if rating < 5 => Some(rating + 1),
                Some(_) => None,
            },
        )),
        text(duration)
            .size(10)
            .color(MUTED)
            .width(Length::Fixed(48.0)),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    let check = button(text(if state.checked { "✓" } else { "○" }).size(11))
        .style(ghost_style)
        .padding([5, 7])
        .on_press(Message::TrackSelectionToggle(id));
    let main = button(content)
        .width(Length::Fill)
        .padding([7, 10])
        .style(track_row_style(selected || state.checked))
        .on_press(Message::PlayTrack(id));
    let add = button(text("+").size(12))
        .style(ghost_style)
        .padding([5, 7])
        .on_press(Message::EnqueueTrack(id));
    let next = button(text("→").size(12))
        .style(ghost_style)
        .padding([5, 7])
        .on_press(Message::PlayNextTrack(id));
    let edit = button(text("✎").size(11))
        .style(ghost_style)
        .padding([5, 7])
        .on_press_maybe(state.editable.then_some(Message::EditTrack(id)));
    let edit = tooltip(
        edit,
        container(
            text(if state.editable {
                "Edit metadata"
            } else {
                "Use a canonical library to edit"
            })
            .size(10)
            .color(TEXT),
        )
        .padding([4, 7])
        .style(tooltip_style),
        tooltip::Position::Top,
    )
    .gap(6)
    .delay(Duration::from_millis(350));

    row![check, main, add, next, edit]
        .spacing(2)
        .align_y(Alignment::Center)
        .into()
}

fn playlist_entry_row<'a>(
    position: usize,
    track_id: TrackId,
    track: &'a Track,
    current: Option<TrackId>,
    checked: bool,
    dragging: bool,
    resource: Option<TrackResourceBadge>,
) -> Element<'a, Message> {
    let is_current = current == Some(track_id);
    let duration = track.duration().map(format_duration).unwrap_or_default();
    let title_color = if is_current { ACCENT } else { TEXT };
    let check = button(text(if checked { "✓" } else { "○" }).size(11))
        .style(ghost_style)
        .padding([4, 5])
        .on_press(Message::PlaylistEntryToggle(position));
    let play = button(text(if is_current { "▶" } else { "›" }).size(12))
        .style(ghost_style)
        .padding([4, 5])
        .on_press(Message::PlaylistPlayEntry(position));
    let queue = button(text("+").size(11))
        .style(ghost_style)
        .padding([4, 5])
        .on_press(Message::PlaylistEnqueueEntry(position));
    let up = button(text("↑").size(10))
        .style(ghost_style)
        .padding([4, 5])
        .on_press_maybe((position > 0).then_some(Message::PlaylistEntryMove(position, -1)));
    let down = button(text("↓").size(10))
        .style(ghost_style)
        .padding([4, 5])
        .on_press(Message::PlaylistEntryMove(position, 1));
    let remove = button(text("×").size(11))
        .style(danger_button_style)
        .padding([4, 5])
        .on_press(Message::PlaylistEntryRemove(position));
    let on_move = move |point: iced::Point| {
        Message::PlaylistDragHover(if point.y < PLAYLIST_ROW_HEIGHT / 2.0 {
            position
        } else {
            position + 1
        })
    };
    let mut title_row = row![text(track.title()).size(12).color(title_color)].spacing(6);
    match resource {
        Some(TrackResourceBadge::Missing) => {
            title_row = title_row.push(text("missing").size(8).color(DANGER));
        }
        Some(TrackResourceBadge::Inaccessible) => {
            title_row = title_row.push(text("inaccessible").size(8).color(DANGER));
        }
        None => {}
    }
    mouse_area(
        row![
            text("≡").size(11).color(MUTED),
            check,
            text(format!("{}", position + 1))
                .size(9)
                .color(MUTED)
                .width(Length::Fixed(24.0)),
            play,
            column![title_row, text(display_artist(track)).size(9).color(MUTED),]
                .spacing(1)
                .width(Length::FillPortion(5)),
            text(display_album(track))
                .size(9)
                .color(MUTED)
                .width(Length::FillPortion(3)),
            text(duration)
                .size(9)
                .color(MUTED)
                .width(Length::Fixed(42.0)),
            queue,
            up,
            down,
            remove,
        ]
        .spacing(3)
        .height(Length::Fixed(PLAYLIST_ROW_HEIGHT))
        .align_y(Alignment::Center),
    )
    .on_press(Message::PlaylistDragStart(position))
    .on_move(on_move)
    .interaction(if dragging {
        mouse::Interaction::Grabbing
    } else {
        mouse::Interaction::Grab
    })
    .into()
}

fn queue_row<'a>(
    entry: QueueEntry,
    index: usize,
    current: Option<QueueEntryId>,
    dragging: bool,
    track: Option<&'a Track>,
    resource: Option<TrackResourceBadge>,
) -> Element<'a, Message> {
    let is_current = current == Some(entry.id());
    let playable = track.is_some_and(is_track_playable);
    let (title, subtitle, duration) = match track {
        Some(track) => (
            track.title().to_owned(),
            match resource {
                Some(TrackResourceBadge::Missing) => "Missing managed audio".to_owned(),
                Some(TrackResourceBadge::Inaccessible) => "Inaccessible audio".to_owned(),
                None if playable => display_artist(track),
                None => "Indexed, not playable".to_owned(),
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

fn transport_button<'a>(
    icon: &'static str,
    label: &'static str,
    primary: bool,
    enabled: bool,
    message: Message,
) -> Element<'a, Message> {
    let content = container(text(icon).size(if primary { 16 } else { 12 }))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill);
    let button = button(content)
        .width(Length::Fixed(if primary { 40.0 } else { 32.0 }))
        .height(Length::Fixed(32.0))
        .padding(0)
        .style(transport_button_style(primary, enabled))
        .on_press_maybe(enabled.then_some(message));
    tooltip(
        button,
        container(text(label).size(10).color(TEXT))
            .padding([4, 7])
            .style(tooltip_style),
        tooltip::Position::Top,
    )
    .gap(6)
    .delay(Duration::from_millis(350))
    .into()
}

fn repeat_button<'a>(mode: PlaybackMode) -> Element<'a, Message> {
    let icon = match mode {
        PlaybackMode::Normal => "↻",
        PlaybackMode::RepeatOne => "↻1",
        PlaybackMode::RepeatAll => "↻A",
        PlaybackMode::Shuffle => "⤨",
        PlaybackMode::ShuffleAll => "⤨A",
    };
    let content = container(text(icon).size(12))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill);
    let button = button(content)
        .width(Length::Fixed(32.0))
        .height(Length::Fixed(32.0))
        .padding(0)
        .style(nav_style(mode != PlaybackMode::Normal))
        .on_press(Message::CycleRepeat);
    tooltip(
        button,
        container(
            text(format!("Playback mode: {}", mode.label()))
                .size(10)
                .color(TEXT),
        )
        .padding([4, 7])
        .style(tooltip_style),
        tooltip::Position::Top,
    )
    .gap(6)
    .delay(Duration::from_millis(350))
    .into()
}

fn migration_plan_summary(plan: &CanonicalMigrationPlan) -> String {
    let issues = if plan.is_ready() {
        "no blocking issues".to_owned()
    } else {
        format!(
            "{} blocking issue(s): {}",
            plan.issues().len(),
            plan.issues()
                .iter()
                .take(3)
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    format!(
        "Canonical migration plan ({}): {} track(s), {} artist(s), {} album(s), \
         about {} MiB to copy; {}. The source is read-only. Track IDs and counters \
         are preserved where the source has them. Choose Execute only when there \
         are no blocking issues.",
        match plan.format() {
            flux_app::CanonicalSourceFormat::PortableSchemaV3 => "schema-v3",
            flux_app::CanonicalSourceFormat::LegacyIndex => "legacy index",
        },
        plan.track_count(),
        plan.artist_count(),
        plan.album_count(),
        plan.estimated_bytes() / (1024 * 1024),
        issues,
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
            search: String::new(),
            search_cache: RefCell::new(SearchCache::default()),
            selected_tracks: Vec::new(),
            focused_track: None,
            filter_favorites: false,
            filter_lyrics: TriFilter::Any,
            filter_artwork: TriFilter::Any,
            filter_release: ReleaseFilter::Any,
            sort_tracks: TrackSort::Default,
            sort_descending: false,
            filter_rating: RatingFilter::Any,
            collection: None,
            dashboard_cache: RefCell::new(None),
            batch_review: None,
            selected_playlist: None,
            playlist_entry_selection: Vec::new(),
            playlist_create_open: false,
            playlist_create_name: String::new(),
            playlist_name_input: String::new(),
            playlist_delete_confirm: false,
            playlist_clear_confirm: false,
            playlist_remove_confirm: false,
            playlist_drag: None,
            playlist_error: None,
            selected_lyrics: None,
            lyrics_mode: LyricsMode::Read,
            lyrics_follow_current: false,
            lyrics_filter: String::new(),
            lyrics_editor: None,
            lyrics_import: None,
            lyrics_export_path: String::new(),
            lyrics_export_pending: None,
            lyrics_error: None,
            lyrics_cache: RefCell::new(BTreeMap::new()),
            lib_input: String::new(),
            track_editor: None,
            import_manager: None,
            deletion_manager: None,
            album_transfer: None,
            external_drag_hover: false,
            artist_selected: None,
            artist_name_input: String::new(),
            artist_rename_input: String::new(),
            new_album_title: String::new(),
            new_album_type: ReleaseType::Album,
            album_title_input: String::new(),
            album_type: ReleaseType::Album,
            album_cover_input: String::new(),
            album_cover_picked: None,
            album_delete_confirm: false,
            album_cover_remove_confirm: false,
            artwork: BTreeMap::new(),
            artist_artwork: BTreeMap::new(),
            track_artwork: BTreeMap::new(),
            artwork_loading: HashSet::new(),
            artist_detail_name_input: String::new(),
            artist_description_input: String::new(),
            artist_image_input: String::new(),
            artist_image_picked: None,
            artist_image_remove_confirm: false,
            last_reconcile: None,
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
            seek_preview: None,
            crossfade_preview: None,
            show_setup: false,
            queue_visible: true,
            queue_overlay_open: false,
            window_width: 1_180.0,
            closed: false,
        }
    }

    fn gui_with_settings(dir: &TempDir, settings: &std::path::Path) -> FluxReady {
        let (engine, audio) = flux_app::default_engine();
        let mut controller =
            Controller::new(engine, audio, Some(dir.path().join("library-index.json")))
                .expect("controller opens the test library");
        controller.load_settings_from_path(settings);
        FluxReady::new(controller)
    }

    fn send(gui: &mut FluxReady, message: Message) {
        let session = gui.controller.library_session();
        let _task = gui.update(message.with_library_session(session));
    }

    #[test]
    fn window_close_shuts_down_and_ignores_later_messages() {
        let dir = TempDir::new("gui-close-shutdown");
        let mut gui = gui(&dir);
        let state = gui.controller.player_state();
        let queue_len = gui.controller.queue_len();
        let current = gui.controller.current_queue_entry_id();
        let _ = gui.update(Message::WindowCloseRequested(iced::window::Id::unique()));
        assert!(gui.closed);
        assert!(gui.controller.is_shutdown());

        send(&mut gui, Message::TogglePlayPause);
        send(&mut gui, Message::Stop);
        send(&mut gui, Message::Next);
        assert_eq!(gui.controller.player_state(), state);
        assert_eq!(gui.controller.queue_len(), queue_len);
        assert_eq!(gui.controller.current_queue_entry_id(), current);
        assert!(gui.controller.poll().is_empty());
    }

    #[test]
    fn settings_view_reflects_loaded_crossfade_and_updates_controller() {
        let dir = TempDir::new("gui-crossfade-settings");
        let settings = dir.path().join("config").join("settings.json");
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("config dir");
        std::fs::write(
            &settings,
            br#"{"version":1,"crossfade":{"enabled":true,"duration_ms":4200}}"#,
        )
        .expect("seed settings");
        let mut gui = gui_with_settings(&dir, &settings);
        assert!(gui.controller.crossfade_enabled());
        assert_eq!(gui.controller.crossfade_duration_ms(), 4_200);
        assert_eq!(gui.controller.crossfade_ms(), 4_200);

        send(&mut gui, Message::SelectView(LibraryView::Settings));
        assert_eq!(gui.view, LibraryView::Settings);
        let _ = gui.settings_view();
        send(&mut gui, Message::CrossfadeEnabledChanged(false));
        assert!(!gui.controller.crossfade_enabled());
        assert_eq!(gui.controller.crossfade_ms(), 0);
        assert_eq!(gui.controller.crossfade_duration_ms(), 4_200);
        let saved = std::fs::read_to_string(&settings).expect("saved settings");
        assert!(saved.contains("\"enabled\": false"));
        assert!(saved.contains("\"duration_ms\": 4200"));

        send(&mut gui, Message::CrossfadeEnabledChanged(true));
        assert!(gui.controller.crossfade_enabled());
        assert_eq!(gui.controller.crossfade_ms(), 4_200);
        send(&mut gui, Message::CrossfadeDurationPreview(2.5));
        assert_eq!(gui.crossfade_preview, Some(2_500));
        send(&mut gui, Message::CrossfadeDurationCommit);
        assert_eq!(gui.crossfade_preview, None);
        assert_eq!(gui.controller.crossfade_duration_ms(), 2_500);
        assert_eq!(gui.controller.crossfade_ms(), 2_500);
        let saved = std::fs::read_to_string(&settings).expect("saved settings");
        assert!(saved.contains("\"duration_ms\": 2500"));

        send(&mut gui, Message::CrossfadeDurationPreview(1.0));
        assert_eq!(gui.crossfade_preview, Some(1_000));
        send(&mut gui, Message::SelectView(LibraryView::Tracks));
        assert_eq!(
            gui.crossfade_preview, None,
            "leaving Settings must not keep a stale preview"
        );
        gui.controller.shutdown();

        let restarted = gui_with_settings(&dir, &settings);
        assert!(restarted.controller.crossfade_enabled());
        assert_eq!(restarted.controller.crossfade_duration_ms(), 2_500);
        assert_eq!(restarted.controller.crossfade_ms(), 2_500);
        restarted.controller.shutdown();
    }

    #[test]
    fn settings_view_surfaces_save_failures_without_losing_runtime_state() {
        let dir = TempDir::new("gui-crossfade-save-failure");
        let settings = dir.path().join("config").join("settings.json");
        let mut gui = gui_with_settings(&dir, &settings);
        std::fs::create_dir_all(&settings).expect("block settings path");
        send(&mut gui, Message::CrossfadeEnabledChanged(true));
        assert!(gui.controller.crossfade_enabled());
        assert_eq!(gui.controller.crossfade_ms(), 3_000);
        assert!(
            gui.controller
                .last_error()
                .is_some_and(|message| message.contains("Could not save crossfade settings")),
            "{:?}",
            gui.controller.last_error()
        );
        gui.controller.shutdown();
    }

    #[test]
    fn settings_keyboard_shortcut_opens_the_settings_view() {
        let dir = TempDir::new("gui-crossfade-shortcut");
        let mut gui = gui_with_settings(&dir, &dir.path().join("config/settings.json"));
        send(
            &mut gui,
            Message::KeyPressed(Key::Character("k".into()), Modifiers::default()),
        );
        assert_eq!(gui.view, LibraryView::Settings);
        gui.controller.shutdown();
    }

    #[test]
    fn navigation_labels_queue_placement_and_resource_badges_are_responsive() {
        let dir = TempDir::new("gui-responsive-navigation");
        let mut gui = gui(&dir);
        assert_eq!(gui.queue_placement(), QueuePlacement::Inline);
        gui.window_width = 920.0;
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);
        send(
            &mut gui,
            Message::KeyPressed(Key::Character("q".into()), Modifiers::default()),
        );
        assert_eq!(gui.queue_placement(), QueuePlacement::Overlay);
        assert!(gui.queue_visible, "wide-window preference remains intact");
        let _ = gui.view();
        send(
            &mut gui,
            Message::KeyPressed(Key::Character("q".into()), Modifiers::default()),
        );
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);
        assert!(gui.queue_visible);

        gui.window_width = QUEUE_INLINE_MIN_WIDTH;
        assert_eq!(gui.queue_placement(), QueuePlacement::Inline);
        gui.queue_visible = false;
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);
        gui.queue_visible = true;
        let _ = gui.view();
        gui.window_width = 920.0;
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);
        let _ = gui.view();
        send(&mut gui, Message::ToggleQueue);
        assert_eq!(gui.queue_placement(), QueuePlacement::Overlay);
        send(&mut gui, Message::WindowResized(1_080.0));
        assert_eq!(gui.queue_placement(), QueuePlacement::Inline);
        assert!(!gui.queue_overlay_open);
        gui.window_width = 920.0;
        assert_eq!(gui.active_library_label(), "library-index.json");
        assert!(gui.active_library_path().is_some());
        send(&mut gui, Message::ToggleQueue);
        assert_eq!(gui.queue_placement(), QueuePlacement::Overlay);
        send(&mut gui, Message::SelectView(LibraryView::Playlists));
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);
        gui.search = "stale query".to_owned();
        gui.selected_group = Some("stale".to_owned());
        send(&mut gui, Message::SelectView(LibraryView::Artists));
        assert_eq!(gui.view, LibraryView::Artists);
        assert!(gui.search.is_empty());
        assert!(gui.selected_group.is_none());

        assert_eq!(truncate_label("short", 8), "short");
        assert_eq!(truncate_label("abcdefghij", 6), "abcde…");
        assert_eq!(truncate_label("ééééé", 3), "éé…");
        assert_eq!(truncate_label("anything", 0), "");
        assert_eq!(
            resource_badge(Some(ResourceState::Missing)),
            Some(TrackResourceBadge::Missing)
        );
        assert_eq!(
            resource_badge(Some(ResourceState::Unavailable(
                "permission denied".to_owned()
            ))),
            Some(TrackResourceBadge::Inaccessible)
        );
        assert_eq!(resource_badge(Some(ResourceState::Present)), None);
        assert_eq!(resource_badge(None), None);
        gui.controller.shutdown();
    }

    #[test]
    fn empty_track_copy_distinguishes_filters_history_collections_and_groups() {
        assert_eq!(
            track_list_empty_copy(LibraryView::Tracks, true, false).0,
            "No tracks match the current filters."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::RecentlyAdded, false, false).0,
            "No recently added tracks yet."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::RecentlyPlayed, false, false).0,
            "No listening history yet."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::MostPlayed, false, false).0,
            "No listening history yet."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::Collections, false, false).0,
            "No tracks match this Smart Collection."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::Artists, false, true).0,
            "No tracks in this group."
        );
        assert_eq!(
            track_list_empty_copy(LibraryView::Tracks, false, false).0,
            "No tracks in this collection."
        );
    }

    #[test]
    fn import_partial_success_and_browser_omissions_are_reported() {
        let partial = ImportApplyReport {
            imported: 0,
            updated: 1,
            skipped: 0,
            failed: 1,
            items: Vec::new(),
        };
        assert!(import_has_committed_success(&partial));
        let failed = ImportApplyReport {
            imported: 0,
            updated: 0,
            skipped: 1,
            failed: 1,
            items: Vec::new(),
        };
        assert!(!import_has_committed_success(&failed));

        let notice = browser_omission_notice(1, 1).expect("notice");
        assert!(notice.contains("1 entry disappeared"), "{notice}");
        assert!(notice.contains("was omitted"), "{notice}");
        assert!(
            notice.contains("1 symbolic link was not followed"),
            "{notice}"
        );
        let notice = browser_omission_notice(2, 2).expect("notice");
        assert!(notice.contains("2 entries disappeared"), "{notice}");
        assert!(notice.contains("were omitted"), "{notice}");
        assert!(
            notice.contains("2 symbolic links were not followed"),
            "{notice}"
        );
        assert_eq!(browser_omission_notice(0, 0), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir = TempDir::new("gui-browser-omissions");
            let folder = dir.path().join("browse");
            std::fs::create_dir_all(&folder).expect("folder");
            let song = folder.join("song.wav");
            std::fs::write(&song, minimal_wav()).expect("song");
            symlink(&song, folder.join("linked.wav")).expect("symlink");
            let browser = new_browser(BrowserMode::Files, folder);
            assert_eq!(browser.entries.len(), 1);
            assert_eq!(browser.symlinks_skipped, 1);
            assert_eq!(browser.omitted, 0);
            assert!(
                browser_omission_notice(browser.omitted, browser.symlinks_skipped)
                    .is_some_and(|notice| notice.contains("symbolic link"))
            );
        }
    }

    #[test]
    fn retry_audio_message_never_claims_recovery_when_none_is_available() {
        let dir = TempDir::new("gui-audio-retry");
        let mut gui = gui(&dir);
        assert!(!gui.controller.can_retry_audio_output());
        send(&mut gui, Message::RetryAudioOutput);
        assert!(
            gui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("not in a retryable failed state")),
            "{:?}",
            gui.lifecycle_msg
        );
        assert!(!matches!(
            gui.controller.audio_status(),
            AudioStatus::Recovering(_)
        ));
        gui.controller.shutdown();
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

        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);
        assert_eq!(
            gui.controller.canonical_root(),
            Some(std::fs::canonicalize(&root).expect("canonical").as_path())
        );
        assert!(
            root.join("catalog.flux").exists(),
            "canonical catalogue exists"
        );
        assert!(
            root.join("library.flux").exists(),
            "canonical manifest exists"
        );
        assert!(!root.join(".flux").exists(), "no legacy portable store");
        assert_eq!(gui.active_library_label(), "library");
        assert!(gui.active_library_path().is_some());
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
        send(&mut gui, Message::DismissNotice);
        assert!(gui.controller.last_error().is_none());
        assert!(gui.lifecycle_msg.is_none());
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);
        gui.controller.shutdown();
    }

    #[test]
    fn cancelled_library_selection_leaves_the_active_instance_unchanged() {
        let dir = TempDir::new("gui-cancelled-switch");
        let mut gui = gui(&dir);
        let session = gui.controller.library_session();
        let mode = gui.controller.storage_mode();
        let root = gui.controller.canonical_root().map(Path::to_path_buf);
        send(
            &mut gui,
            Message::LibInputChanged(dir.path().join("not-opened").display().to_string()),
        );
        assert_eq!(gui.controller.library_session(), session);
        assert_eq!(gui.controller.storage_mode(), mode);
        assert_eq!(gui.controller.canonical_root().map(Path::to_path_buf), root);
        gui.controller.shutdown();
    }

    #[test]
    fn async_results_from_the_previous_library_instance_are_dropped() {
        let dir = TempDir::new("gui-stale-library-tasks");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("source/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        gui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("import");
        let id = gui.controller.tracks()[0].0;
        let old_session = gui.controller.library_session();
        let request = ImportRequest {
            sources: vec![source.clone()],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: root.clone(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: old_session,
        };
        let old_plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );

        // Reopening the same path changes the in-memory library instance even
        // though the path and entity IDs are unchanged.
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let current_session = gui.controller.library_session();
        assert_ne!(current_session, old_session);
        assert_eq!(gui.controller.tracks()[0].0, id);

        send(&mut gui, Message::ImportOpen);
        let _ = gui.update(Message::ImportPlanReady(old_session, Box::new(old_plan)));
        let _ = gui.update(Message::ImportPrepared(old_session, Box::default()));
        let manager = gui.import_manager.as_ref().expect("new manager");
        assert!(manager.plan.is_none());
        assert_eq!(manager.stage, ImportUiStage::Choose);
        gui.import_manager.as_mut().expect("manager").sources =
            vec![PathBuf::from("keep-current-selection")];
        let _ = gui.update(Message::ImportNativeFilesPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-selection")])),
        ));
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").sources,
            vec![PathBuf::from("keep-current-selection")]
        );
        gui.import_manager.as_mut().expect("manager").edit_artwork = "keep-track-art".to_owned();
        let _ = gui.update(Message::ImportArtworkPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-track-art.png")])),
        ));
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").edit_artwork,
            "keep-track-art"
        );
        gui.import_manager
            .as_mut()
            .expect("manager")
            .edit_album_artwork = "keep-album-art".to_owned();
        let _ = gui.update(Message::ImportAlbumArtworkPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-album-art.png")])),
        ));
        assert_eq!(
            gui.import_manager
                .as_ref()
                .expect("manager")
                .edit_album_artwork,
            "keep-album-art"
        );
        gui.import_manager
            .as_mut()
            .expect("manager")
            .bulk_album_artwork_path = "keep-bulk-album-art".to_owned();
        let _ = gui.update(Message::ImportBulkAlbumArtworkPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-bulk-album-art.png")])),
        ));
        assert_eq!(
            gui.import_manager
                .as_ref()
                .expect("manager")
                .bulk_album_artwork_path,
            "keep-bulk-album-art"
        );

        send(&mut gui, Message::EditTrack(id));
        let editor_before = gui
            .track_editor
            .as_ref()
            .expect("editor")
            .replacement_source
            .clone();
        let _ = gui.update(Message::EditorReplacementPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-replacement.wav")])),
        ));
        assert_eq!(
            gui.track_editor
                .as_ref()
                .expect("editor")
                .replacement_source,
            editor_before
        );
        gui.track_editor.as_mut().expect("editor").artwork_input = "keep-track-override".to_owned();
        let _ = gui.update(Message::TrackArtworkPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-track-override.png")])),
        ));
        assert_eq!(
            gui.track_editor.as_ref().expect("editor").artwork_input,
            "keep-track-override"
        );

        gui.artist_image_input = "keep-artist-image".to_owned();
        let _ = gui.update(Message::ArtistImagePicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-artist-image.png")])),
        ));
        assert_eq!(gui.artist_image_input, "keep-artist-image");

        gui.album_cover_input = "keep-cover".to_owned();
        let _ = gui.update(Message::AlbumCoverPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-cover.png")])),
        ));
        assert_eq!(gui.album_cover_input, "keep-cover");

        send(&mut gui, Message::LyricsSelect(id));
        let _ = gui.update(Message::LyricsImportPicked(
            old_session,
            Ok(Some(vec![PathBuf::from("stale-lyrics.lrc")])),
        ));
        assert!(gui.lyrics_import.is_none());

        let key = ArtworkKey {
            owner: ArtworkOwner::Album(AlbumId::new(1)),
            path: PathBuf::from("stale-cover.png"),
            modified_ns: 0,
            len: 0,
        };
        gui.artwork_loading.insert(key.clone());
        let _ = gui.update(Message::ArtworkLoaded(
            old_session,
            key.clone(),
            Err("stale artwork".to_owned()),
        ));
        assert!(
            gui.artwork_loading.contains(&key),
            "stale artwork result was processed"
        );
        let _ = gui.update(Message::ArtworkLoaded(
            current_session,
            key.clone(),
            Err("current artwork".to_owned()),
        ));
        assert!(!gui.artwork_loading.contains(&key));

        assert_eq!(gui.controller.tracks().len(), 1);
        assert_eq!(gui.controller.tracks()[0].0, id);
        gui.controller.shutdown();
    }

    #[test]
    fn switch_clears_library_owned_ui_state_and_preserves_global_preferences() {
        let dir = TempDir::new("gui-switch-state-isolation");
        let root_a = dir.path().join("library-a");
        let root_b = dir.path().join("library-b");
        std::fs::create_dir_all(&root_a).expect("root a");
        std::fs::create_dir_all(&root_b).expect("root b");
        flux_app::create_canonical_library(&root_a).expect("canonical a");
        flux_app::create_canonical_library(&root_b).expect("canonical b");
        let source_a = dir.path().join("source/a.wav");
        let source_b = dir.path().join("source/b.wav");
        std::fs::create_dir_all(source_a.parent().expect("parent")).expect("dirs");
        std::fs::write(&source_a, minimal_wav()).expect("a");
        let mut second = minimal_wav();
        second.push(1);
        std::fs::write(&source_b, second).expect("b");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root_a.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        gui.controller
            .import_files(std::slice::from_ref(&source_a), false)
            .expect("import a");
        let old_id = gui.controller.tracks()[0].0;
        let old_playlist = gui
            .controller
            .set_playlist(None, "Old library only", vec![old_id])
            .expect("playlist");
        gui.selected_tracks = vec![old_id];
        gui.focused_track = Some(old_id);
        gui.selected_group = Some("stale-album-selection".to_owned());
        gui.selected_playlist = Some(old_playlist);
        gui.collection = Some(SmartCollection::Favorites);
        gui.filter_favorites = true;
        gui.sort_tracks = TrackSort::Title;
        gui.controller.set_volume(0.5);
        gui.search = "old library".to_owned();
        let _ = gui.search_snapshot();
        let dashboard = gui.dashboard_summary();
        *gui.dashboard_cache.borrow_mut() = Some(dashboard);
        gui.batch_review = Some(BatchReview {
            playlist_id: Some(old_playlist),
            playlist_label: "Old".to_owned(),
            expected_playlist_track_ids: None,
            new_playlist_name: String::new(),
            track_ids: vec![old_id],
            error: None,
        });
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::EditTrack(old_id));
        send(&mut gui, Message::LyricsSelect(old_id));
        gui.artwork_loading.insert(ArtworkKey {
            owner: ArtworkOwner::Album(AlbumId::new(1)),
            path: PathBuf::from("old-cover.png"),
            modified_ns: 0,
            len: 0,
        });
        gui.window_width = 920.0;
        gui.queue_overlay_open = true;
        gui.album_cover_picked = Some(PathBuf::from("stale-cover-picked.png"));
        let old_session = gui.controller.library_session();

        send(
            &mut gui,
            Message::LibInputChanged(root_b.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        assert_ne!(gui.controller.library_session(), old_session);
        assert_eq!(gui.controller.canonical_root(), Some(root_b.as_path()));
        assert!(gui.controller.tracks().is_empty());
        assert_eq!(gui.controller.current_queue_entry_id(), None);
        assert_eq!(gui.controller.player_state(), PlaybackState::Stopped);
        assert!(gui.controller.playlist(old_playlist).is_none());

        assert!(gui.selected_tracks.is_empty());
        assert_eq!(gui.focused_track, None);
        assert_eq!(gui.selected_group, None);
        assert_eq!(gui.selected_playlist, None);
        assert_eq!(gui.collection, None);
        assert!(gui.search.is_empty());
        assert_eq!(gui.search_cache.borrow().generation, u64::MAX);
        assert!(gui.track_editor.is_none());
        assert!(gui.selected_lyrics.is_none());
        assert!(gui.lyrics_import.is_none());
        assert!(gui.lyrics_editor.is_none());
        assert!(gui.import_manager.is_none());
        assert!(gui.batch_review.is_none());
        assert!(gui.artwork.is_empty());
        assert!(gui.artwork_loading.is_empty());
        assert!(gui.dashboard_cache.borrow().is_none());
        assert!(gui.lyrics_cache.borrow().is_empty());
        assert!(gui.album_cover_picked.is_none());
        assert!(!gui.queue_overlay_open);
        assert_eq!(gui.queue_placement(), QueuePlacement::Hidden);

        // Purely global preferences survive the switch.
        assert!(gui.filter_favorites);
        assert_eq!(gui.sort_tracks, TrackSort::Title);
        assert!((gui.controller.volume() - 0.5).abs() < 1e-6);

        // Both libraries use ID 1, but the active identity is the new one.
        gui.controller
            .import_files(std::slice::from_ref(&source_b), false)
            .expect("import b");
        let new_id = gui.controller.tracks()[0].0;
        assert_eq!(new_id, old_id, "test requires overlapping TrackIds");
        assert_eq!(
            gui.controller.track(new_id).map(|track| track.title()),
            Some("b")
        );
        assert_ne!(gui.controller.library_session(), old_session);
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
        let destination = dir.path().join("canonical-result");
        send(
            &mut gui,
            Message::LibInputChanged(destination.display().to_string()),
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
        assert!(!destination.exists(), "cancel wrote nothing");
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        send(&mut gui, Message::PlanMigrationPressed);
        send(&mut gui, Message::ExecuteMigrationPressed);
        assert!(gui.pending_plan.is_none());
        assert_eq!(gui.migrated_dest.as_deref(), Some(destination.as_path()));
        assert!(destination.join("catalog.flux").exists());
        assert!(destination.join("library.flux").exists());
        assert_eq!(gui.controller.storage_mode(), StorageMode::Legacy);

        send(&mut gui, Message::OpenMigratedPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);
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
        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);
        send(&mut gui, Message::ReconcilePressed);
        assert_eq!(
            gui.controller.storage_mode(),
            StorageMode::Canonical,
            "refreshing never switches libraries"
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
        let mut second = minimal_wav();
        second.push(1);
        std::fs::write(&two, second).expect("two");
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
    fn drop_and_flux_browser_messages_prepare_a_scan() {
        let dir = TempDir::new("gui-import-entry");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("library root");
        flux_app::create_canonical_library(&root).expect("create canonical library");
        let source = root.join("drop.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);

        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportFileHovered);
        assert!(gui.import_manager.as_ref().expect("manager").drop_hover);
        send(&mut gui, Message::ImportFileHoverLeft);
        assert!(!gui.import_manager.as_ref().expect("manager").drop_hover);
        send(&mut gui, Message::ImportDropPath(source.clone()));
        assert_eq!(
            gui.import_manager
                .as_ref()
                .expect("manager")
                .pending_drops
                .len(),
            1
        );
        gui.import_manager.as_mut().expect("manager").drop_deadline =
            Some(Instant::now() - Duration::from_millis(300));
        send(&mut gui, Message::Tick);
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").stage,
            ImportUiStage::Scanning
        );
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").sources,
            vec![source.clone()]
        );

        send(&mut gui, Message::ImportClose);
        assert!(gui.import_manager.is_none());
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportBrowserOpenFiles);
        assert!(
            gui.import_manager
                .as_ref()
                .and_then(|manager| manager.browser.as_ref())
                .is_some()
        );
        send(&mut gui, Message::ImportBrowserToggle(source.clone()));
        assert!(
            gui.import_manager
                .as_ref()
                .and_then(|manager| manager.browser.as_ref())
                .is_some_and(|browser| browser.selected.contains(&source))
        );
        send(&mut gui, Message::ImportBrowserConfirm);
        let manager = gui.import_manager.as_ref().expect("manager");
        assert_eq!(manager.stage, ImportUiStage::Scanning);
        assert_eq!(manager.sources, vec![source]);
        gui.controller.shutdown();
    }

    #[test]
    fn import_manager_reviews_and_commits_through_gui_messages() {
        let dir = TempDir::new("gui-import-manager");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("library root");
        flux_app::create_canonical_library(&root).expect("create canonical library");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("import.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let source_bytes = std::fs::read(&source).expect("source bytes");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);

        let request = ImportRequest {
            sources: vec![source.clone()],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: gui.controller.canonical_root().expect("root").to_path_buf(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = prepare_import_plan(request, progress, cancel);
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(0, Box::new(plan)));
        let manager = gui.import_manager.as_ref().expect("manager open");
        assert_eq!(manager.stage, ImportUiStage::Review);
        assert_eq!(manager.selected, Some(0));

        send(
            &mut gui,
            Message::ImportTitleChanged("GUI imported title".to_owned()),
        );
        send(
            &mut gui,
            Message::ImportArtistChanged("GUI import artist".to_owned()),
        );
        send(
            &mut gui,
            Message::ImportAlbumChanged("GUI import album".to_owned()),
        );
        send(
            &mut gui,
            Message::ImportCreditNameChanged("Featured Artist".to_owned()),
        );
        send(&mut gui, Message::ImportCreditAddNamed);
        let track_art = dir.path().join("import-track.png");
        let album_art = dir.path().join("import-album.png");
        write_test_png(&track_art, 48, 48);
        write_test_png(&album_art, 64, 64);
        send(
            &mut gui,
            Message::ImportArtworkPicked(0, Ok(Some(vec![track_art.clone()]))),
        );
        send(&mut gui, Message::ImportArtworkApply);
        send(
            &mut gui,
            Message::ImportAlbumArtworkPicked(0, Ok(Some(vec![album_art.clone()]))),
        );
        send(&mut gui, Message::ImportAlbumArtworkApply);
        let lyrics = dir.path().join("import.lrc");
        std::fs::write(&lyrics, "[00:01.00]GUI imported lyrics\n").expect("lyrics");
        send(
            &mut gui,
            Message::ImportLyricsPathChanged(lyrics.display().to_string()),
        );
        send(&mut gui, Message::ImportLyricsAttach);
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        let candidate = plan.candidates.first().expect("candidate");
        assert_eq!(candidate.artwork_path(), Some(track_art.as_path()));
        assert_eq!(candidate.album_artwork_path(), Some(album_art.as_path()));
        assert_eq!(
            candidate.lyrics_text(),
            Some("[00:01.00]GUI imported lyrics\n")
        );
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let prepared = prepare_import_files(&plan, progress, cancel);
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        let manager = gui.import_manager.as_ref().expect("manager");
        assert_eq!(manager.stage, ImportUiStage::Completed);
        let track = gui.controller.tracks()[0].1.clone();
        assert_eq!(track.title(), "GUI imported title");
        assert_eq!(track.artist(), Some("GUI import artist"));
        assert_eq!(track.album(), Some("GUI import album"));
        let id = gui.controller.tracks()[0].0;
        assert_eq!(gui.controller.track_credits(id).len(), 2);
        assert_eq!(
            gui.controller
                .track_credits(id)
                .first()
                .and_then(|artist| gui.controller.artist(*artist))
                .map(|artist| artist.name().to_owned()),
            Some("GUI import artist".to_owned())
        );
        let imported_art = gui
            .controller
            .track_artwork_path(id)
            .expect("imported track artwork");
        assert!(imported_art.is_file());
        let imported_album = gui.controller.track_album_id(id).expect("album");
        assert!(
            gui.controller
                .album_cover_path(imported_album)
                .is_some_and(|path| path.is_file())
        );
        assert_eq!(
            gui.controller.track_lyrics(id).as_deref(),
            Some("[00:01.00]GUI imported lyrics\n")
        );
        assert_eq!(gui.controller.queue_len(), 1);
        assert_eq!(std::fs::read(&source).expect("source"), source_bytes);
        assert_ne!(track.source().path(), source);
        gui.controller.shutdown();
    }

    #[test]
    fn import_review_clears_album_and_publishes_a_standalone_track() {
        let dir = TempDir::new("gui-import-standalone");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create");
        let source = dir.path().join("Incoming/Standalone.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source dir");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let request = ImportRequest {
            sources: vec![source],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: root,
            existing_tracks: Vec::new(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(0, Box::new(plan)));
        send(&mut gui, Message::ImportAlbumChanged(String::new()));
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        assert_eq!(plan.candidates[0].album(), None);
        assert!(plan.can_import(), "{:?}", plan.blocking_reason());
        let prepared = prepare_import_files(
            &plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        let id = gui.controller.tracks()[0].0;
        assert_eq!(gui.controller.track_album_id(id), None);
        assert_eq!(gui.controller.albums().len(), 0);
        assert!(
            gui.controller
                .track(id)
                .is_some_and(|track| track.source().path().is_file())
        );
        gui.view = LibraryView::Albums;
        gui.selected_group = None;
        assert!(
            gui.groups_for(LibraryView::Albums)
                .iter()
                .any(|(key, count)| key.contains("Standalone tracks") && *count == 1),
            "standalone Tracks have a visible Albums-view group"
        );
        gui.controller.shutdown();
    }

    #[test]
    fn import_review_bulk_applies_artist_album_and_artwork_to_checked_candidates() {
        let dir = TempDir::new("gui-import-bulk");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create");
        let source_dir = dir.path().join("Incoming");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let one = source_dir.join("one.wav");
        let mut two_bytes = minimal_wav();
        two_bytes.push(1);
        let two = source_dir.join("two.wav");
        std::fs::write(&one, minimal_wav()).expect("one");
        std::fs::write(&two, two_bytes).expect("two");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let artist = gui
            .controller
            .create_artist("Bulk GUI Artist")
            .expect("artist");
        let request = ImportRequest {
            sources: vec![one, two],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: root.clone(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(0, Box::new(plan)));
        send(&mut gui, Message::ImportToggleChecked(0));
        send(&mut gui, Message::ImportToggleChecked(1));
        send(
            &mut gui,
            Message::ImportBulkArtistPicked(Some(ArtistChoice {
                id: Some(artist),
                label: "Bulk GUI Artist".to_owned(),
            })),
        );
        send(&mut gui, Message::ImportBulkApplyArtist);
        send(
            &mut gui,
            Message::ImportBulkAlbumChanged("Bulk GUI Album".to_owned()),
        );
        send(&mut gui, Message::ImportBulkApplyAlbum);
        let album_art = dir.path().join("bulk-gui.png");
        write_test_png(&album_art, 64, 64);
        send(
            &mut gui,
            Message::ImportBulkAlbumArtworkPicked(0, Ok(Some(vec![album_art]))),
        );
        send(&mut gui, Message::ImportBulkApplyAlbumArtwork);
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        assert_eq!(
            gui.import_manager
                .as_ref()
                .map(|manager| manager.checked.len()),
            Some(2)
        );
        assert_eq!(plan.candidates[0].artist_id(), Some(artist));
        assert_eq!(plan.candidates[1].artist_id(), Some(artist));
        assert_eq!(plan.candidates[0].album(), Some("Bulk GUI Album"));
        assert_eq!(plan.candidates[1].album(), Some("Bulk GUI Album"));
        assert!(
            plan.candidates
                .iter()
                .all(|candidate| candidate.album_artwork_path().is_some())
        );
        let prepared = prepare_import_files(
            &plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        let albums = gui.controller.albums();
        assert_eq!(albums.len(), 1);
        assert_eq!(gui.controller.album_tracks(albums[0].id).len(), 2);
        assert!(
            gui.controller
                .tracks()
                .iter()
                .all(|(id, _)| { gui.controller.track_artist_id(*id) == Some(artist) })
        );
        gui.controller.shutdown();
    }

    #[test]
    fn import_review_selects_existing_artist_and_album_without_duplicates() {
        let dir = TempDir::new("gui-import-existing-entities");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create");
        let source = dir.path().join("Incoming/New Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source dir");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let artist = gui
            .controller
            .create_artist("Existing Artist")
            .expect("artist");
        let album = gui
            .controller
            .create_album(artist, "Existing Album", ReleaseType::Album)
            .expect("album");
        let request = ImportRequest {
            sources: vec![source],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: root.clone(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(0, Box::new(plan)));
        send(
            &mut gui,
            Message::ImportArtistPicked(Some(ArtistChoice {
                id: Some(artist),
                label: "Existing Artist".to_owned(),
            })),
        );
        send(
            &mut gui,
            Message::ImportAlbumPicked(Some(AlbumChoice {
                id: album,
                label: "Existing Album".to_owned(),
            })),
        );
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        let candidate = plan.candidates.first().expect("candidate");
        assert_eq!(candidate.artist_id(), Some(artist));
        assert_eq!(candidate.album(), Some("Existing Album"));
        let prepared = prepare_import_files(
            &plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        let imported = gui.controller.tracks()[0].0;
        assert_eq!(gui.controller.track_artist_id(imported), Some(artist));
        assert_eq!(gui.controller.track_album_id(imported), Some(album));
        assert_eq!(gui.controller.albums().len(), 1, "no duplicate Album");
        assert_eq!(gui.controller.artists().len(), 1, "no duplicate Artist");
        assert_eq!(gui.controller.album_tracks(album), vec![imported]);
        gui.controller.shutdown();
    }

    #[test]
    fn duplicate_review_updates_metadata_in_place_and_keeps_the_audio() {
        let dir = TempDir::new("gui-duplicate-update");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("library root");
        flux_app::create_canonical_library(&root).expect("create canonical library");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        gui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("base import");
        let (id, track) = gui.controller.tracks()[0].clone();
        let managed = track.source().path().to_path_buf();
        let managed_before = std::fs::read(&managed).expect("managed");
        std::fs::copy(&managed, &source).expect("duplicate source");

        let request = ImportRequest {
            sources: vec![source],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: gui.controller.canonical_root().expect("root").to_path_buf(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: gui
                .controller
                .tracks()
                .iter()
                .filter_map(|(track_id, _)| {
                    gui.controller
                        .track_artist_id(*track_id)
                        .map(|artist| (*track_id, artist))
                })
                .collect(),
            existing_credits: gui
                .controller
                .tracks()
                .iter()
                .map(|(track_id, _)| (*track_id, gui.controller.track_credits(*track_id)))
                .collect(),
            existing_album_ids: gui
                .controller
                .tracks()
                .iter()
                .filter_map(|(track_id, _)| {
                    gui.controller
                        .track_album_id(*track_id)
                        .map(|album| (*track_id, album))
                })
                .collect(),
            target_album: None,
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(0, Box::new(plan)));
        assert!(matches!(
            gui.import_manager
                .as_ref()
                .and_then(|manager| manager.plan.as_ref())
                .and_then(|plan| plan.candidates.first())
                .map(flux_app::ImportCandidate::status),
            Some(CandidateStatus::SameContent {
                track_id: Some(existing),
                ..
            }) if *existing == id
        ));
        send(
            &mut gui,
            Message::ImportDecisionChanged(0, ImportDecision::UpdateExisting),
        );
        send(
            &mut gui,
            Message::ImportTitleChanged("GUI duplicate corrected".to_owned()),
        );
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        let prepared = prepare_import_files(
            &plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        let manager = gui.import_manager.as_ref().expect("manager");
        assert_eq!(manager.stage, ImportUiStage::Completed);
        assert_eq!(
            manager.report.as_ref().map(|report| report.updated),
            Some(1)
        );
        assert_eq!(gui.controller.tracks().len(), 1);
        assert_eq!(gui.controller.tracks()[0].0, id);
        assert_eq!(
            gui.controller.track(id).map(|track| track.title()),
            Some("GUI duplicate corrected")
        );
        assert_eq!(
            std::fs::read(&managed).expect("managed after"),
            managed_before
        );
        gui.controller.shutdown();
    }

    #[test]
    fn import_edit_and_artist_messages_persist_through_the_gui_state() {
        let dir = TempDir::new("gui-manage-flow");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("library root");
        flux_app::create_canonical_library(&root).expect("create canonical library");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut gui = gui(&dir);

        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);
        gui.controller
            .import_files(std::slice::from_ref(&source), true)
            .expect("quick fixture import");
        assert_eq!(gui.controller.tracks().len(), 1);
        let id = gui.controller.tracks()[0].0;

        send(&mut gui, Message::EditTrack(id));
        assert!(gui.track_editor.is_some());
        send(
            &mut gui,
            Message::EditorTitleChanged("Edited title".to_owned()),
        );
        send(
            &mut gui,
            Message::EditorAlbumChanged("Edited album".to_owned()),
        );
        send(&mut gui, Message::EditorSave);
        assert!(gui.track_editor.is_none());
        let track = gui.controller.track(id).expect("edited track");
        assert_eq!(track.title(), "Edited title");
        assert_eq!(track.artist(), Some("Artist"));
        assert_eq!(track.album(), Some("Edited album"));
        let artist_id = gui.controller.track_artist_id(id).expect("artist");

        send(
            &mut gui,
            Message::ArtistSelected(Some(ArtistChoice {
                id: Some(artist_id),
                label: "GUI Artist".to_owned(),
            })),
        );
        send(
            &mut gui,
            Message::ArtistRenameNameChanged("Renamed Artist".to_owned()),
        );
        send(&mut gui, Message::ArtistRenamePressed);
        assert_eq!(
            gui.controller.track(id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(gui.controller.track_artist_id(id), Some(artist_id));
        gui.controller.shutdown();
    }

    #[test]
    fn seek_preview_clamps_and_clears_on_commit_or_track_change() {
        let dir = TempDir::new("seek-preview");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("music dir");
        let one = root.join("one.wav");
        let two = root.join("two.wav");
        std::fs::write(&one, minimal_wav()).expect("one");
        std::fs::write(&two, minimal_wav()).expect("two");
        std::fs::write(
            dir.path().join("library-index.json"),
            format!(
                r#"{{"version":1,"next_id":3,"tracks":[{{"id":1,"title":"One","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":1000,"path":{{"utf8":"{}"}}}},{{"id":2,"title":"Two","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":1000,"path":{{"utf8":"{}"}}}}]}}"#,
                one.display(),
                two.display()
            ),
        )
        .expect("index");
        let mut gui = gui(&dir);
        assert_eq!(
            gui.controller.current_duration(),
            Some(Duration::from_secs(1))
        );

        send(&mut gui, Message::SeekPreview(0.5));
        assert_eq!(gui.seek_preview, Some(0.5));
        send(&mut gui, Message::SeekPreview(5.0));
        assert_eq!(gui.seek_preview, Some(1.0));
        send(&mut gui, Message::Tick);
        assert_eq!(
            gui.seek_preview,
            Some(1.0),
            "a repaint must not commit an in-progress slider drag"
        );
        send(&mut gui, Message::SeekCommit);
        assert!(gui.seek_preview.is_none());

        send(&mut gui, Message::SeekPreview(250.0));
        send(&mut gui, Message::Next);
        assert!(gui.seek_preview.is_none(), "track changes cancel a preview");
        gui.controller.shutdown();
    }

    #[test]
    fn scope_sampling_interpolates_between_real_samples() {
        let samples = [0.0, 0.5, 1.0];
        assert!((scope_sample(&samples, 0.0) - 0.0).abs() < f32::EPSILON);
        assert!((scope_sample(&samples, 1.0) - 1.0).abs() < f32::EPSILON);
        assert!((scope_sample(&samples, 0.25) - 0.25).abs() < f32::EPSILON);
        assert!((scope_sample(&samples, 0.75) - 0.75).abs() < f32::EPSILON);
        assert!((scope_sample(&samples, -1.0) - 0.0).abs() < f32::EPSILON);
        assert!((scope_sample(&samples, 2.0) - 1.0).abs() < f32::EPSILON);
        assert!((scope_sample(&[], 0.5) - 0.0).abs() < f32::EPSILON);
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

    #[test]
    fn gui_track_editor_edits_ordered_credits_without_changing_owner() {
        let dir = TempDir::new("gui-lifecycle-credits");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let a = gui.controller.create_artist("A").expect("A");
        let b = gui.controller.create_artist("B").expect("B");
        let c = gui.controller.create_artist("C").expect("C");
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("song");
        let id = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        let album = gui.controller.track_album_id(id);
        send(&mut gui, Message::EditTrack(id));
        send(
            &mut gui,
            Message::EditorCreditSelected(Some(ArtistChoice {
                id: Some(c),
                label: "C".to_owned(),
            })),
        );
        send(&mut gui, Message::EditorCreditAdd);
        send(&mut gui, Message::EditorCreditMove(2, -1));
        send(
            &mut gui,
            Message::EditorTitleChanged("Edited lifecycle".to_owned()),
        );
        send(&mut gui, Message::EditorSave);
        assert!(gui.track_editor.is_none());
        assert_eq!(gui.controller.track_credits(id), vec![a, c, b]);
        assert_eq!(gui.controller.track_artist_id(id), Some(a));
        assert_eq!(gui.controller.track_album_id(id), album);
        assert_eq!(
            gui.controller.track(id).map(|track| track.title()),
            Some("Edited lifecycle")
        );
        gui.controller.shutdown();
    }

    #[test]
    fn gui_reviews_and_confirms_managed_audio_replacement() {
        let dir = TempDir::new("gui-lifecycle-replace");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("song");
        gui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("import");
        let (id, track) = gui.controller.tracks()[0].clone();
        let old_audio = track.source().path().to_path_buf();
        let mut replacement_bytes = minimal_wav();
        replacement_bytes.push(9);
        let replacement = source_dir.join("replacement.wav");
        std::fs::write(&replacement, &replacement_bytes).expect("replacement");
        send(&mut gui, Message::EditTrack(id));
        send(
            &mut gui,
            Message::EditorReplacementChanged(replacement.display().to_string()),
        );
        send(&mut gui, Message::EditorPlanReplacement);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_plan.is_some())
        );
        send(&mut gui, Message::EditorConfirmReplacement);
        assert!(gui.track_editor.is_none());
        assert_eq!(gui.controller.tracks()[0].0, id);
        assert_eq!(
            std::fs::read(gui.controller.track(id).expect("track").source().path())
                .expect("new audio"),
            replacement_bytes
        );
        assert!(!old_audio.exists());
        assert_eq!(
            std::fs::read(&replacement).expect("source preserved"),
            replacement_bytes
        );
        gui.controller.shutdown();
    }

    #[test]
    fn gui_reviews_and_confirms_move_into_existing_album() {
        let dir = TempDir::new("gui-lifecycle-move");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("song");
        gui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("import");
        let id = gui.controller.tracks()[0].0;
        let source_album = gui.controller.track_album_id(id).expect("album");
        let owner = gui.controller.track_artist_id(id).expect("owner");
        let destination = gui
            .controller
            .create_album(owner, "Destination", flux_app::ReleaseType::Album)
            .expect("destination");
        send(&mut gui, Message::EditTrack(id));
        send(
            &mut gui,
            Message::EditorMoveDestination(Some(AlbumChoice {
                id: destination,
                label: format!("Destination · {destination}"),
            })),
        );
        send(&mut gui, Message::EditorMovePositionChanged("1".to_owned()));
        send(&mut gui, Message::EditorPlanMove);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.move_plan.is_some())
        );
        send(&mut gui, Message::EditorConfirmMove);
        assert!(gui.track_editor.is_none());
        assert_eq!(gui.controller.track_album_id(id), Some(destination));
        assert_eq!(gui.controller.album_tracks(destination), vec![id]);
        assert_eq!(
            gui.controller.album_tracks(source_album),
            Vec::<TrackId>::new()
        );
        assert!(gui.controller.album(source_album).is_some());
        gui.controller.shutdown();
    }

    #[test]
    fn gui_track_editor_transfers_owner_using_explicit_release_title() {
        let dir = TempDir::new("gui-transfer");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        assert_eq!(gui.controller.storage_mode(), StorageMode::Canonical);
        let a = gui.controller.create_artist("A").expect("A");
        let b = gui.controller.create_artist("B").expect("B");
        let c = gui.controller.create_artist("C").expect("C");
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("song");
        let id = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        send(&mut gui, Message::EditTrack(id));
        send(
            &mut gui,
            Message::EditorArtistChanged(Some(ArtistChoice {
                id: Some(c),
                label: "C".to_owned(),
            })),
        );
        send(
            &mut gui,
            Message::EditorAlbumChanged("Destination".to_owned()),
        );
        send(&mut gui, Message::EditorTransferOwner);
        assert!(
            gui.track_editor.is_some(),
            "failed transfer keeps editor open"
        );
        assert!(
            gui.track_editor
                .as_ref()
                .and_then(|e| e.error.as_ref())
                .is_some()
        );
        assert_eq!(gui.controller.track_artist_id(id), Some(a));
        send(
            &mut gui,
            Message::EditorArtistChanged(Some(ArtistChoice {
                id: Some(b),
                label: "B".to_owned(),
            })),
        );
        send(&mut gui, Message::EditorTransferOwner);
        assert!(gui.track_editor.is_none(), "transfer closes editor");
        assert_eq!(gui.controller.track_artist_id(id), Some(b));
        assert_eq!(
            gui.controller.track(id).and_then(Track::album),
            Some("Destination")
        );
        assert_eq!(gui.controller.track_credits(id), vec![a, b]);
        gui.controller.shutdown();
    }

    #[test]
    fn gui_artist_deletion_edits_decisions_blocks_unresolved_and_commits() {
        let dir = TempDir::new("gui-artist-delete");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let a = gui.controller.create_artist("A").expect("A");
        let b = gui.controller.create_artist("B").expect("B");
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let collab_source = source_dir.join("collab.wav");
        let solo_source = source_dir.join("solo.wav");
        let guest_source = source_dir.join("guest.wav");
        for path in [&collab_source, &solo_source, &guest_source] {
            std::fs::write(path, minimal_wav()).expect("wav");
        }
        let collab = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: collab_source,
                title: "Collab".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("A Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("collab");
        let solo = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: solo_source,
                title: "Solo".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("A Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("solo");
        let guest = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: guest_source,
                title: "Guest".to_owned(),
                artist_id: Some(b),
                artist_name: "B".to_owned(),
                credited_artist_ids: vec![b, a],
                album_title: Some("B Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("guest");
        let destination = gui.controller.track_album_id(guest).expect("album");
        send(
            &mut gui,
            Message::ArtistSelected(Some(ArtistChoice {
                id: Some(a),
                label: "A".to_owned(),
            })),
        );
        send(&mut gui, Message::ArtistDeleteOpen);
        assert!(gui.deletion_manager.is_some());
        send(&mut gui, Message::ArtistDeletionCancel);
        assert!(gui.deletion_manager.is_none(), "cancel changes nothing");
        assert!(gui.controller.artist(a).is_some());
        send(&mut gui, Message::ArtistDeleteOpen);
        send(&mut gui, Message::ArtistDeletionExecute);
        assert_eq!(
            gui.deletion_manager.as_ref().expect("manager").stage,
            DeletionUiStage::Review,
            "unresolved plan cannot execute"
        );
        assert!(gui.controller.artist(a).is_some());
        send(&mut gui, Message::ArtistDeletionSelect(collab));
        send(
            &mut gui,
            Message::ArtistDeletionDestinationArtist(Some(ArtistChoice {
                id: Some(b),
                label: "B".to_owned(),
            })),
        );
        send(
            &mut gui,
            Message::ArtistDeletionDestinationAlbum(Some(AlbumChoice {
                id: destination,
                label: "B Album".to_owned(),
            })),
        );
        send(&mut gui, Message::ArtistDeletionSetTransfer);
        send(&mut gui, Message::ArtistDeletionSelect(solo));
        send(&mut gui, Message::ArtistDeletionSetDelete);
        send(&mut gui, Message::ArtistDeletionSelect(guest));
        send(&mut gui, Message::ArtistDeletionSetRemoveCredit);
        let plan = gui.deletion_manager.as_ref().expect("manager").plan.clone();
        assert!(plan.is_ready(), "{:?}", plan.issues);
        send(&mut gui, Message::ArtistDeletionExecute);
        send(&mut gui, Message::ArtistDeletionCommit);
        assert_eq!(
            gui.deletion_manager.as_ref().expect("manager").stage,
            DeletionUiStage::Completed
        );
        assert!(gui.controller.artist(a).is_none());
        assert!(gui.controller.track(solo).is_none());
        assert_eq!(gui.controller.track_artist_id(collab), Some(b));
        assert_eq!(gui.controller.track_credits(collab), vec![b]);
        gui.controller.shutdown();
    }

    #[test]
    fn gui_album_transfer_reviews_credit_block_cancel_and_commit() {
        let dir = TempDir::new("gui-album-transfer");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let a = gui.controller.create_artist("A").expect("A");
        let b = gui.controller.create_artist("B").expect("B");
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let one = source_dir.join("one.wav");
        let two = source_dir.join("two.wav");
        std::fs::write(&one, minimal_wav()).expect("one");
        std::fs::write(&two, minimal_wav()).expect("two");
        let first = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: one,
                title: "One".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("first");
        let second = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: two,
                title: "Two".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("Album".to_owned()),
                release_type: flux_app::ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("second");
        let album = gui.controller.track_album_id(first).expect("album");
        gui.view = LibraryView::Albums;
        gui.selected_group = Some(format!("Album\u{1e}{}", album.get()));
        gui.controller.enqueue_track(first).expect("queue");
        gui.controller.play_track(first);
        let current = gui.controller.current_queue_entry_id().expect("current");

        send(&mut gui, Message::AlbumTransferOpen);
        assert!(gui.album_transfer.is_some());
        send(&mut gui, Message::AlbumTransferCancel);
        assert!(gui.album_transfer.is_none(), "cancel writes nothing");
        assert_eq!(gui.controller.album_owner(album), Some(a));

        send(&mut gui, Message::AlbumTransferOpen);
        send(&mut gui, Message::AlbumTransferExecute);
        assert_eq!(
            gui.album_transfer.as_ref().expect("manager").stage,
            AlbumTransferUiStage::Review,
            "unapproved credit blocks"
        );
        send(&mut gui, Message::AlbumTransferAllowCredit(second, true));
        let plan = gui.album_transfer.as_ref().expect("manager").plan.clone();
        assert!(plan.is_ready(), "{:?}", plan.issues);
        send(&mut gui, Message::AlbumTransferExecute);
        send(&mut gui, Message::AlbumTransferCommit);
        assert_eq!(
            gui.album_transfer.as_ref().expect("manager").stage,
            AlbumTransferUiStage::Completed
        );
        assert_eq!(gui.controller.album_owner(album), Some(b));
        assert_eq!(gui.controller.track_album_id(first), Some(album));
        assert_eq!(gui.controller.track_album_id(second), Some(album));
        assert_eq!(gui.controller.track_credits(second), vec![a, b]);
        assert_eq!(gui.controller.current_queue_entry_id(), Some(current));
        gui.controller.shutdown();
    }

    #[test]
    fn gui_artist_detail_and_track_artwork_workflows_are_end_to_end() {
        let dir = TempDir::new("gui-artist-detail");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Entity Song",
            "Entity Artist",
            "Entity Album",
            ReleaseType::Album,
            1,
        );
        let artist = gui.controller.track_artist_id(id).expect("artist");
        let album = gui.controller.track_album_id(id).expect("album");
        send(&mut gui, Message::SearchNavigateArtist(artist));
        assert_eq!(gui.view, LibraryView::Artists);
        assert_eq!(gui.selected_artist_id(), Some(artist));

        send(
            &mut gui,
            Message::ArtistDetailNameChanged("Renamed Entity Artist".to_owned()),
        );
        send(
            &mut gui,
            Message::ArtistDescriptionChanged("A user-authored description.".to_owned()),
        );
        send(&mut gui, Message::ArtistMetadataSave);
        assert_eq!(
            gui.controller
                .artist(artist)
                .map(|artist| artist.name().to_owned()),
            Some("Renamed Entity Artist".to_owned())
        );
        assert_eq!(
            gui.controller.artist_description(artist),
            Some("A user-authored description.")
        );
        assert_eq!(
            gui.controller.track(id).and_then(Track::artist),
            Some("Renamed Entity Artist")
        );
        assert!(
            gui.selected_group
                .as_deref()
                .is_some_and(|key| key.contains("Renamed Entity Artist")),
            "artist detail selection follows the renamed display identity"
        );

        let artist_image = dir.path().join("artist-image.png");
        write_test_png(&artist_image, 64, 64);
        send(
            &mut gui,
            Message::ArtistImagePicked(0, Ok(Some(vec![artist_image]))),
        );
        send(&mut gui, Message::ArtistImageApply);
        assert!(
            gui.controller
                .artist_image_path(artist)
                .is_some_and(|path| path.is_file())
        );
        send(&mut gui, Message::ArtistImageRemove);
        assert!(gui.controller.artist_image_path(artist).is_some());
        send(&mut gui, Message::ArtistImageRemove);
        assert!(gui.controller.artist_image_path(artist).is_none());

        send(&mut gui, Message::OpenAlbum(album));
        assert_eq!(gui.view, LibraryView::Albums);
        assert_eq!(gui.selected_album_id(), Some(album));
        send(&mut gui, Message::OpenArtist(artist));
        assert_eq!(gui.view, LibraryView::Artists);
        assert_eq!(gui.selected_artist_id(), Some(artist));

        send(&mut gui, Message::EditTrack(id));
        let invalid_art = dir.path().join("invalid-art.png");
        std::fs::write(&invalid_art, b"not an image").expect("invalid art");
        send(
            &mut gui,
            Message::TrackArtworkPicked(0, Ok(Some(vec![invalid_art]))),
        );
        send(&mut gui, Message::TrackArtworkApply);
        assert!(
            gui.controller.track_artwork_path(id).is_none(),
            "invalid Track artwork must be rejected without changing the library"
        );
        let track_image = dir.path().join("track-image.png");
        write_test_png(&track_image, 48, 48);
        send(
            &mut gui,
            Message::TrackArtworkPicked(0, Ok(Some(vec![track_image]))),
        );
        send(&mut gui, Message::TrackArtworkApply);
        let managed_track_art = gui
            .controller
            .track_artwork_path(id)
            .expect("track artwork override");
        assert!(managed_track_art.is_file());
        let expected_art_name = format!("track_{}.png", id.get());
        assert_eq!(
            managed_track_art.file_name().and_then(|name| name.to_str()),
            Some(expected_art_name.as_str())
        );
        send(&mut gui, Message::TrackArtworkRemove);
        assert!(gui.controller.track_artwork_path(id).is_some());
        send(&mut gui, Message::TrackArtworkRemove);
        assert!(gui.controller.track_artwork_path(id).is_none());
        assert!(
            gui.controller.album_cover_path(album).is_none(),
            "Track override removal does not fabricate Album artwork"
        );

        send(&mut gui, Message::EditTrack(id));
        send(&mut gui, Message::EditorPlanDetach);
        assert!(
            gui.track_editor
                .as_ref()
                .and_then(|editor| editor.move_plan.as_ref())
                .is_some_and(|plan| plan.to_album_id.is_none())
        );
        send(&mut gui, Message::EditorConfirmMove);
        assert_eq!(gui.controller.track_album_id(id), None);
        assert!(
            gui.controller
                .album(album)
                .is_some_and(|album| album.track_order.is_empty()),
            "empty Album remains a first-class entity"
        );

        send(&mut gui, Message::EditTrack(id));
        send(
            &mut gui,
            Message::EditorMoveDestination(Some(AlbumChoice {
                id: album,
                label: "Entity Album".to_owned(),
            })),
        );
        send(&mut gui, Message::EditorMovePositionChanged("1".to_owned()));
        send(&mut gui, Message::EditorPlanMove);
        send(&mut gui, Message::EditorConfirmMove);
        assert_eq!(gui.controller.track_album_id(id), Some(album));
        assert_eq!(gui.controller.album_tracks(album), vec![id]);
        gui.controller.shutdown();
    }

    #[test]
    fn gui_track_editor_reviews_and_deletes_one_track() {
        let dir = TempDir::new("gui-delete-track");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Delete Me",
            "Delete Artist",
            "Delete Album",
            ReleaseType::Album,
            1,
        );
        let album = gui.controller.track_album_id(id).expect("album");
        send(&mut gui, Message::EditTrack(id));
        gui.selected_tracks = vec![id];
        gui.focused_track = Some(id);
        send(&mut gui, Message::EditorPlanDeletion);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.deletion_plan.is_some())
        );
        *gui.dashboard_cache.borrow_mut() = Some(DashboardSummary::default());
        send(&mut gui, Message::EditorConfirmDeletion);
        assert!(gui.controller.track(id).is_none());
        assert!(gui.controller.album(album).is_some());
        assert!(gui.track_editor.is_none());
        assert!(gui.dashboard_cache.borrow().is_none());
        assert!(gui.selected_tracks.is_empty());
        assert_eq!(gui.focused_track, None);
        assert!(
            gui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("Deleted track"))
        );
        gui.controller.shutdown();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_picker_paths_preserve_non_utf8_names_for_replacement_and_artwork() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = TempDir::new("gui-native-non-utf8");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Native Path",
            "Native Artist",
            "Native Album",
            ReleaseType::Album,
            1,
        );
        let album = gui.controller.track_album_id(id).expect("album");

        let mut replacement_name = b"replacement-".to_vec();
        replacement_name.push(0xff);
        replacement_name.extend_from_slice(b".wav");
        let replacement = dir
            .path()
            .join(OsString::from_vec(replacement_name.clone()));
        let mut replacement_bytes = minimal_wav();
        replacement_bytes.push(9);
        std::fs::write(&replacement, &replacement_bytes).expect("replacement");
        send(&mut gui, Message::EditTrack(id));
        if let Some(editor) = gui.track_editor.as_mut() {
            editor.replacement_picked = Some(PathBuf::from("stale-picked-path"));
        }
        send(
            &mut gui,
            Message::EditorReplacementChanged("typed path".to_owned()),
        );
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_picked.is_none())
        );
        send(
            &mut gui,
            Message::EditorReplacementPicked(0, Ok(Some(vec![replacement]))),
        );
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_picked.is_some())
        );
        send(&mut gui, Message::EditorPlanReplacement);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_plan.is_some()),
            "the hidden non-UTF-8 path must survive review"
        );
        send(&mut gui, Message::EditorConfirmReplacement);
        assert_eq!(
            std::fs::read(gui.controller.track(id).expect("track").source().path())
                .expect("managed replacement"),
            replacement_bytes
        );

        let mut cover_name = b"cover-".to_vec();
        cover_name.push(0xfe);
        cover_name.extend_from_slice(b".png");
        let cover = dir.path().join(OsString::from_vec(cover_name));
        write_test_png(&cover, 64, 64);
        gui.view = LibraryView::Albums;
        gui.selected_group = Some(album_key_for_title(album, "Native Album"));
        send(
            &mut gui,
            Message::AlbumCoverPicked(0, Ok(Some(vec![cover.clone()]))),
        );
        assert!(gui.album_cover_picked.is_some());
        send(
            &mut gui,
            Message::AlbumCoverChanged("typed cover path".to_owned()),
        );
        assert!(gui.album_cover_picked.is_none());
        send(
            &mut gui,
            Message::AlbumCoverPicked(0, Ok(Some(vec![cover]))),
        );
        send(&mut gui, Message::AlbumCoverApply);
        assert!(
            gui.controller.album(album).expect("album").cover.is_some(),
            "the hidden non-UTF-8 cover path must survive publication"
        );
        assert_eq!(
            gui.controller.album_cover_resource_state(album),
            Some(ResourceState::Present)
        );
        gui.controller.shutdown();
    }

    #[test]
    fn gui_repairs_missing_audio_and_rejects_invalid_cover_images() {
        let dir = TempDir::new("gui-repair-resources");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Repair Me",
            "Repair Artist",
            "Repair Album",
            ReleaseType::Album,
            1,
        );
        let album = gui.controller.track_album_id(id).expect("album");
        let audio = gui
            .controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();
        std::fs::remove_file(&audio).expect("remove managed audio");
        send(&mut gui, Message::EditTrack(id));
        let replacement = dir.path().join("replacement.wav");
        let mut replacement_bytes = minimal_wav();
        replacement_bytes.push(9);
        std::fs::write(&replacement, replacement_bytes).expect("replacement");
        send(
            &mut gui,
            Message::EditorReplacementChanged(replacement.display().to_string()),
        );
        send(&mut gui, Message::EditorPlanReplacement);
        assert!(
            gui.track_editor
                .as_ref()
                .and_then(|editor| editor.replacement_plan.as_ref())
                .is_some_and(|plan| plan.restores_missing_audio())
        );
        send(&mut gui, Message::EditorConfirmReplacement);
        assert!(gui.controller.track(id).is_some());
        assert_eq!(
            gui.controller.track_audio_resource_state(id),
            Some(ResourceState::Present)
        );
        assert_eq!(gui.controller.track_album_id(id), Some(album));

        gui.view = LibraryView::Albums;
        gui.selected_group = Some(album_key_for_title(album, "Repair Album"));
        let invalid = dir.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").expect("invalid cover");
        gui.album_cover_input = invalid.display().to_string();
        send(&mut gui, Message::AlbumCoverApply);
        assert!(gui.controller.album(album).expect("album").cover.is_none());
        assert!(
            gui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("rejected"))
        );

        let valid = dir.path().join("valid-cover.png");
        write_test_png(&valid, 64, 64);
        gui.album_cover_input = valid.display().to_string();
        send(&mut gui, Message::AlbumCoverApply);
        assert!(gui.controller.album(album).expect("album").cover.is_some());
        let managed = gui.controller.album_cover_path(album).expect("cover path");
        std::fs::remove_file(managed).expect("remove cover");
        assert_eq!(
            gui.controller.album_cover_resource_state(album),
            Some(ResourceState::Missing)
        );
        let repaired = dir.path().join("repaired-cover.png");
        write_test_png(&repaired, 32, 32);
        gui.album_cover_input = repaired.display().to_string();
        send(&mut gui, Message::AlbumCoverApply);
        assert_eq!(
            gui.controller.album_cover_resource_state(album),
            Some(ResourceState::Present)
        );
        gui.controller.shutdown();
    }

    #[test]
    fn gui_empty_album_create_metadata_cover_delete_and_reorder() {
        let dir = TempDir::new("gui-album-lifecycle");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let artist = gui.controller.create_artist("Artist").expect("artist");
        send(
            &mut gui,
            Message::ArtistSelected(Some(ArtistChoice {
                id: Some(artist),
                label: "Artist".to_owned(),
            })),
        );
        send(&mut gui, Message::NewAlbumTitleChanged("Empty".to_owned()));
        send(&mut gui, Message::AlbumCreatePressed);
        let empty = gui
            .controller
            .albums()
            .into_iter()
            .find(|album| album.title == "Empty")
            .expect("empty album");
        assert_eq!(gui.view, LibraryView::Albums);
        assert_eq!(gui.selected_album_id(), Some(empty.id));
        assert!(
            gui.groups_for(LibraryView::Albums)
                .iter()
                .any(|(key, count)| {
                    album_key_parts(key).1 == empty.id.to_string() && *count == 0
                })
        );
        send(&mut gui, Message::AlbumTitleChanged("Renamed".to_owned()));
        send(&mut gui, Message::AlbumType(ReleaseType::Ep));
        send(&mut gui, Message::AlbumMetadataSave);
        assert_eq!(
            gui.controller
                .album(empty.id)
                .map(|album| album.title.clone()),
            Some("Renamed".to_owned())
        );
        assert_eq!(
            gui.controller
                .album(empty.id)
                .map(|album| album.release_type),
            Some(ReleaseType::Ep)
        );
        let cover = dir.path().join("cover.png");
        write_test_png(&cover, 64, 64);
        send(
            &mut gui,
            Message::AlbumCoverChanged(cover.display().to_string()),
        );
        send(&mut gui, Message::AlbumCoverApply);
        assert_eq!(
            gui.controller
                .album(empty.id)
                .and_then(|album| album.cover.clone()),
            Some("cover.png".to_owned())
        );
        send(&mut gui, Message::AlbumCoverRemove);
        send(&mut gui, Message::AlbumCoverRemove);
        assert!(
            gui.controller
                .album(empty.id)
                .and_then(|album| album.cover.clone())
                .is_none()
        );
        send(&mut gui, Message::AlbumDeletePressed);
        assert!(gui.album_delete_confirm);
        send(&mut gui, Message::AlbumDeleteConfirm);
        assert!(gui.controller.album(empty.id).is_none());
        assert!(gui.selected_group.is_none());

        // Create a populated album and reorder through the same detail view.
        send(
            &mut gui,
            Message::NewAlbumTitleChanged("Populated".to_owned()),
        );
        send(&mut gui, Message::AlbumCreatePressed);
        let populated = gui
            .controller
            .albums()
            .into_iter()
            .find(|album| album.title == "Populated")
            .expect("populated album");
        let source = dir.path().join("source/one.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let first = gui
            .controller
            .import_canonical_track_into_album(
                populated.id,
                flux_app::CanonicalImportedTrack {
                    source,
                    title: "One".to_owned(),
                    artist_id: None,
                    artist_name: "ignored".to_owned(),
                    credited_artist_ids: Vec::new(),
                    album_title: None,
                    release_type: ReleaseType::Album,
                    track_number: Some(1),
                    disc_number: None,
                    duration_ms: None,
                },
            )
            .expect("first");
        let source = dir.path().join("source/two.wav");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let second = gui
            .controller
            .import_canonical_track_into_album(
                populated.id,
                flux_app::CanonicalImportedTrack {
                    source,
                    title: "Two".to_owned(),
                    artist_id: None,
                    artist_name: "ignored".to_owned(),
                    credited_artist_ids: Vec::new(),
                    album_title: None,
                    release_type: ReleaseType::Album,
                    track_number: Some(2),
                    disc_number: None,
                    duration_ms: None,
                },
            )
            .expect("second");
        gui.selected_group = Some(album_key_for_title(populated.id, "Populated"));
        gui.sync_album_editor();
        send(&mut gui, Message::AlbumTrackMove(second, -1));
        assert_eq!(
            gui.controller.album_tracks(populated.id),
            vec![second, first]
        );
        send(&mut gui, Message::AlbumDeletePressed);
        assert!(
            !gui.album_delete_confirm,
            "populated album deletion is blocked"
        );
        gui.controller.shutdown();
    }

    #[test]
    fn gui_import_manager_targets_existing_album_and_rejects_stale_target() {
        let dir = TempDir::new("gui-album-import-target");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let artist = gui.controller.create_artist("Owner").expect("artist");
        let album = gui
            .controller
            .create_album(artist, "Target Album", ReleaseType::Album)
            .expect("album");
        let cover = dir.path().join("cover.png");
        std::fs::write(&cover, b"\x89PNG\r\n\x1a\ncover").expect("cover");
        gui.controller
            .set_album_cover(album, &cover)
            .expect("cover");
        gui.view = LibraryView::Albums;
        gui.selected_group = Some(album_key_for_title(album, "Target Album"));
        gui.sync_album_editor();
        send(&mut gui, Message::AlbumAddTracks);
        assert_eq!(
            gui.import_manager
                .as_ref()
                .and_then(|manager| manager.target_album.clone())
                .map(|target| target.album_id),
            Some(album)
        );
        send(&mut gui, Message::ImportCancel);
        assert!(gui.import_manager.is_none());
        assert!(
            gui.controller.album_tracks(album).is_empty(),
            "cancel writes nothing"
        );

        send(&mut gui, Message::AlbumAddTracks);
        let target = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.target_album.clone())
            .expect("target");
        let one = dir.path().join("source/one.wav");
        let two = dir.path().join("source/two.wav");
        std::fs::create_dir_all(one.parent().expect("parent")).expect("dirs");
        std::fs::write(&one, minimal_wav()).expect("one");
        let mut second = minimal_wav();
        second.push(1);
        std::fs::write(&two, second).expect("two");
        let request = ImportRequest {
            sources: vec![two.clone(), one.clone()],
            recursive: false,
            storage: ImportStorage::Copy,
            library_root: root.clone(),
            existing_tracks: gui.controller.tracks().to_vec(),
            existing_artists: Vec::new(),
            existing_credits: Vec::new(),
            existing_album_ids: Vec::new(),
            target_album: Some(target.clone()),
            generation: gui.controller.generation(),
            library_session: gui.controller.library_session(),
        };
        let plan = prepare_import_plan(
            request,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            plan.target_album.as_ref().map(|target| target.album_id),
            Some(album)
        );
        assert!(
            plan.candidates
                .iter()
                .all(|candidate| candidate.album() == Some("Target Album"))
        );
        send(
            &mut gui,
            Message::ImportPlanReady(0, Box::new(plan.clone())),
        );
        let prepared = prepare_import_files(
            &plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(&mut gui, Message::ImportPrepared(0, Box::new(prepared)));
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").stage,
            ImportUiStage::Completed
        );
        assert_eq!(gui.controller.album_tracks(album).len(), 2);
        assert_eq!(
            gui.controller.album(album).map(|album| album.title.clone()),
            Some("Target Album".to_owned())
        );
        assert_eq!(
            gui.controller
                .album(album)
                .and_then(|album| album.cover.clone()),
            Some("cover.png".to_owned())
        );

        // Stale target title after review must reject the whole batch.
        send(&mut gui, Message::ImportReset);
        let stale_target = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.target_album.clone());
        // ImportReset keeps the fixed target; rebuild through album detail if needed.
        let stale_target = stale_target.expect("target retained after reset");
        gui.controller
            .update_album_metadata(album, "Renamed Target", ReleaseType::Album)
            .expect("rename");
        let stale_source = dir.path().join("source/stale.wav");
        let mut stale = minimal_wav();
        stale.push(2);
        std::fs::write(&stale_source, stale).expect("stale source");
        let stale_plan = prepare_import_plan(
            ImportRequest {
                sources: vec![stale_source],
                recursive: false,
                storage: ImportStorage::Copy,
                library_root: root,
                existing_tracks: gui.controller.tracks().to_vec(),
                existing_artists: Vec::new(),
                existing_credits: Vec::new(),
                existing_album_ids: Vec::new(),
                target_album: Some(stale_target),
                generation: gui.controller.generation(),
                library_session: gui.controller.library_session(),
            },
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(
            &mut gui,
            Message::ImportPlanReady(0, Box::new(stale_plan.clone())),
        );
        let stale_prepared = prepare_import_files(
            &stale_plan,
            Arc::new(Mutex::new(ImportProgress::default())),
            Arc::new(AtomicBool::new(false)),
        );
        send(
            &mut gui,
            Message::ImportPrepared(0, Box::new(stale_prepared)),
        );
        assert!(
            gui.controller.album_tracks(album).len() == 2,
            "stale import must not append"
        );
        assert_eq!(
            gui.import_manager.as_ref().expect("manager").stage,
            ImportUiStage::Failed
        );
        gui.controller.shutdown();
    }

    fn write_test_png(path: &Path, width: u32, height: u32) {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([40, 80, 160, 255]));
        image.save(path).expect("write png");
    }

    #[test]
    fn artwork_decodes_supported_images_and_rejects_missing_invalid_or_oversized() {
        let dir = TempDir::new("gui-artwork-decode");
        let png = dir.path().join("cover.png");
        write_test_png(&png, 600, 300);
        let key = artwork_key(ArtworkOwner::Album(AlbumId::new(1)), png.clone());
        let decoded = decode_artwork(&key).expect("png decodes");
        match decoded.handle {
            ImageHandle::Rgba { width, height, .. } => {
                assert_eq!(width, 512);
                assert_eq!(height, 256);
            }
            _ => panic!("expected decoded RGBA artwork"),
        }

        let jpeg = dir.path().join("cover.jpg");
        image::RgbImage::from_pixel(64, 32, image::Rgb([120, 40, 80]))
            .save_with_format(&jpeg, image::ImageFormat::Jpeg)
            .expect("write jpeg");
        assert!(decode_artwork(&artwork_key(ArtworkOwner::Album(AlbumId::new(1)), jpeg)).is_ok());
        let webp = dir.path().join("cover.webp");
        image::RgbImage::from_pixel(48, 48, image::Rgb([20, 120, 80]))
            .save_with_format(&webp, image::ImageFormat::WebP)
            .expect("write webp");
        assert!(decode_artwork(&artwork_key(ArtworkOwner::Album(AlbumId::new(1)), webp)).is_ok());

        let missing = artwork_key(
            ArtworkOwner::Album(AlbumId::new(1)),
            dir.path().join("missing.png"),
        );
        assert!(decode_artwork(&missing).is_err());

        let invalid = dir.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").expect("invalid");
        assert!(
            decode_artwork(&artwork_key(ArtworkOwner::Album(AlbumId::new(1)), invalid)).is_err()
        );

        let oversized = dir.path().join("oversized.png");
        write_test_png(&oversized, 5_000, 1);
        assert!(
            decode_artwork(&artwork_key(
                ArtworkOwner::Album(AlbumId::new(1)),
                oversized
            ))
            .is_err()
        );

        let unsupported = dir.path().join("cover.bmp");
        std::fs::write(&unsupported, b"BM unsupported").expect("bmp");
        assert!(
            decode_artwork(&artwork_key(
                ArtworkOwner::Album(AlbumId::new(1)),
                unsupported
            ))
            .is_err()
        );
    }

    #[test]
    fn artwork_key_changes_after_cover_replacement() {
        let dir = TempDir::new("gui-artwork-invalidation");
        let path = dir.path().join("cover.png");
        write_test_png(&path, 32, 32);
        let first = artwork_key(ArtworkOwner::Album(AlbumId::new(1)), path.clone());
        std::thread::sleep(Duration::from_millis(5));
        write_test_png(&path, 33, 33);
        let second = artwork_key(ArtworkOwner::Album(AlbumId::new(1)), path);
        assert_ne!(first, second);
    }

    #[test]
    fn native_picker_cancellation_and_selection_still_requires_review() {
        let dir = TempDir::new("gui-native-selection");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let source_dir = dir.path().join("source");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("song");
        gui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("import");
        let id = gui.controller.tracks()[0].0;
        let original_audio = gui
            .controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();

        send(&mut gui, Message::EditTrack(id));
        send(&mut gui, Message::EditorReplacementPicked(0, Ok(None)));
        assert!(gui.track_editor.as_ref().is_some_and(
            |editor| editor.replacement_source.is_empty() && editor.replacement_plan.is_none()
        ));
        let invalid_replacement = source_dir.join("not-audio.txt");
        std::fs::write(&invalid_replacement, b"invalid").expect("invalid replacement");
        send(
            &mut gui,
            Message::EditorReplacementPicked(0, Ok(Some(vec![invalid_replacement]))),
        );
        send(&mut gui, Message::EditorPlanReplacement);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_plan.is_none() && editor.error.is_some())
        );
        let replacement = source_dir.join("replacement.wav");
        let mut replacement_bytes = minimal_wav();
        replacement_bytes.push(7);
        std::fs::write(&replacement, &replacement_bytes).expect("replacement");
        send(
            &mut gui,
            Message::EditorReplacementPicked(0, Ok(Some(vec![replacement.clone()]))),
        );
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_plan.is_none())
        );
        send(&mut gui, Message::EditorPlanReplacement);
        assert!(
            gui.track_editor
                .as_ref()
                .is_some_and(|editor| editor.replacement_plan.is_some())
        );
        send(&mut gui, Message::EditorConfirmReplacement);
        assert_eq!(
            std::fs::read(gui.controller.track(id).expect("track").source().path())
                .expect("new audio"),
            replacement_bytes
        );
        assert!(!original_audio.exists());

        // Cover selection also only fills the review field.
        let album = gui.controller.track_album_id(id).expect("album");
        gui.view = LibraryView::Albums;
        gui.selected_group = Some(album_key_for_title(album, "Album"));
        send(&mut gui, Message::AlbumCoverPicked(0, Ok(None)));
        assert!(gui.controller.album(album).expect("album").cover.is_none());
        let invalid = dir.path().join("cover.txt");
        std::fs::write(&invalid, b"not an image").expect("invalid cover");
        send(
            &mut gui,
            Message::AlbumCoverPicked(0, Ok(Some(vec![invalid.display().to_string().into()]))),
        );
        send(&mut gui, Message::AlbumCoverApply);
        assert!(gui.controller.album(album).expect("album").cover.is_none());
        let cover = dir.path().join("cover.png");
        write_test_png(&cover, 64, 64);
        send(
            &mut gui,
            Message::AlbumCoverPicked(0, Ok(Some(vec![cover.clone()]))),
        );
        send(&mut gui, Message::AlbumCoverApply);
        assert_eq!(
            gui.controller.album(album).expect("album").cover.as_deref(),
            Some("cover.png")
        );
        assert!(
            !gui.artwork.contains_key(&album),
            "replacement invalidates cache"
        );
        gui.controller.shutdown();
    }

    #[test]
    fn native_import_picker_preserves_fixed_album_target() {
        let dir = TempDir::new("gui-native-target");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let artist = gui.controller.create_artist("A").expect("artist");
        let album = gui
            .controller
            .create_album(artist, "Target", flux_app::ReleaseType::Album)
            .expect("album");
        let target = ImportAlbumTarget {
            album_id: album,
            owner_artist_id: artist,
            owner_name: "A".to_owned(),
            title: "Target".to_owned(),
            release_type: flux_app::ReleaseType::Album,
            cover: None,
            track_order: Vec::new(),
            generation: gui.controller.generation(),
        };
        let mut manager = ImportManager::new();
        manager.set_album_target(target.clone());
        gui.import_manager = Some(manager);
        gui.prepare_native_import_picker();
        assert_eq!(
            gui.import_manager
                .as_ref()
                .and_then(|manager| manager.target_album()),
            Some(target)
        );
        gui.controller.shutdown();
    }

    fn import_named_track(
        gui: &mut FluxReady,
        dir: &TempDir,
        title: &str,
        artist_name: &str,
        album_title: &str,
        release_type: ReleaseType,
        marker: u8,
    ) -> TrackId {
        let artist = gui
            .controller
            .create_artist(artist_name)
            .expect("create artist");
        let source_dir = dir.path().join("fixtures").join(format!("{marker}"));
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join(format!("{marker}.wav"));
        let mut bytes = minimal_wav();
        bytes.push(marker);
        std::fs::write(&source, bytes).expect("source");
        gui.controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source,
                title: title.to_owned(),
                artist_id: Some(artist),
                artist_name: artist_name.to_owned(),
                credited_artist_ids: vec![artist],
                album_title: Some(album_title.to_owned()),
                release_type,
                track_number: Some(u32::from(marker)),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import")
    }

    #[test]
    fn global_search_groups_entities_and_navigates_without_writes() {
        let dir = TempDir::new("gui-global-search");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Zed Track",
            "Zed Artist",
            "Zed Album",
            ReleaseType::Album,
            1,
        );
        let album = gui.controller.track_album_id(id).expect("album");
        let artist = gui.controller.track_artist_id(id).expect("artist");
        let generation = gui.controller.generation();

        send(&mut gui, Message::SearchChanged("zEd".to_owned()));
        {
            let cache = gui.search_snapshot();
            assert_eq!(cache.artists.len(), 1);
            assert_eq!(cache.albums, vec![(album, "Zed Album".to_owned())]);
            assert_eq!(cache.track_ids, vec![id]);
        }
        assert_eq!(
            gui.controller.generation(),
            generation,
            "search is read-only"
        );

        send(&mut gui, Message::SearchNavigateAlbum(album));
        assert_eq!(gui.view, LibraryView::Albums);
        assert_eq!(
            gui.selected_group.as_deref(),
            Some(album_key_for_title(album, "Zed Album").as_str())
        );
        assert!(gui.search.is_empty());

        send(&mut gui, Message::SearchChanged("ZED".to_owned()));
        send(&mut gui, Message::SearchNavigateArtist(artist));
        assert_eq!(gui.view, LibraryView::Artists);
        assert!(
            gui.selected_group
                .as_deref()
                .is_some_and(|key| key.ends_with(&artist.to_string()))
        );

        send(&mut gui, Message::SearchChanged("zed".to_owned()));
        send(&mut gui, Message::SearchNavigateTrack(id));
        assert_eq!(gui.view, LibraryView::Tracks);
        assert_eq!(gui.focused_track, Some(id));
        assert!(gui.search.is_empty());

        send(
            &mut gui,
            Message::SearchChanged("no-such-catalogue-entry".to_owned()),
        );
        {
            let cache = gui.search_snapshot();
            assert!(cache.artists.is_empty());
            assert!(cache.albums.is_empty());
            assert!(cache.track_ids.is_empty());
        }
        send(&mut gui, Message::ClearSearch);
        assert!(gui.search.is_empty());
        gui.controller.shutdown();
    }

    #[test]
    fn filters_and_sorting_are_deterministic_and_album_order_is_preserved() {
        let dir = TempDir::new("gui-filters-sort");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let charlie = import_named_track(
            &mut gui,
            &dir,
            "Charlie",
            "Charlie Artist",
            "Charlie Album",
            ReleaseType::Album,
            3,
        );
        let alpha = import_named_track(
            &mut gui,
            &dir,
            "Alpha",
            "Alpha Artist",
            "Alpha Album",
            ReleaseType::Ep,
            1,
        );
        let bravo = import_named_track(
            &mut gui,
            &dir,
            "Bravo",
            "Bravo Artist",
            "Bravo Album",
            ReleaseType::Single,
            2,
        );
        gui.controller
            .set_favorites(&[bravo], true)
            .expect("favorite");
        gui.controller
            .set_lyrics(alpha, Some("lyrics"))
            .expect("lyrics");

        send(&mut gui, Message::SortTracksChanged(TrackSort::Title));
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![alpha, bravo, charlie]
        );

        send(&mut gui, Message::FilterFavoritesToggled);
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![bravo]
        );
        send(&mut gui, Message::FilterFavoritesToggled);
        send(&mut gui, Message::FilterLyricsCycle);
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![alpha]
        );
        send(&mut gui, Message::ResetLibraryFilters);
        send(&mut gui, Message::FilterReleaseChanged(ReleaseFilter::Ep));
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![alpha]
        );
        send(&mut gui, Message::ResetLibraryFilters);
        assert!(!gui.filters_active(), "reset clears every filter and sort");

        // Album detail always follows the canonical album order, even when a
        // title sort is active in the general library view.
        let owner = gui.controller.track_artist_id(charlie).expect("owner");
        let album = gui.controller.track_album_id(charlie).expect("album");
        let second = gui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source: {
                    let source = dir.path().join("fixtures/album-second.wav");
                    let mut bytes = minimal_wav();
                    bytes.push(9);
                    std::fs::write(&source, bytes).expect("second");
                    source
                },
                title: "Aardvark".to_owned(),
                artist_id: Some(owner),
                artist_name: "Charlie Artist".to_owned(),
                credited_artist_ids: vec![owner],
                album_title: Some("Charlie Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("second album track");
        let album = gui.controller.track_album_id(second).unwrap_or(album);
        send(&mut gui, Message::SortTracksChanged(TrackSort::Title));
        send(&mut gui, Message::SelectView(LibraryView::Albums));
        send(
            &mut gui,
            Message::SelectGroup(Some(album_key_for_title(album, "Charlie Album"))),
        );
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![charlie, second]
        );
        gui.controller.shutdown();
    }

    #[test]
    fn track_selection_is_id_based_and_batch_favorites_preserve_playback() {
        let dir = TempDir::new("gui-selection-favorites");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        gui.controller
            .set_rating(one, Some(4), false)
            .expect("rating");
        let queue_entry = gui.controller.current_queue_entry_id();
        let playback_state = gui.controller.player_state();

        send(&mut gui, Message::TrackSelectionToggle(one));
        send(&mut gui, Message::TrackSelectionToggle(two));
        assert_eq!(gui.selected_tracks, vec![one, two]);
        send(&mut gui, Message::SortTracksChanged(TrackSort::Title));
        assert_eq!(gui.selected_tracks, vec![one, two], "sorting keeps IDs");
        send(&mut gui, Message::BatchFavorites(true));
        assert!(gui.selected_tracks.is_empty());
        assert!(gui.controller.rating(one).is_some_and(|r| r.favorite));
        assert!(gui.controller.rating(two).is_some_and(|r| r.favorite));
        assert_eq!(
            gui.controller.rating(one).and_then(|r| r.rating),
            Some(4),
            "batch favorite preserves the rating value"
        );
        assert_eq!(gui.controller.current_queue_entry_id(), queue_entry);
        assert_eq!(gui.controller.player_state(), playback_state);

        send(&mut gui, Message::TrackSelectionToggle(one));
        send(&mut gui, Message::FilterFavoritesToggled);
        assert!(
            gui.selected_tracks.is_empty(),
            "filter changes clear selection"
        );
        send(&mut gui, Message::FilterFavoritesToggled);
        send(&mut gui, Message::TrackSelectionSelectAll);
        assert_eq!(
            gui.selected_tracks.len(),
            2,
            "select all uses visible TrackIds"
        );
        send(&mut gui, Message::SelectView(LibraryView::Artists));
        assert!(
            gui.selected_tracks.is_empty(),
            "view changes clear selection"
        );
        gui.controller.shutdown();
    }

    #[test]
    fn batch_playlist_review_preserves_order_and_rejects_stale_ids() {
        let dir = TempDir::new("gui-batch-playlist");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        let queue_entry = gui.controller.current_queue_entry_id();
        let playback_state = gui.controller.player_state();

        send(&mut gui, Message::TrackSelectionToggle(two));
        send(&mut gui, Message::TrackSelectionToggle(one));
        send(&mut gui, Message::BatchPlaylistCreateOpen);
        assert!(gui.batch_review.is_some());
        send(
            &mut gui,
            Message::BatchPlaylistNameChanged("Mix".to_owned()),
        );
        send(&mut gui, Message::BatchReviewConfirm);
        assert!(gui.batch_review.is_none());
        assert!(gui.selected_tracks.is_empty());
        let playlist = gui
            .controller
            .playlists()
            .into_iter()
            .find(|playlist| playlist.name == "Mix")
            .expect("created playlist");
        assert_eq!(
            playlist.track_ids,
            vec![two, one],
            "selection order preserved"
        );

        send(&mut gui, Message::TrackSelectionToggle(one));
        send(&mut gui, Message::BatchPlaylistAdd(playlist.id));
        assert!(gui.batch_review.is_some());
        send(&mut gui, Message::BatchReviewConfirm);
        let updated = gui
            .controller
            .playlists()
            .into_iter()
            .find(|entry| entry.id == playlist.id)
            .expect("playlist");
        assert_eq!(updated.track_ids, vec![two, one, one]);

        let count = gui.controller.playlists().len();
        gui.selected_tracks = vec![one, TrackId::new(99_999)];
        send(&mut gui, Message::BatchPlaylistCreateOpen);
        send(
            &mut gui,
            Message::BatchPlaylistNameChanged("Bad".to_owned()),
        );
        send(&mut gui, Message::BatchReviewConfirm);
        assert!(gui.batch_review.is_some());
        assert!(
            gui.batch_review
                .as_ref()
                .and_then(|review| review.error.as_ref())
                .is_some()
        );
        assert_eq!(
            gui.controller.playlists().len(),
            count,
            "no partial playlist"
        );
        send(&mut gui, Message::BatchReviewCancel);
        assert_eq!(gui.controller.current_queue_entry_id(), queue_entry);
        assert_eq!(gui.controller.player_state(), playback_state);
        gui.controller.shutdown();
    }

    #[test]
    fn playlist_library_creates_renames_and_deletes_without_touching_tracks() {
        let dir = TempDir::new("gui-playlist-library");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let track = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let queue_entry = gui.controller.current_queue_entry_id();
        let playback_state = gui.controller.player_state();

        send(&mut gui, Message::PlaylistCreateOpen);
        send(&mut gui, Message::PlaylistCreateConfirm);
        assert!(gui.playlist_error.is_some(), "empty name is rejected");
        assert_eq!(gui.controller.playlists().len(), 0);
        send(
            &mut gui,
            Message::PlaylistCreateNameChanged("Mix".to_owned()),
        );
        send(&mut gui, Message::PlaylistCreateConfirm);
        assert_eq!(gui.view, LibraryView::Playlists);
        assert_eq!(gui.controller.playlists().len(), 1);
        let playlist = gui.selected_playlist.expect("opened playlist");
        assert!(
            gui.controller
                .playlist(playlist)
                .is_some_and(|playlist| playlist.track_ids.is_empty())
        );

        send(
            &mut gui,
            Message::PlaylistRenameChanged("Renamed".to_owned()),
        );
        send(&mut gui, Message::PlaylistRenameSave);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|playlist| playlist.name.as_str()),
            Some("Renamed")
        );
        assert!(
            gui.controller
                .playlist(playlist)
                .is_some_and(|playlist| playlist.track_ids.is_empty())
        );

        send(&mut gui, Message::PlaylistDelete);
        assert!(gui.controller.playlist(playlist).is_some());
        send(&mut gui, Message::PlaylistDelete);
        assert!(gui.controller.playlist(playlist).is_none());
        assert!(gui.controller.track(track).is_some(), "track is untouched");
        assert_eq!(gui.controller.current_queue_entry_id(), queue_entry);
        assert_eq!(gui.controller.player_state(), playback_state);
        gui.controller.shutdown();
    }

    #[test]
    fn playlist_entries_remove_reorder_clear_and_reject_stale_plans() {
        let dir = TempDir::new("gui-playlist-entries");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        let playlist = gui.controller.create_playlist("Entries").expect("playlist");
        gui.controller
            .set_playlist(Some(playlist), "Entries", vec![one, two, two])
            .expect("contents");
        let queue_entry = gui.controller.current_queue_entry_id();
        let playback_state = gui.controller.player_state();
        send(&mut gui, Message::PlaylistSelect(playlist));
        assert_eq!(gui.selected_playlist, Some(playlist));
        assert_eq!(gui.view, LibraryView::Playlists);

        send(&mut gui, Message::PlaylistEntryToggle(1));
        send(&mut gui, Message::PlaylistEntryToggle(2));
        send(&mut gui, Message::PlaylistRemoveSelected);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![one, two, two]),
            "first click only asks for confirmation"
        );
        send(&mut gui, Message::PlaylistRemoveSelected);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![one]),
            "duplicate entries are removed by position"
        );
        assert!(gui.controller.track(two).is_some());

        gui.controller
            .set_playlist(Some(playlist), "Entries", vec![one, two])
            .expect("reset contents");
        send(&mut gui, Message::PlaylistEntryMove(0, 1));
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![two, one])
        );
        gui.controller
            .set_playlist(Some(playlist), "Entries", vec![one, two])
            .expect("reset contents");
        send(&mut gui, Message::PlaylistDragStart(0));
        send(&mut gui, Message::PlaylistDragHover(2));
        send(&mut gui, Message::QueueDragRelease);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![two, one])
        );

        // A drag snapshot that no longer matches the playlist is rejected.
        gui.controller
            .set_playlist(Some(playlist), "Entries", vec![one, two])
            .expect("reset contents");
        send(&mut gui, Message::PlaylistDragStart(0));
        gui.controller
            .set_playlist(Some(playlist), "Entries", vec![two, one])
            .expect("concurrent change");
        send(&mut gui, Message::PlaylistDragHover(2));
        send(&mut gui, Message::QueueDragRelease);
        assert!(gui.playlist_error.is_some());
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![two, one]),
            "stale drag does not reorder"
        );

        send(&mut gui, Message::PlaylistClear);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(vec![two, one])
        );
        send(&mut gui, Message::PlaylistClear);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(Vec::new())
        );
        assert!(gui.controller.track(one).is_some());
        assert!(gui.controller.track(two).is_some());
        assert_eq!(gui.controller.current_queue_entry_id(), queue_entry);
        assert_eq!(gui.controller.player_state(), playback_state);

        let before = gui
            .controller
            .playlist(playlist)
            .map(|p| p.track_ids.clone())
            .unwrap();
        assert!(
            gui.controller
                .remove_playlist_entries(playlist, &[0], &[TrackId::new(99_999)])
                .is_err()
        );
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(before)
        );
        gui.controller.shutdown();
    }

    #[test]
    fn playlist_playback_preserves_order_duplicates_and_queue_semantics() {
        let dir = TempDir::new("gui-playlist-playback");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        let playlist = gui
            .controller
            .create_playlist("Playback")
            .expect("playlist");
        let canonical_order = vec![one, two, one];
        gui.controller
            .set_playlist(Some(playlist), "Playback", canonical_order.clone())
            .expect("contents");
        gui.controller.play_track(one);
        send(&mut gui, Message::PlaylistSelect(playlist));

        let before_len = gui.controller.queue_len();
        let before_current = gui.controller.current_queue_entry_id();
        send(&mut gui, Message::PlaylistEnqueue);
        assert_eq!(gui.controller.queue_len(), before_len + 3);
        assert_eq!(gui.controller.current_queue_entry_id(), before_current);
        assert_eq!(
            gui.controller.queue_entries()[before_len..]
                .iter()
                .map(|entry| entry.track())
                .collect::<Vec<_>>(),
            canonical_order
        );

        send(&mut gui, Message::PlaylistPlayEntry(2));
        assert_eq!(gui.controller.queue_len(), 3);
        let current = gui.controller.current_queue_entry_id().expect("current");
        assert_eq!(gui.controller.queue_entry_index(current), Some(2));
        assert_eq!(
            gui.controller.current_track().map(|track| track.title()),
            Some("One")
        );
        let current_before_edit = gui.controller.current_queue_entry_id();
        send(&mut gui, Message::PlaylistEntryMove(0, 1));
        assert_eq!(gui.controller.current_queue_entry_id(), current_before_edit);
        let reordered_order = vec![two, one, one];
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(reordered_order.clone())
        );

        send(&mut gui, Message::PlaylistShuffle);
        assert_eq!(
            gui.controller
                .playlist(playlist)
                .map(|p| p.track_ids.clone()),
            Some(reordered_order),
            "shuffle never mutates canonical order"
        );
        assert_eq!(gui.controller.queue_len(), 3);
        send(&mut gui, Message::PlaylistPlay);
        assert_eq!(gui.controller.queue_len(), 3);
        let current = gui.controller.current_queue_entry_id().expect("current");
        assert_eq!(gui.controller.queue_entry_index(current), Some(0));
        gui.controller.shutdown();
    }

    #[test]
    fn lyrics_library_classifies_and_filters_plain_and_synced_tracks() {
        let dir = TempDir::new("gui-lyrics-library");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let plain = import_named_track(
            &mut gui,
            &dir,
            "Plain Track",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            1,
        );
        let synced = import_named_track(
            &mut gui,
            &dir,
            "Sync Track",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            2,
        );
        gui.controller
            .set_lyrics(plain, Some("Café 世界\nsecond line"))
            .expect("plain lyrics");
        gui.controller
            .set_lyrics(synced, Some("[00:01.000]first\n[00:02.500]second"))
            .expect("synced lyrics");
        send(&mut gui, Message::SelectView(LibraryView::Lyrics));
        assert_eq!(gui.lyrics_kind_for(plain), LyricsKind::Plain);
        assert_eq!(gui.lyrics_kind_for(synced), LyricsKind::Synced);
        assert_eq!(gui.filtered_lyrics_tracks().len(), 2);
        send(&mut gui, Message::LyricsFilterChanged("plain".to_owned()));
        assert_eq!(
            gui.filtered_lyrics_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![plain]
        );
        send(&mut gui, Message::LyricsSelect(synced));
        assert_eq!(gui.view, LibraryView::Lyrics);
        assert_eq!(gui.selected_lyrics, Some(synced));
        assert_eq!(gui.lyrics_mode, LyricsMode::Read);
        assert!(
            gui.lyrics_cache_entry(synced)
                .and_then(|entry| entry.document)
                .is_some_and(|document| document.kind() == LyricsKind::Synced)
        );
        gui.controller.shutdown();
    }

    #[test]
    fn lyrics_editor_preserves_unicode_blank_lines_and_rejects_stale_or_cancelled_edits() {
        let dir = TempDir::new("gui-lyrics-editor");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "Edit Track",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            1,
        );
        send(&mut gui, Message::LyricsSelect(id));
        send(&mut gui, Message::LyricsEditOpen);
        send(
            &mut gui,
            Message::LyricsEditorTextChanged(0, "Café 世界".to_owned()),
        );
        send(&mut gui, Message::LyricsEditorAddLine);
        send(&mut gui, Message::LyricsEditorAddLine);
        send(
            &mut gui,
            Message::LyricsEditorTextChanged(2, "second".to_owned()),
        );
        send(&mut gui, Message::LyricsEditorSave);
        assert!(gui.lyrics_editor.is_none());
        assert_eq!(
            gui.controller.track_lyrics(id).as_deref(),
            Some("Café 世界\n\nsecond\n")
        );
        assert_eq!(gui.lyrics_kind_for(id), LyricsKind::Plain);

        // Unsaved cancellation requires a second explicit action.
        send(&mut gui, Message::LyricsEditOpen);
        send(
            &mut gui,
            Message::LyricsEditorTextChanged(0, "discard me".to_owned()),
        );
        send(&mut gui, Message::LyricsEditorCancel);
        assert!(gui.lyrics_editor.is_some());
        send(&mut gui, Message::LyricsEditorCancel);
        assert!(gui.lyrics_editor.is_none());
        assert_eq!(
            gui.controller.track_lyrics(id).as_deref(),
            Some("Café 世界\n\nsecond\n")
        );

        // A stale edit cannot overwrite lyrics changed elsewhere.
        send(&mut gui, Message::LyricsEditOpen);
        gui.controller
            .set_lyrics(id, Some("newer external lyrics"))
            .expect("external edit");
        send(
            &mut gui,
            Message::LyricsEditorTextChanged(0, "stale edit".to_owned()),
        );
        send(&mut gui, Message::LyricsEditorSave);
        assert!(gui.lyrics_editor.is_some());
        assert!(
            gui.lyrics_editor
                .as_ref()
                .and_then(|editor| editor.error.as_ref())
                .is_some()
        );
        assert_eq!(
            gui.controller.track_lyrics(id).as_deref(),
            Some("newer external lyrics")
        );
        gui.controller.shutdown();
    }

    #[test]
    fn lyrics_timestamp_editor_synced_conversion_and_follow_current() {
        let dir = TempDir::new("gui-lyrics-sync");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let first = import_named_track(
            &mut gui,
            &dir,
            "First",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            1,
        );
        let second = import_named_track(
            &mut gui,
            &dir,
            "Second",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            2,
        );
        gui.controller
            .set_lyrics(first, Some("[00:01.000]one\n[00:02.000]two"))
            .expect("synced");
        gui.controller
            .set_lyrics(second, Some("[00:01.000]other"))
            .expect("other synced");
        send(&mut gui, Message::LyricsSelect(first));
        gui.controller.play_track(first);
        send(&mut gui, Message::LyricsOpenCurrent);
        send(&mut gui, Message::LyricsEditOpen);
        send(&mut gui, Message::LyricsEditorSetCurrentTime(0));
        send(
            &mut gui,
            Message::LyricsEditorTextChanged(0, "one edited".to_owned()),
        );
        send(
            &mut gui,
            Message::LyricsEditorTimestampChanged(1, "00:02.000".to_owned()),
        );
        send(&mut gui, Message::LyricsEditorSave);
        let raw = gui.controller.track_lyrics(first).expect("saved");
        assert!(LyricsDocument::parse(&raw).expect("parse").kind() == LyricsKind::Synced);
        assert!(raw.contains("one edited"));
        assert_eq!(gui.lyrics_kind_for(first), LyricsKind::Synced);

        gui.controller
            .set_lyrics(first, Some("[00:01.000]only"))
            .expect("reset synced");
        send(&mut gui, Message::LyricsEditOpen);
        send(&mut gui, Message::LyricsEditorClearTimestamp(0));
        send(&mut gui, Message::LyricsEditorSave);
        assert!(
            gui.lyrics_editor.is_some(),
            "plain conversion needs confirmation"
        );
        send(&mut gui, Message::LyricsEditorSave);
        assert!(gui.lyrics_editor.is_none());
        assert_eq!(gui.lyrics_kind_for(first), LyricsKind::Plain);
        assert_eq!(
            gui.controller.track_lyrics(first).as_deref(),
            Some("only\n")
        );

        // Player lyrics follow explicit track changes while in follow mode.
        gui.controller.play_track(first);
        send(&mut gui, Message::LyricsOpenCurrent);
        gui.controller.play_track(second);
        send(&mut gui, Message::Tick);
        assert_eq!(gui.selected_lyrics, Some(second));
        gui.controller.shutdown();
    }

    #[test]
    fn lyrics_import_export_preview_and_overwrite_confirmation() {
        let dir = TempDir::new("gui-lyrics-io");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let id = import_named_track(
            &mut gui,
            &dir,
            "IO Track",
            "Lyrics Artist",
            "Lyrics Album",
            ReleaseType::Album,
            1,
        );
        send(&mut gui, Message::LyricsSelect(id));
        let import_path = dir.path().join("import.lrc");
        std::fs::write(&import_path, "[00:01.000]Café\n[00:02.000]second\n").expect("import file");
        send(
            &mut gui,
            Message::LyricsImportPicked(0, Ok(Some(vec![import_path.clone()]))),
        );
        assert!(gui.lyrics_import.is_some());
        assert!(
            gui.controller.track_lyrics(id).is_none(),
            "preview does not save"
        );
        send(&mut gui, Message::LyricsImportConfirm);
        assert_eq!(
            gui.lyrics_kind_for(id),
            LyricsKind::Synced,
            "import saved canonical LRC"
        );
        assert_eq!(
            std::fs::read_to_string(&import_path).expect("source preserved"),
            "[00:01.000]Café\n[00:02.000]second\n"
        );

        let export_path = dir.path().join("export.lrc");
        send(
            &mut gui,
            Message::LyricsExportPathChanged(export_path.display().to_string()),
        );
        send(&mut gui, Message::LyricsExport("lrc".to_owned()));
        assert!(export_path.is_file());
        assert!(
            std::fs::read_to_string(&export_path)
                .expect("export")
                .contains("[00:01.000]Café")
        );
        std::fs::write(&export_path, "keep me").expect("existing destination");
        send(&mut gui, Message::LyricsExport("lrc".to_owned()));
        assert_eq!(
            std::fs::read_to_string(&export_path).expect("existing"),
            "keep me",
            "first export click must not overwrite"
        );
        send(&mut gui, Message::LyricsExport("lrc".to_owned()));
        assert!(
            std::fs::read_to_string(&export_path)
                .expect("overwrite")
                .contains("Café")
        );

        // Existing lyrics require an explicit second import confirmation.
        let second_import = dir.path().join("second.txt");
        std::fs::write(&second_import, "new plain").expect("second import");
        send(
            &mut gui,
            Message::LyricsImportPicked(0, Ok(Some(vec![second_import]))),
        );
        send(&mut gui, Message::LyricsImportConfirm);
        assert!(gui.lyrics_import.is_some());
        assert_eq!(gui.lyrics_kind_for(id), LyricsKind::Synced);
        send(&mut gui, Message::LyricsImportConfirm);
        assert!(gui.lyrics_import.is_none());
        assert_eq!(gui.lyrics_kind_for(id), LyricsKind::Plain);

        let invalid = dir.path().join("invalid.lrc");
        std::fs::write(&invalid, "[00:99.00]bad").expect("invalid");
        send(
            &mut gui,
            Message::LyricsImportPicked(0, Ok(Some(vec![invalid]))),
        );
        assert!(
            gui.lyrics_import
                .as_ref()
                .and_then(|preview| preview.error.as_ref())
                .is_some()
        );
        send(&mut gui, Message::LyricsImportConfirm);
        assert!(gui.lyrics_import.is_some());
        send(&mut gui, Message::LyricsImportCancel);
        assert!(gui.lyrics_import.is_none());
        assert_eq!(
            gui.controller.track_lyrics(id).as_deref(),
            Some("new plain")
        );
        gui.controller.shutdown();
    }

    #[test]
    fn dashboard_summary_and_smart_collections_track_catalogue_changes() {
        let dir = TempDir::new("gui-dashboard");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        let three = import_named_track(
            &mut gui,
            &dir,
            "Three",
            "Artist Three",
            "Album Three",
            ReleaseType::Album,
            3,
        );
        gui.controller
            .set_favorites(&[one], true)
            .expect("favorite");
        gui.controller
            .set_track_rating(two, Some(5))
            .expect("rating");
        gui.controller
            .set_lyrics(one, Some("[00:01.000]synced"))
            .expect("synced");
        gui.controller
            .set_lyrics(two, Some("plain"))
            .expect("plain");
        let summary = gui.dashboard_summary();
        assert_eq!(summary.total_tracks, 3);
        assert_eq!(summary.total_artists, 3);
        assert_eq!(summary.total_albums, 3);
        assert_eq!(summary.favorites, 1);
        assert_eq!(summary.rated, 1);
        assert_eq!(summary.with_lyrics, 2);
        assert_eq!(summary.synced_lyrics, 1);
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::Favorites),
            vec![one]
        );
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::HighlyRated),
            vec![two]
        );
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::NoLyrics),
            vec![three]
        );
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::SyncedLyrics),
            vec![one]
        );
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::RecentlyAdded)
                .len(),
            3
        );
        gui.controller
            .set_favorites(&[three], true)
            .expect("new favorite");
        let summary = gui.dashboard_summary();
        assert_eq!(summary.favorites, 2, "dashboard cache invalidates");
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::Favorites),
            vec![one, three]
        );
        gui.controller.shutdown();
    }

    #[test]
    fn history_aggregates_drive_dashboard_collections_sorting_and_cache_invalidation() {
        let dir = TempDir::new("gui-history-aggregates");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui1 = gui(&dir);
        send(
            &mut gui1,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui1, Message::OpenLibraryPressed);
        let one = import_named_track(
            &mut gui1,
            &dir,
            "One",
            "Artist One",
            "Album One",
            ReleaseType::Album,
            1,
        );
        let two = import_named_track(
            &mut gui1,
            &dir,
            "Two",
            "Artist Two",
            "Album Two",
            ReleaseType::Album,
            2,
        );
        gui1.controller.shutdown();
        std::fs::write(
            root.join("history.flux"),
            format!(
                "{{\"version\":1,\"kind\":\"flux-playback-history\",\"entries\":[\
                 {{\"track_id\":{},\"play_count\":2,\"last_played_unix_ms\":2000}},\
                 {{\"track_id\":{},\"play_count\":1,\"last_played_unix_ms\":3000}}]}}",
                one.get(),
                two.get(),
            ),
        )
        .expect("fixture history");

        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let summary = gui.dashboard_summary();
        assert_eq!(summary.recently_played, vec![two, one]);
        assert_eq!(summary.most_played, vec![one, two]);
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::RecentlyPlayed),
            vec![two, one]
        );
        assert_eq!(
            gui.smart_collection_ids(SmartCollection::MostPlayed),
            vec![one, two]
        );
        let (artists, albums) = history_entity_breakdown(&gui.controller);
        assert_eq!(
            artists,
            vec![("Artist One".to_owned(), 2), ("Artist Two".to_owned(), 1)]
        );
        assert_eq!(
            albums,
            vec![("Album One".to_owned(), 2), ("Album Two".to_owned(), 1)]
        );

        gui.sort_tracks = TrackSort::PlayCount;
        gui.sort_descending = false;
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![two, one]
        );
        gui.sort_descending = true;
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![one, two]
        );

        let mut stale_summary = summary.clone();
        stale_summary.history_generation = stale_summary.history_generation.wrapping_add(1);
        stale_summary.total_tracks = 999;
        *gui.dashboard_cache.borrow_mut() = Some(stale_summary);
        let refreshed = gui.dashboard_summary();
        assert_eq!(
            refreshed.total_tracks, 2,
            "dashboard cache ignored history generation"
        );
        assert_eq!(
            refreshed.history_generation,
            gui.controller.history_generation()
        );
        gui.controller.shutdown();
    }

    #[test]
    fn rating_filters_favorite_controls_and_advanced_sorting_are_stable() {
        let dir = TempDir::new("gui-ratings-sort");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("create canonical");
        let mut gui = gui(&dir);
        send(
            &mut gui,
            Message::LibInputChanged(root.display().to_string()),
        );
        send(&mut gui, Message::OpenLibraryPressed);
        let low = import_named_track(
            &mut gui,
            &dir,
            "Low",
            "Artist Low",
            "Album Low",
            ReleaseType::Album,
            1,
        );
        let high = import_named_track(
            &mut gui,
            &dir,
            "High",
            "Artist High",
            "Album High",
            ReleaseType::Album,
            2,
        );
        let none = import_named_track(
            &mut gui,
            &dir,
            "None",
            "Artist None",
            "Album None",
            ReleaseType::Album,
            3,
        );
        gui.controller.set_track_rating(low, Some(1)).expect("low");
        gui.controller
            .set_track_rating(high, Some(5))
            .expect("high");
        gui.controller
            .set_favorites(&[high], true)
            .expect("favorite");
        send(&mut gui, Message::SortTracksChanged(TrackSort::Rating));
        send(&mut gui, Message::SortDirectionToggled);
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![high, low, none]
        );
        send(&mut gui, Message::FilterRatingChanged(RatingFilter::Four));
        assert_eq!(
            gui.filtered_tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![high]
        );
        send(&mut gui, Message::ResetLibraryFilters);
        send(&mut gui, Message::TrackFavoriteToggle(low));
        assert!(
            gui.controller
                .rating(low)
                .is_some_and(|rating| rating.favorite)
        );
        send(&mut gui, Message::TrackRatingSet(low, Some(4)));
        assert_eq!(
            gui.controller.rating(low).and_then(|rating| rating.rating),
            Some(4)
        );
        assert!(
            gui.controller
                .rating(low)
                .is_some_and(|rating| rating.favorite)
        );
        send(&mut gui, Message::TrackSelectionToggle(high));
        send(&mut gui, Message::TrackSelectionToggle(none));
        send(&mut gui, Message::BatchRating(Some(3)));
        assert_eq!(
            gui.controller.rating(high).and_then(|rating| rating.rating),
            Some(3)
        );
        assert_eq!(
            gui.controller.rating(none).and_then(|rating| rating.rating),
            Some(3)
        );
        assert!(
            gui.controller
                .rating(high)
                .is_some_and(|rating| rating.favorite)
        );
        gui.controller.shutdown();
    }
}
