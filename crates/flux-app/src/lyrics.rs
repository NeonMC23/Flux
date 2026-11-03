//! Plain-text and synchronized LRC lyrics handling.
//!
//! Lyrics remain ordinary UTF-8 text in the existing canonical managed lyrics
//! file. The parser never treats audio tags as lyrics and never changes track
//! metadata from LRC metadata tags.

use std::fmt;

/// One line of lyrics. A line may be untimed (plain text) or synchronized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    /// Line text, preserved exactly apart from a stripped line ending.
    pub text: String,
    /// Synchronized timestamp in milliseconds.
    pub timestamp_ms: Option<u64>,
}

impl LyricLine {
    /// Creates a plain line.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            timestamp_ms: None,
        }
    }

    /// Creates a synchronized line.
    #[must_use]
    pub fn timed(text: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            text: text.into(),
            timestamp_ms: Some(timestamp_ms),
        }
    }
}

/// A parsed lyrics document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricsDocument {
    /// LRC metadata entries, preserved in source order.
    pub metadata: Vec<(String, String)>,
    /// Lines in source order.
    pub lines: Vec<LyricLine>,
}

/// Lyrics classification for browsing and editor warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsKind {
    /// No lines or only untimed lines.
    Plain,
    /// At least one timed line.
    Synced,
}

/// One synchronized event used for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLyricLine {
    /// Effective timestamp after applying the LRC offset.
    pub time_ms: u64,
    /// Line text.
    pub text: String,
    /// Source line index.
    pub line_index: usize,
}

impl LyricsDocument {
    /// Parses plain text or LRC. Malformed timestamp syntax is an error rather
    /// than being silently downgraded to plain text.
    pub fn parse(input: &str) -> Result<Self, LyricsError> {
        let mut document = Self::default();
        for (line_number, raw) in input.lines().enumerate() {
            let raw = raw.trim_end_matches('\r');
            if raw.is_empty() {
                document.lines.push(LyricLine::plain(""));
                continue;
            }
            let mut rest = raw;
            let mut timestamps = Vec::new();
            while rest.starts_with('[') {
                match parse_timestamp_prefix(rest) {
                    Ok(Some((timestamp, next))) => {
                        timestamps.push(timestamp);
                        rest = next;
                    }
                    Ok(None) => break,
                    Err(message) => {
                        return Err(LyricsError::new(format!(
                            "line {}: {message}",
                            line_number + 1
                        )));
                    }
                }
            }
            if timestamps.is_empty() {
                if let Some((key, value)) = parse_metadata(raw) {
                    document.metadata.push((key, value));
                } else if raw.starts_with('[') && looks_like_timestamp(raw) {
                    return Err(LyricsError::new(format!(
                        "line {}: malformed timestamp",
                        line_number + 1
                    )));
                } else {
                    document.lines.push(LyricLine::plain(raw));
                }
                continue;
            }
            for timestamp in timestamps {
                document.lines.push(LyricLine::timed(rest, timestamp));
            }
        }
        if document
            .metadata
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("offset"))
        {
            document.offset_ms_value()?;
        }
        Ok(document)
    }

    /// Whether this is plain or synchronized lyrics.
    #[must_use]
    pub fn kind(&self) -> LyricsKind {
        if self.lines.iter().any(|line| line.timestamp_ms.is_some()) {
            LyricsKind::Synced
        } else {
            LyricsKind::Plain
        }
    }

    /// LRC offset in milliseconds. Invalid values are reported by
    /// [`parse`](Self::parse), so this returns zero for a manually constructed
    /// invalid document.
    #[must_use]
    pub fn offset_ms(&self) -> i64 {
        self.offset_ms_value().unwrap_or(0)
    }

    fn offset_ms_value(&self) -> Result<i64, LyricsError> {
        let mut offset = 0i64;
        for (key, value) in &self.metadata {
            if key.eq_ignore_ascii_case("offset") {
                offset = value.trim().parse::<i64>().map_err(|_| {
                    LyricsError::new("LRC offset must be a signed millisecond integer")
                })?;
            }
        }
        Ok(offset)
    }

    /// Effective synchronized events, sorted by timestamp and then source line.
    #[must_use]
    pub fn sync_events(&self) -> Vec<SyncLyricLine> {
        let offset = self.offset_ms();
        let mut events = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(line_index, line)| {
                line.timestamp_ms.map(|timestamp| {
                    let adjusted = i128::from(timestamp) + i128::from(offset);
                    let time_ms = adjusted.clamp(0, i128::from(u64::MAX)) as u64;
                    SyncLyricLine {
                        time_ms,
                        text: line.text.clone(),
                        line_index,
                    }
                })
            })
            .collect::<Vec<_>>();
        events.sort_by(|left, right| {
            left.time_ms
                .cmp(&right.time_ms)
                .then_with(|| left.line_index.cmp(&right.line_index))
        });
        events
    }

    /// Index of the current event for a playback position, if any.
    #[must_use]
    pub fn current_event_index(&self, position_ms: u64) -> Option<usize> {
        let events = self.sync_events();
        let index = events.partition_point(|event| event.time_ms <= position_ms);
        if index == 0 { None } else { Some(index - 1) }
    }

    /// Serializes as LRC. Timestamps use millisecond precision.
    #[must_use]
    pub fn to_lrc(&self) -> String {
        let mut output = String::new();
        for (key, value) in &self.metadata {
            output.push('[');
            output.push_str(key);
            output.push(':');
            output.push_str(value);
            output.push_str("]\n");
        }
        for line in &self.lines {
            if let Some(timestamp) = line.timestamp_ms {
                output.push_str(&format_timestamp(timestamp));
            }
            output.push_str(&line.text);
            output.push('\n');
        }
        output
    }

    /// Serializes only the line text, preserving order and blank lines.
    #[must_use]
    pub fn to_plain_text(&self) -> String {
        let mut output = String::new();
        for line in &self.lines {
            output.push_str(&line.text);
            output.push('\n');
        }
        output
    }

    /// Classifies raw managed lyrics text for browsing.
    pub fn classify(input: &str) -> LyricsKind {
        Self::parse(input).map_or(LyricsKind::Plain, |document| document.kind())
    }
}

/// Lyrics parsing/validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsError {
    message: String,
}

impl LyricsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LyricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LyricsError {}

fn parse_timestamp_prefix(input: &str) -> Result<Option<(u64, &str)>, String> {
    let Some(close) = input.find(']') else {
        if looks_like_timestamp(input) {
            return Err("timestamp is missing its closing bracket".to_owned());
        }
        return Ok(None);
    };
    let inside = &input[1..close];
    if !inside.contains(':') || !inside.starts_with(|character: char| character.is_ascii_digit()) {
        return Ok(None);
    }
    let (minutes_text, rest) = inside
        .split_once(':')
        .ok_or_else(|| "timestamp must contain minutes and seconds".to_owned())?;
    if minutes_text.is_empty() || !minutes_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("timestamp minutes must be digits".to_owned());
    }
    let (seconds_text, fraction) = match rest.split_once('.') {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (rest, None),
    };
    if seconds_text.len() != 2 || !seconds_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("timestamp seconds must be two digits".to_owned());
    }
    let minutes = minutes_text
        .parse::<u64>()
        .map_err(|_| "timestamp minutes overflow".to_owned())?;
    let seconds = seconds_text
        .parse::<u64>()
        .map_err(|_| "timestamp seconds overflow".to_owned())?;
    if seconds >= 60 {
        return Err("timestamp seconds must be less than 60".to_owned());
    }
    let fraction = fraction.ok_or_else(|| "timestamp needs a fractional part".to_owned())?;
    if fraction.is_empty()
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("timestamp fraction must contain one to three digits".to_owned());
    }
    let scale = 10u64.pow(3 - fraction.len() as u32);
    let millis = fraction
        .parse::<u64>()
        .map_err(|_| "timestamp fraction overflow".to_owned())?
        * scale;
    let total = minutes
        .checked_mul(60)
        .and_then(|value| value.checked_add(seconds))
        .and_then(|value| value.checked_mul(1000))
        .and_then(|value| value.checked_add(millis))
        .ok_or_else(|| "timestamp overflow".to_owned())?;
    Ok(Some((total, &input[close + 1..])))
}

fn looks_like_timestamp(input: &str) -> bool {
    let Some(close) = input.find(']') else {
        return input.starts_with('[')
            && input[1..]
                .bytes()
                .take_while(|byte| *byte != b']')
                .any(|byte| byte == b':');
    };
    let inside = &input[1..close];
    inside.contains(':') && inside.starts_with(|character: char| character.is_ascii_digit())
}

fn parse_metadata(input: &str) -> Option<(String, String)> {
    if !input.starts_with('[') || !input.ends_with(']') {
        return None;
    }
    let inside = &input[1..input.len() - 1];
    let (key, value) = inside.split_once(':')?;
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some((key.to_owned(), value.to_owned()))
}

fn format_timestamp(timestamp_ms: u64) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms / 1000) % 60;
    let millis = timestamp_ms % 1000;
    format!("[{minutes:02}:{seconds:02}.{millis:03}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_unicode_blank_and_untimed_lines() {
        let document =
            LyricsDocument::parse("First line\r\n\r\nCafé 世界\nlast").expect("plain lyrics");
        assert_eq!(document.kind(), LyricsKind::Plain);
        assert_eq!(document.lines.len(), 4);
        assert_eq!(document.lines[0].text, "First line");
        assert_eq!(document.lines[1].text, "");
        assert_eq!(document.lines[2].text, "Café 世界");
        assert_eq!(document.to_plain_text(), "First line\n\nCafé 世界\nlast\n");
    }

    #[test]
    fn parses_lrc_multiple_timestamps_duplicates_and_metadata() {
        let input = "[ti:Song]\n[ar:Artist]\n[offset:250]\n[00:10.50][00:20.500]Chorus\n[00:10.50]Again\nplain tail\n";
        let document = LyricsDocument::parse(input).expect("lrc");
        assert_eq!(document.kind(), LyricsKind::Synced);
        assert_eq!(document.metadata.len(), 3);
        assert_eq!(document.offset_ms(), 250);
        assert_eq!(
            document.lines,
            vec![
                LyricLine::timed("Chorus", 10_500),
                LyricLine::timed("Chorus", 20_500),
                LyricLine::timed("Again", 10_500),
                LyricLine::plain("plain tail"),
            ]
        );
        let events = document.sync_events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].time_ms, 10_750);
        assert_eq!(events[1].time_ms, 10_750);
        assert_eq!(events[2].time_ms, 20_750);
    }

    #[test]
    fn round_trips_lrc_and_plain_text() {
        let input = "[ti:Title]\n[offset:-100]\n[00:01.005]Café\n\n[00:02.000]世界\n";
        let document = LyricsDocument::parse(input).expect("lrc");
        let serialized = document.to_lrc();
        let reparsed = LyricsDocument::parse(&serialized).expect("reserialized lrc");
        assert_eq!(reparsed.lines, document.lines);
        assert_eq!(reparsed.metadata, document.metadata);
        assert_eq!(document.to_plain_text(), "Café\n\n世界\n");
    }

    #[test]
    fn rejects_malformed_timestamps_instead_of_downgrading() {
        for input in [
            "[00:99.00]bad seconds",
            "[00:10]missing fraction",
            "[00:10.0000]too precise",
            "[00:1.00]short seconds",
            "[00:10.00missing bracket",
        ] {
            assert!(LyricsDocument::parse(input).is_err(), "{input}");
        }
        assert!(LyricsDocument::parse("[00:99.00]x").is_err());
    }

    #[test]
    fn current_event_handles_seek_boundaries() {
        let document = LyricsDocument::parse(
            "[00:01.000]one\n[00:02.000]two\n[00:02.000]two-b\n[00:03.000]three\n",
        )
        .expect("lrc");
        assert_eq!(document.current_event_index(0), None);
        assert_eq!(document.current_event_index(999), None);
        assert_eq!(document.current_event_index(1_000), Some(0));
        assert_eq!(document.current_event_index(2_500), Some(2));
        assert_eq!(document.current_event_index(99_000), Some(3));
    }

    #[test]
    fn unknown_metadata_is_preserved_and_not_track_authority() {
        let document =
            LyricsDocument::parse("[custom:value:with:colons]\n[00:00.00]text").expect("lrc");
        assert_eq!(
            document.metadata,
            vec![("custom".to_owned(), "value:with:colons".to_owned())]
        );
        assert_eq!(document.lines[0].text, "text");
    }

    #[test]
    fn negative_offset_clamps_to_zero_for_display() {
        let document = LyricsDocument::parse("[offset:-5000]\n[00:01.00]early").expect("lrc");
        assert_eq!(document.sync_events()[0].time_ms, 0);
        assert_eq!(document.current_event_index(0), Some(0));
    }
}
