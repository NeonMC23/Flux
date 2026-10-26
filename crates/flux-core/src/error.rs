//! Error convention shared by Flux's core and in-memory models.
//!
//! The enum is deliberately small and hand-written: it contains only the
//! variants that the current milestone's code can actually produce. New
//! variants are added when new behavior needs them, not speculatively.

use std::fmt;

use crate::queue::QueueEntryId;
use crate::track::TrackId;

/// Result alias used throughout Flux's backend-independent logic.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by Flux's queue and playback-control models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An operation required at least one queued track, but the queue was
    /// empty (for example, pressing play with nothing queued).
    EmptyQueue,
    /// A queue index was outside the queue's current bounds.
    QueueIndexOutOfBounds {
        /// The index that was requested.
        index: usize,
        /// The number of items in the queue at the time.
        len: usize,
    },
    /// A track ID was expected to be in the queue, but was not present.
    TrackNotInQueue {
        /// The track ID that could not be found in the queue.
        id: TrackId,
    },
    /// A queue entry identity was expected to be in the queue, but was
    /// not present (for example, it was already removed).
    QueueEntryNotFound {
        /// The entry ID that could not be found.
        id: QueueEntryId,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQueue => write!(f, "the queue is empty"),
            Self::QueueIndexOutOfBounds { index, len } => {
                write!(
                    f,
                    "queue index {index} is out of bounds (queue length {len})"
                )
            }
            Self::TrackNotInQueue { id } => write!(f, "track {id} is not in the queue"),
            Self::QueueEntryNotFound { id } => {
                write!(f, "queue entry {id} is not in the queue")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::track::TrackId;

    #[test]
    fn display_messages_are_descriptive() {
        assert_eq!(Error::EmptyQueue.to_string(), "the queue is empty");
        assert_eq!(
            Error::QueueIndexOutOfBounds { index: 5, len: 2 }.to_string(),
            "queue index 5 is out of bounds (queue length 2)"
        );
        assert_eq!(
            Error::TrackNotInQueue {
                id: TrackId::new(7)
            }
            .to_string(),
            "track 7 is not in the queue"
        );
    }

    #[test]
    fn error_implements_std_error() {
        fn assert_std_error<E: std::error::Error>(_: &E) {}
        assert_std_error(&Error::EmptyQueue);
    }
}
