//! Width-aware text and display helpers for the terminal interface.

use flux_core::Track;
use unicode_width::UnicodeWidthChar;

pub(crate) fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

pub(crate) fn sentence_case(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), characters.as_str()),
        None => String::new(),
    }
}

pub(crate) fn display_album(track: &Track) -> String {
    track.album().unwrap_or("Standalone").to_owned()
}

pub(crate) fn display_title(track: &Track) -> String {
    match track.artist() {
        Some(artist) => format!("{} — {artist}", track.title()),
        None => track.title().to_owned(),
    }
}

pub(crate) fn format_lyrics_timestamp(timestamp_ms: u64) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms / 1000) % 60;
    let millis = timestamp_ms % 1000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

pub(crate) fn resource_label(state: Option<flux_app::ResourceState>) -> Option<&'static str> {
    match state {
        Some(flux_app::ResourceState::Missing) => Some("[missing]"),
        Some(flux_app::ResourceState::Unavailable(_)) => Some("[inaccessible]"),
        Some(flux_app::ResourceState::Present) | None => None,
    }
}

pub(crate) fn terminal_too_small(width: u16, height: u16) -> bool {
    width < 20 || height < 6
}

pub(crate) fn clamp_terminal_size(width: u16, height: u16) -> (usize, usize) {
    (usize::from(width.max(20)), usize::from(height.max(6)))
}

pub(crate) fn detail_view_fits(height: usize) -> bool {
    height >= 8
}

pub(crate) fn char_width(character: char) -> usize {
    character.width().unwrap_or(0)
}

pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

pub(crate) fn truncate_to_width(text: &str, width: usize) -> String {
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

pub(crate) fn fit_input_line(prefix: &str, input: &str, width: usize) -> String {
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

pub(crate) fn pad_with_right(left: &str, right: &str, width: usize) -> String {
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
