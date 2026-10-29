//! Stable artist identity.
//!
//! Artist IDs are issued by a library catalogue and, like track IDs, are
//! meaningful only within the library that issued them. They exist so a
//! display-name change never has to rewrite every track that refers to the
//! artist.

use std::fmt;

/// Opaque identifier for an artist within one library.
///
/// The catalogue persists both issued IDs and its monotonic counter, so an
/// ID is stable across saves, restarts, rescans, and artist renames. IDs are
/// never reused for a different artist within the same library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtistId(u64);

impl ArtistId {
    /// Creates an ID from a raw value.
    ///
    /// Callers outside the catalogue should normally treat IDs as opaque;
    /// this constructor exists for catalogue implementations and tests.
    #[must_use]
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw underlying value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ArtistId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ArtistId;

    #[test]
    fn artist_ids_are_copyable_hashable_and_ordered() {
        let a = ArtistId::new(9);
        let b = a;
        assert_eq!(a, b);
        assert_eq!(a.get(), 9);
        assert_eq!(a.to_string(), "9");
        assert!(ArtistId::new(1) < ArtistId::new(2));
    }
}
