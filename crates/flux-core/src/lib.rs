//! Shared domain foundation for Flux.
//!
//! This crate is the future home of domain types and application-level
//! abstractions shared by the GUI and TUI. It must stay independent of any
//! user interface, filesystem, database, audio-engine, or network
//! implementation.
//!
//! Milestone M0 intentionally defines no library, playback, or provider
//! behavior. The constants and helper below exist only so both binaries share
//! one honest startup identity instead of duplicating it.

/// Product name shown by every Flux entry point.
pub const APP_NAME: &str = "Flux";

/// Milestone that established this foundation.
pub const MILESTONE: &str = "M0";

/// Startup text shared by the GUI and TUI binaries.
///
/// The wording is deliberately explicit: M0 provides project foundations
/// only, with no library management or audio playback.
pub fn startup_message() -> String {
    format!("{APP_NAME} — {MILESTONE} foundations. Library and playback are not implemented yet.")
}

#[cfg(test)]
mod tests {
    use super::{APP_NAME, MILESTONE, startup_message};

    #[test]
    fn startup_message_names_the_app_and_milestone() {
        let message = startup_message();
        assert!(message.contains(APP_NAME));
        assert!(message.contains(MILESTONE));
        assert!(message.contains("not implemented yet"));
    }
}
