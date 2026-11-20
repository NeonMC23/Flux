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

/// Stable reverse-DNS identity used for the Linux application id, macOS bundle
/// identifier, desktop metadata and release artifact names. Changing this is a
/// release-breaking identity change.
pub const APP_ID: &str = "io.github.neonmc23.flux";

/// Version reported by release artifacts and the in-app Settings footer.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line product description used by package metadata.
pub const APP_DESCRIPTION: &str = "A local-first, native music player for your own library.";

/// Copyright line for About/installer metadata.
pub const APP_COPYRIGHT: &str = "Copyright (c) 2026 NeonMC23";

/// Latest milestone whose implementation is complete and validated.
pub const MILESTONE: &str = "M11";

/// Startup text shared by the GUI and TUI binaries.
///
/// Kept deliberately factual: both applications are working local music
/// players with canonical library management, playback, queues, playlists,
/// lyrics, artwork, import review and audio-device selection. Remaining
/// limits are documented in the README rather than hinted at here.
pub fn startup_message() -> String {
    format!("{APP_NAME} — {MILESTONE}. A local music player for your own library.")
}

/// Machine-stable release identity used by `--version` and packaging probes.
#[must_use]
pub fn release_info() -> String {
    format!("{APP_NAME} {APP_VERSION} ({APP_ID})")
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

    #[test]
    fn release_identity_is_stable_and_reverse_dns() {
        assert_eq!(super::APP_ID, "io.github.neonmc23.flux");
        assert!(super::APP_ID.split('.').count() >= 3);
        assert_eq!(super::APP_VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!super::APP_DESCRIPTION.is_empty());
        assert!(super::APP_COPYRIGHT.contains("NeonMC23"));
        let info = super::release_info();
        assert!(info.starts_with("Flux "));
        assert!(info.contains(super::APP_ID));
    }
}
