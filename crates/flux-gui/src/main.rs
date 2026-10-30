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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flux_app::{
    Artist, AudioStatus, Availability, CandidateStatus, CanonicalMigrationPlan, Controller,
    ImportApplyReport, ImportDecision, ImportItemState, ImportPlan, ImportProgress, ImportRequest,
    ImportStage, ImportStorage, PreparedImport, ReconcileReport, RelPath, StorageMode,
    TrackMetadataEdit, WAVEFORM_WINDOW, format_duration, is_track_playable, prepare_import_files,
    prepare_import_plan,
};
use flux_core::{
    APP_NAME, ArtistId, PlaybackState, QueueEntry, QueueEntryId, RepeatMode, Track, TrackId,
};
use iced::advanced::Widget;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as RendererTrait};
use iced::advanced::widget::Tree;
use iced::keyboard::{Key, Modifiers, key::Named};
use iced::widget::{
    button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, slider, stack,
    text, text_input, tooltip,
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
const OSC_GLOW: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.16);
const OSC_GUIDE: Color = Color::from_rgba8(0xff, 0x66, 0xcc, 0.12);

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
    let entries = match std::fs::read_dir(&browser.current) {
        Ok(entries) => entries,
        Err(err) => {
            browser.entries.clear();
            browser.error = Some(format!("Could not read this folder: {err}"));
            return;
        }
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
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
            "A different audio file shares title/artist/album metadata. That alone is not treated as the same track."
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
        LibraryView::Tracks => key.to_owned(),
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
}

#[derive(Debug)]
struct ImportManager {
    stage: ImportUiStage,
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
    edit_album: String,
    edit_track_number: String,
    edit_disc_number: String,
    number_error: Option<String>,
}

impl ImportManager {
    fn new() -> Self {
        Self {
            stage: ImportUiStage::Choose,
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
            edit_album: String::new(),
            edit_track_number: String::new(),
            edit_disc_number: String::new(),
            number_error: None,
        }
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
        self.number_error = None;
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
        self.edit_album = candidate.album().unwrap_or_default().to_owned();
        self.edit_track_number = candidate
            .track_number()
            .map(|number| number.to_string())
            .unwrap_or_default();
        self.edit_disc_number = candidate
            .disc_number()
            .map(|number| number.to_string())
            .unwrap_or_default();
        self.number_error = None;
    }
}

struct FluxReady {
    controller: Controller,
    search: String,
    lib_input: String,
    track_editor: Option<TrackEditor>,
    import_manager: Option<ImportManager>,
    external_drag_hover: bool,
    artist_selected: Option<ArtistId>,
    artist_name_input: String,
    artist_rename_input: String,
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
    show_setup: bool,
    queue_visible: bool,
    window_width: f32,
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
    EditorCancel,
    ArtistSelected(Option<ArtistChoice>),
    ArtistNameChanged(String),
    ArtistCreatePressed,
    ArtistRenameNameChanged(String),
    ArtistRenamePressed,
    RelinkTrack(TrackId, RelPath),
    ImportOpen,
    ImportOpenSetup,
    ImportClose,
    ImportReset,
    ImportNoop,
    ImportNativeFiles,
    ImportNativeFolder,
    ImportNativeFilesPicked(Result<Option<Vec<PathBuf>>, String>),
    ImportNativeFolderPicked(Result<Option<PathBuf>, String>),
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
    ImportPlanReady(Box<ImportPlan>),
    ImportPrepared(Box<PreparedImport>),
    ImportSelect(usize),
    ImportDecisionChanged(usize, ImportDecision),
    ImportBulkSkip,
    ImportBulkImport,
    ImportBulkUpdate,
    ImportTitleChanged(String),
    ImportArtistChanged(String),
    ImportAlbumChanged(String),
    ImportTrackNumberChanged(String),
    ImportDiscNumberChanged(String),
    ImportArtistPicked(Option<ArtistChoice>),
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
    SeekPreview(f32),
    SeekCommit,
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
            search: String::new(),
            lib_input: String::new(),
            track_editor: None,
            import_manager: None,
            external_drag_hover: false,
            artist_selected: None,
            artist_name_input: String::new(),
            artist_rename_input: String::new(),
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
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.lifecycle_msg = None;
        self.queue_drag = None;
        self.seek_preview = None;
        self.track_editor = None;
        self.artist_selected = None;
        self.artist_name_input.clear();
        self.artist_rename_input.clear();
        self.last_reconcile = None;
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
            self.seek_preview = None;
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

    fn start_track_editor(&mut self, id: TrackId) {
        let Some(track) = self.controller.track(id) else {
            return;
        };
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
        });
        self.show_setup = false;
    }

    fn save_track_editor(&mut self) {
        let Some(editor) = self.track_editor.as_ref() else {
            return;
        };
        let id = editor.id;
        let edit = match (
            parse_optional_number(&editor.track_number, "track number"),
            parse_optional_number(&editor.disc_number, "disc number"),
        ) {
            (Ok(track_number), Ok(disc_number)) => TrackMetadataEdit {
                title: nonempty_string(&editor.title),
                artist_id: editor.artist_id,
                album: nonempty_string(&editor.album),
                track_number,
                disc_number,
            },
            (Err(error), _) | (_, Err(error)) => {
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.error = Some(error);
                }
                return;
            }
        };
        match self.controller.edit_track_metadata(id, edit) {
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
                if let Some(editor) = self.track_editor.as_mut() {
                    editor.artist_id = Some(id);
                    editor.new_artist.clear();
                    editor.error = None;
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
        self.show_setup = false;
        self.track_editor = None;
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
        };
        manager.reset_for_selection();
        manager.stage = ImportUiStage::Scanning;
        let progress = Arc::clone(&manager.progress);
        let cancel = Arc::clone(&manager.cancel);
        Task::perform(
            async move { prepare_import_plan(request, progress, cancel) },
            |plan| Message::ImportPlanReady(Box::new(plan)),
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
                    .unwrap_or("There is nothing selected to import.")
                    .to_owned(),
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
        Task::perform(
            async move { prepare_import_files(&plan, progress, cancel) },
            |prepared| Message::ImportPrepared(Box::new(prepared)),
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
                manager.report = Some(report);
                manager.prepared = Some(prepared);
                manager.stage = ImportUiStage::Completed;
                manager.progress_view.stage = ImportStage::Completed;
                manager.progress_view.message = "Import complete.".to_owned();
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

    fn native_files_task(&mut self) -> Task<Message> {
        let start = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
            .or_else(home_directory);
        self.open_import_manager();
        Task::perform(
            async move { flux_platform::pick_audio_files_in(start.as_deref()) },
            Message::ImportNativeFilesPicked,
        )
    }

    fn native_folder_task(&mut self) -> Task<Message> {
        let start = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .map(Path::to_path_buf)
            .or_else(home_directory);
        self.open_import_manager();
        Task::perform(
            async move { flux_platform::pick_folder_in(start.as_deref()) },
            Message::ImportNativeFolderPicked,
        )
    }

    fn open_import_browser(&mut self, mode: BrowserMode) {
        self.open_import_manager();
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
            Message::EditorCancel => self.track_editor = None,
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
                                self.lifecycle_msg =
                                    Some(format!("Renamed artist {id} to {name}."));
                            }
                            Err(error) => self.lifecycle_msg = Some(error),
                        }
                    }
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
                self.open_import_manager();
                if let Some(manager) = self.import_manager.as_mut() {
                    let recursive = manager.recursive;
                    let storage = manager.storage;
                    *manager = ImportManager::new();
                    manager.recursive = recursive;
                    let _ = storage;
                    manager.storage = ImportStorage::Copy;
                }
            }
            Message::ImportNoop => {}
            Message::ImportNativeFiles => return self.native_files_task(),
            Message::ImportNativeFolder => return self.native_folder_task(),
            Message::ImportNativeFilesPicked(result) => match result {
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
            Message::ImportNativeFolderPicked(result) => match result {
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
            Message::ImportPlanReady(plan) => {
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
            Message::ImportPrepared(prepared) => self.apply_import_review(*prepared),
            Message::ImportSelect(index) => {
                if let Some(manager) = self.import_manager.as_mut() {
                    manager.select_candidate(index);
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
                    }
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
            Message::QueueDragRelease => {
                self.finish_queue_drag();
                self.commit_seek_preview();
            }
            Message::TogglePlayPause => self.controller.toggle_play_pause(),
            Message::Stop => {
                self.seek_preview = None;
                self.controller.stop();
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
                    Key::Character("l") => self.toggle_setup(),
                    Key::Named(Named::Escape) => {
                        if self.track_editor.take().is_some() {
                            return Task::none();
                        }
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

        let base: Element<'_, Message> = body.into();
        let body: Element<'_, Message> = if self.import_manager.is_some() {
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
            .portable_root()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "No portable library is active".to_owned());
        let mut body = column![].spacing(10);
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
            CandidateStatus::ExistingPath(_) => {
                ("Already in library", Color::from_rgb8(0xff, 0xc2, 0x66))
            }
            CandidateStatus::SameContent { .. } => {
                ("Possible same content", Color::from_rgb8(0xff, 0xc2, 0x66))
            }
            CandidateStatus::SimilarMetadata(_) => ("Similar metadata", MUTED),
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
        button(title)
            .width(Length::Fill)
            .padding([7, 8])
            .style(track_row_style(selected))
            .on_press(Message::ImportSelect(index))
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
        match candidate.status() {
            CandidateStatus::ExistingPath(id) => {
                body = body.push(
                    text(format!(
                        "This exact path is track {id}. Update its Flux overrides to keep the \
                         same TrackId and queue entries."
                    ))
                    .size(9)
                    .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                        button(text("Update existing").size(10))
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
            CandidateStatus::New | CandidateStatus::SimilarMetadata(_) => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Import separately").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::ImportNew))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::ImportNew,
                            )),
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                    ]
                    .spacing(6),
                );
            }
            CandidateStatus::SameContent { .. } => {
                body = body.push(
                    text(status_description(candidate.status()))
                        .size(9)
                        .color(MUTED),
                );
                body = body.push(
                    row![
                        button(text("Import separately").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::ImportNew))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(
                                index,
                                ImportDecision::ImportNew,
                            )),
                        button(text("Skip").size(10))
                            .style(nav_style(candidate.decision() == ImportDecision::Skip))
                            .padding([6, 9])
                            .on_press(Message::ImportDecisionChanged(index, ImportDecision::Skip)),
                    ]
                    .spacing(6),
                );
            }
        }

        let editable = manager.stage == ImportUiStage::Review;
        body = body.push(text("TITLE").size(8).color(MUTED));
        body = body.push(
            text_input("Title", &manager.edit_title)
                .on_input(Message::ImportTitleChanged)
                .style(input_style)
                .width(Length::Fill),
        );
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
        body = body.push(text("ALBUM").size(8).color(MUTED));
        body = body.push(
            text_input("Album", &manager.edit_album)
                .on_input(Message::ImportAlbumChanged)
                .style(input_style)
                .width(Length::Fill),
        );
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
                "Metadata was inferred only from filenames and folders. Review edits become \
                 canonical Flux metadata; audio tags are never read or written. Artwork \
                 extraction/preview is not part of this build.",
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
                body = body.push(
                    row![
                        text(state).size(9).color(color).width(Length::Fixed(56.0)),
                        text(match item.track_id {
                            Some(id) => format!("Track {id}"),
                            None => item.message.clone(),
                        })
                        .size(10)
                        .color(MUTED)
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

    fn nav_counts(&self) -> (usize, usize, usize, usize) {
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
            button(text("Import music").size(12))
                .width(Length::Fill)
                .padding([8, 10])
                .style(accent_button_style)
                .on_press(Message::ImportOpen),
        );
        nav = nav.push(iced::widget::Space::new().height(6));
        nav = nav.push(
            button(text("Library setup").size(12))
                .width(Length::Fill)
                .padding([8, 10])
                .style(nav_style(self.show_setup))
                .on_press(Message::ToggleSetup),
        );
        nav = nav.push(iced::widget::Space::new().height(Length::Fill));
        nav = nav.push(text("SPACE PLAY · N/P SKIP").size(8).color(MUTED));
        nav = nav.push(text("←/→ SEEK · SHIFT ±30S").size(8).color(MUTED));
        nav = nav.push(text("1-4 VIEWS · L SETUP · / SEARCH").size(8).color(MUTED));
        nav = nav.push(iced::widget::Space::new().height(10));

        let library_state = match self.controller.storage_mode() {
            StorageMode::Legacy => "App index",
            StorageMode::Portable => "Portable library (migration source)",
            StorageMode::Canonical => "Canonical library",
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
        } else if let Some(editor) = &self.track_editor {
            (
                "Edit track metadata".to_owned(),
                format!(
                    "Track {} · Flux stores canonical metadata; the audio file and its tags stay untouched.",
                    editor.id
                ),
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

        if !self.show_setup && self.track_editor.is_none() {
            bar = bar.push(
                text_input("Search title, artist, album…", &self.search)
                    .id(iced::widget::Id::new("flux-search"))
                    .on_input(Message::SearchChanged)
                    .style(input_style)
                    .width(Length::Fixed(250.0)),
            );
        }
        if self.window_width >= 1_080.0 && self.track_editor.is_none() {
            bar = bar.push(
                button(text("Queue").size(11))
                    .style(nav_style(self.queue_visible))
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

        Some(
            container(line)
                .padding([10, 12])
                .width(Length::Fill)
                .style(error_banner_style)
                .into(),
        )
    }

    fn library_view(&self) -> Element<'_, Message> {
        if self.track_editor.is_some() {
            return self.track_editor_view();
        }
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
        let missing = matches!(
            self.controller.track_availability(editor.id),
            Some(Availability::Missing)
        );

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
                text("This file is currently missing. Metadata can still be edited; playback will fail until it is relinked or restored.")
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
        body = body.push(
            text_input("Album…", &editor.album)
                .on_input(Message::EditorAlbumChanged)
                .style(input_style)
                .width(Length::Fill),
        );
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
        body = body.push(text("Artist identity").size(9).color(MUTED));
        body = body.push(artist_row);
        body = body.push(new_artist_row);
        body = body.push(
            text("Clear album or number fields to fall back to source metadata. The title always has a filename fallback.")
                .size(9)
                .color(MUTED),
        );
        if let Some(error) = &editor.error {
            body = body.push(text(error).size(10).color(DANGER));
        }
        body = body.push(
            row![
                button(text("Save metadata").size(12))
                    .style(accent_button_style)
                    .padding([8, 14])
                    .on_press(Message::EditorSave),
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

    fn group_key_for(&self, view: LibraryView, id: TrackId, track: &Track) -> Option<String> {
        match view {
            LibraryView::Artists => {
                Some(artist_group_key(self.controller.track_artist_id(id), track))
            }
            LibraryView::Albums => Some(album_group_key(self.controller.track_album_id(id), track)),
            _ => group_key(view, track),
        }
    }

    fn groups_for(&self, view: LibraryView) -> Vec<(String, usize)> {
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        for (id, track) in self.controller.tracks() {
            if let Some(key) = self.group_key_for(view, *id, track) {
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
                    format!("{count} track(s) · no stable album")
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
            rows.retain(|(id, track)| {
                self.group_key_for(self.view, *id, track).as_deref() == Some(group.as_str())
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
        let editable = matches!(
            self.controller.storage_mode(),
            StorageMode::Portable | StorageMode::Canonical
        );

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
            text("EDIT").size(9).color(MUTED).width(Length::Fixed(36.0)),
        ]
        .spacing(8)
        .padding([2, 10]);

        let mut list = column![].spacing(1);
        for (offset, (id, track)) in rows[start..end].iter().enumerate() {
            let missing = matches!(
                self.controller.track_availability(*id),
                Some(Availability::Missing)
            );
            list = list.push(track_row(
                start + offset + 1,
                *id,
                track,
                current,
                missing,
                editable,
            ));
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
        let audio_ready = self.controller.audio_status().is_ready();
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
        let can_seek = audio_ready && playable && total.is_some_and(|d| !d.is_zero());

        let transport_enabled = audio_ready && current.is_some();
        let play_enabled = audio_ready && playable;
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
            repeat_button(self.controller.repeat_mode()),
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
            let (_, _, color) = audio_summary(self.controller.audio_status());
            info = info.push(
                text("Audio unavailable · see Library setup")
                    .size(9)
                    .color(color),
            );
        }
        let compact = self.window_width < 1_080.0;
        let left = container(
            row![artwork(current, 54.0), info,]
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

fn track_row<'a>(
    index: usize,
    id: TrackId,
    track: &'a Track,
    current: Option<TrackId>,
    missing: bool,
    editable: bool,
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

    let mut title = row![text(track.title()).size(13).color(title_color)]
        .spacing(8)
        .align_y(Alignment::Center);
    if missing {
        title = title.push(text("missing").size(9).color(DANGER));
    }
    if !playable {
        title = title.push(text("indexed, not playable").size(9).color(MUTED));
    }

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
    let edit = button(text("✎").size(11))
        .style(ghost_style)
        .padding([5, 7])
        .on_press_maybe(editable.then_some(Message::EditTrack(id)));
    let edit = tooltip(
        edit,
        container(
            text(if editable {
                "Edit metadata"
            } else {
                "Use a portable library to edit"
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

    row![main, add, next, edit]
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

fn repeat_button<'a>(mode: RepeatMode) -> Element<'a, Message> {
    let icon = if mode == RepeatMode::One {
        "↻1"
    } else {
        "↻"
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
        .style(nav_style(mode != RepeatMode::Off))
        .on_press(Message::CycleRepeat);
    tooltip(
        button,
        container(text(repeat_label(mode)).size(10).color(TEXT))
            .padding([4, 7])
            .style(tooltip_style),
        tooltip::Position::Top,
    )
    .gap(6)
    .delay(Duration::from_millis(350))
    .into()
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "Repeat: off",
        RepeatMode::All => "Repeat: all",
        RepeatMode::One => "Repeat: one",
    }
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
            lib_input: String::new(),
            track_editor: None,
            import_manager: None,
            external_drag_hover: false,
            artist_selected: None,
            artist_name_input: String::new(),
            artist_rename_input: String::new(),
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
        };
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = prepare_import_plan(request, progress, cancel);
        send(&mut gui, Message::ImportOpen);
        send(&mut gui, Message::ImportPlanReady(Box::new(plan)));
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
        let plan = gui
            .import_manager
            .as_ref()
            .and_then(|manager| manager.plan.clone())
            .expect("plan");
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let prepared = prepare_import_files(&plan, progress, cancel);
        send(&mut gui, Message::ImportPrepared(Box::new(prepared)));
        let manager = gui.import_manager.as_ref().expect("manager");
        assert_eq!(manager.stage, ImportUiStage::Completed);
        let track = gui.controller.tracks()[0].1.clone();
        assert_eq!(track.title(), "GUI imported title");
        assert_eq!(track.artist(), Some("GUI import artist"));
        assert_eq!(track.album(), Some("GUI import album"));
        assert_eq!(gui.controller.queue_len(), 1);
        assert_eq!(std::fs::read(&source).expect("source"), source_bytes);
        assert_ne!(track.source().path(), source);
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
}
