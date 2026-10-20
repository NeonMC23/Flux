//! Track identity and metadata types.
//!
//! These types are plain data. Nothing in this module performs filesystem
//! I/O: a [`TrackSource`] records *where* a track is expected to come from,
//! without checking that anything exists there.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Opaque in-memory identifier for a track.
///
/// IDs are assigned by collections (such as the in-memory library) when a
/// track is added. They are **not** persistent, are **not** globally unique
/// across runs or collections, and carry no meaning outside the collection
/// that issued them. They exist so queues and other models can refer to
/// tracks without copying metadata around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u64);

impl TrackId {
    /// Creates an ID from a raw value.
    ///
    /// Callers outside collections should treat IDs as opaque; this
    /// constructor exists for collections that mint IDs and for tests.
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

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a track's audio is expected to come from.
///
/// This milestone supports only local files, represented as a path *value*.
/// No filesystem access is performed when constructing or comparing
/// sources, and the path is not checked for existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    /// A track stored as a local file at the given path.
    Local {
        /// Path of the local file, exactly as provided by the caller.
        path: PathBuf,
    },
}

impl TrackSource {
    /// Creates a local source from a path value.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::Local { path: path.into() }
    }

    /// Returns the local file path recorded for this source.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Local { path } => path,
        }
    }
}

/// A single track's metadata and source.
///
/// A `Track` does not contain its own [`TrackId`]: IDs belong to the
/// collection that stores the track, so the same track value can be
/// discussed before it is added anywhere. All metadata besides the title
/// and source is optional, because real-world music metadata is frequently
/// incomplete; `None` means "unknown", never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    duration: Option<Duration>,
    source: TrackSource,
}

impl Track {
    /// Creates a track with only a title and source; all optional metadata
    /// starts as `None` and can be added with the `with_*` builders.
    pub fn new(title: impl Into<String>, source: TrackSource) -> Self {
        Self {
            title: title.into(),
            artist: None,
            album: None,
            track_number: None,
            disc_number: None,
            duration: None,
            source,
        }
    }

    /// Sets the artist name.
    #[must_use]
    pub fn with_artist(mut self, artist: impl Into<String>) -> Self {
        self.artist = Some(artist.into());
        self
    }

    /// Sets the album title.
    #[must_use]
    pub fn with_album(mut self, album: impl Into<String>) -> Self {
        self.album = Some(album.into());
        self
    }

    /// Sets the track number within its disc/album.
    #[must_use]
    pub fn with_track_number(mut self, track_number: u32) -> Self {
        self.track_number = Some(track_number);
        self
    }

    /// Sets the disc number for multi-disc releases.
    #[must_use]
    pub fn with_disc_number(mut self, disc_number: u32) -> Self {
        self.disc_number = Some(disc_number);
        self
    }

    /// Sets the track duration.
    #[must_use]
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// The track's title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The artist name, if known.
    #[must_use]
    pub fn artist(&self) -> Option<&str> {
        self.artist.as_deref()
    }

    /// The album title, if known.
    #[must_use]
    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    /// The track number, if known.
    #[must_use]
    pub fn track_number(&self) -> Option<u32> {
        self.track_number
    }

    /// The disc number, if known.
    #[must_use]
    pub fn disc_number(&self) -> Option<u32> {
        self.disc_number
    }

    /// The duration, if known.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Where this track's audio is expected to come from.
    #[must_use]
    pub fn source(&self) -> &TrackSource {
        &self.source
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::time::Duration;

    use super::{Track, TrackId, TrackSource};

    fn local_track(title: &str) -> Track {
        Track::new(title, TrackSource::local("/music/song.flac"))
    }

    #[test]
    fn new_track_has_no_optional_metadata() {
        let track = local_track("Blue Monday");
        assert_eq!(track.title(), "Blue Monday");
        assert_eq!(track.artist(), None);
        assert_eq!(track.album(), None);
        assert_eq!(track.track_number(), None);
        assert_eq!(track.disc_number(), None);
        assert_eq!(track.duration(), None);
        assert_eq!(track.source().path(), Path::new("/music/song.flac"));
    }

    #[test]
    fn builders_set_optional_metadata() {
        let track = local_track("Blue Monday")
            .with_artist("New Order")
            .with_album("Power, Corruption & Lies")
            .with_track_number(1)
            .with_disc_number(1)
            .with_duration(Duration::from_secs(449));
        assert_eq!(track.artist(), Some("New Order"));
        assert_eq!(track.album(), Some("Power, Corruption & Lies"));
        assert_eq!(track.track_number(), Some(1));
        assert_eq!(track.disc_number(), Some(1));
        assert_eq!(track.duration(), Some(Duration::from_secs(449)));
    }

    #[test]
    fn builders_override_previous_values() {
        let track = local_track("Song")
            .with_artist("First")
            .with_artist("Second");
        assert_eq!(track.artist(), Some("Second"));
    }

    #[test]
    fn track_id_is_copyable_hashable_and_opaque() {
        let a = TrackId::new(1);
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(a.get(), 1);
        assert_eq!(a.to_string(), "1");
        let mut set = HashSet::new();
        set.insert(TrackId::new(1));
        set.insert(TrackId::new(2));
        set.insert(TrackId::new(1));
        assert_eq!(set.len(), 2);
        assert!(TrackId::new(1) < TrackId::new(2));
    }
}
