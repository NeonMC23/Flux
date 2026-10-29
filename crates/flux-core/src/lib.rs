//! Shared domain foundation for Flux.
//!
//! This crate is the home of domain types and application-level logic
//! shared by the GUI and TUI. It must stay independent of any user
//! interface, filesystem, database, audio-engine, or network
//! implementation: everything here is plain data and pure logic that can
//! be tested without hardware or I/O.
//!
//! Milestone M1 adds the backend-independent domain model:
//!
//! - [`track`]: track identity ([`TrackId`]), sources, and metadata.
//! - [`queue`]: the play queue, [`RepeatMode`], and [`PlaybackState`].
//! - [`error`]: the small shared [`Error`] / [`Result`] convention.
//!
//! There is still no library scanning, persistence, provider, or audio
//! output behavior anywhere in this crate.

pub mod artist;
pub mod error;
pub mod queue;
pub mod track;

pub use artist::ArtistId;
pub use error::{Error, Result};
pub use queue::{PlaybackState, Queue, QueueEntry, QueueEntryId, RepeatMode};
pub use track::{Track, TrackId, TrackSource};

/// Product name shown by every Flux entry point.
pub const APP_NAME: &str = "Flux";

/// Latest milestone whose implementation is complete and validated.
pub const MILESTONE: &str = "M5";

/// Startup text shared by the GUI and TUI binaries.
///
/// Kept deliberately factual: as of M5 both applications are working
/// local music players (library scanning plus playback for the common
/// local formats); what is still missing (playlists, seeking, …) is
/// listed in the README rather than hinted at here.
pub fn startup_message() -> String {
    format!("{APP_NAME} — {MILESTONE}. A local music player for your own library.")
}

#[cfg(test)]
mod tests {
    use super::{APP_NAME, MILESTONE, startup_message};

    #[test]
    fn startup_message_names_the_app_and_milestone() {
        let message = startup_message();
        assert!(message.contains(APP_NAME));
        assert!(message.contains(MILESTONE));
        assert!(message.contains("local music player"));
    }
}
