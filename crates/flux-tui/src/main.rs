//! Flux terminal entry point.
//!
//! Milestone M0 scope: print the shared startup message from `flux-core` and
//! exit. There is intentionally no interactive terminal interface, library,
//! or playback functionality yet, so no TUI framework dependency is taken on
//! until a later milestone implements the real interface.

fn main() {
    println!("{}", flux_core::startup_message());
    println!("The Flux terminal interface is not implemented yet.");
}
