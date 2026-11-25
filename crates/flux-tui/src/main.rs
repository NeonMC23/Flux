//! Flux terminal interface (`flux-tui`).
//!
//! A complete keyboard-first player over the shared [`flux_app::Controller`] —
//! the same controller, canonical library, playback engine, queue, history,
//! ratings, playlists, lyrics and import pipeline as the GUI.
//!
//! The interface is organized as library destinations (Tracks, Artists,
//! Albums, Playlists, Collections, Lyrics, Settings) with contextual detail
//! views, a first-class queue, five layout presets, a command palette and a
//! real audio visualizer fed by the engine's post-gain waveform tap. Metadata
//! editing uses labelled, prefilled forms. Rendering is hand-written on
//! `crossterm`; no domain or persistence logic lives here.
//!
//! Usage: `flux-tui [MUSIC_DIR]` — with a directory argument, the same
//! cancellable review-first import opens immediately. Real audio output
//! requires a build with `--features cpal`; a no-CPAL build reports that
//! honestly. See `docs/m16-tui-parity.md` for the full keymap and limitations.
//!
//! `Tab`/`Shift+Tab` and `F1`–`F8` switch destinations. `?` opens contextual
//! help. `m` cycles layouts; `v` toggles the visualizer and `Alt+v` cycles its
//! modes. The terminal is restored on normal exit, error and panic.

mod forms;
mod help;
mod import;
mod render;
mod text;
mod visualizer;

use forms::{
    ImageEdit, album_edit_form, artist_edit_form, parse_album_edit_form, parse_artist_edit_form,
    parse_release_type, parse_track_edit_form, track_edit_form,
};
#[cfg(test)]
use help::normal_help;
use help::{help_lines, migration_plan_summary, reconcile_summary};
#[cfg(test)]
use import::ImportWorkerMessage;
use import::TuiImport;
use text::{
    clamp_terminal_size, count_label, detail_view_fits, display_album, display_title,
    display_width, fit_input_line, format_lyrics_timestamp, pad_with_right, resource_label,
    sentence_case, terminal_too_small, truncate_to_width,
};
use visualizer::{VizMode, visualization_canvas};

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use flux_app::{
    AlbumId, AlbumTransferPlan, ArtistDeletionDecision, ArtistDeletionDestination,
    ArtistDeletionPlan, AudioOutputDevice, AudioStatus, CandidateStatus, CanonicalMigrationPlan,
    Controller, ImportAlbumTarget, ImportDecision, OpenedAudioOutput, PlaylistId, ReconcileReport,
    ReleaseType, SmartCollection, StorageMode, TrackDeletionPlan, TrackMetadataEdit, TrackMovePlan,
    WaveformScope, format_duration, is_track_playable,
};
use flux_core::{APP_VERSION, ArtistId, PlaybackState, Track, TrackId};

fn handle_cli() -> bool {
    for arg in std::env::args_os().skip(1) {
        if arg == "--version" || arg == "-V" {
            println!("{}", flux_core::release_info());
            return true;
        }
        if arg == "--audio-diagnostics" {
            println!("{}", flux_app::audio_diagnostics_report());
            return true;
        }
        if arg == "--help" || arg == "-h" {
            println!(
                "Flux {}\nA local-first, native music player (terminal interface).\n\n\
                 Usage: flux-tui [MUSIC_DIR]\n\n\
                 MUSIC_DIR opens the cancellable import review for that folder.\n\
                 --audio-diagnostics reports output devices and stream startup.\n\
                 Use --version to print the release identity and --help for this text.",
                APP_VERSION
            );
            return true;
        }
    }
    false
}

fn main() -> ExitCode {
    if handle_cli() {
        return ExitCode::SUCCESS;
    }
    let initial_dir = std::env::args_os().nth(1).map(PathBuf::from);
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("{}", flux_core::startup_message());
        eprintln!("flux-tui needs an interactive terminal (stdin/stdout are not TTYs).");
        return ExitCode::FAILURE;
    }
    match run(initial_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("flux-tui: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Restores the user's terminal: show cursor, leave the alternate
/// screen, disable raw mode. Called on every exit path, including
/// panics.
fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = execute!(out, Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(err) = execute!(std::io::stdout(), EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Primary library destinations. Navigation is shared by the tab bar,
/// keyboard shortcuts and the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Tracks,
    Artists,
    Albums,
    Playlists,
    Collections,
    Lyrics,
    Settings,
}

impl View {
    const ALL: [Self; 8] = [
        Self::Home,
        Self::Tracks,
        Self::Artists,
        Self::Albums,
        Self::Playlists,
        Self::Collections,
        Self::Lyrics,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Tracks => "Tracks",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Playlists => "Playlists",
            Self::Collections => "Collections",
            Self::Lyrics => "Lyrics",
            Self::Settings => "Settings",
        }
    }
}

/// A selected entity context inside a primary view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    Artist(ArtistId),
    Album(AlbumId),
    Playlist(PlaylistId),
}

/// One visible, selectable row. Entity rows preserve stable identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Track(TrackId),
    Artist(ArtistId),
    Album(AlbumId),
    Playlist(PlaylistId),
    Collection(SmartCollection),
    LyricsTrack(TrackId),
    Setting(SettingRow),
    AudioDevice(Option<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingRow {
    Layout,
    Visualizer,
    VisualizationMode,
    FollowPlayback,
    RefreshDevices,
    LibrarySetup,
}

impl SettingRow {
    const ALL: [Self; 6] = [
        Self::Layout,
        Self::Visualizer,
        Self::VisualizationMode,
        Self::FollowPlayback,
        Self::RefreshDevices,
        Self::LibrarySetup,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Layout => "Layout",
            Self::Visualizer => "Visualizer",
            Self::VisualizationMode => "Visualization mode",
            Self::FollowPlayback => "Lyrics follow playback",
            Self::RefreshDevices => "Refresh audio devices",
            Self::LibrarySetup => "Library setup",
        }
    }
}

/// Coherent layout presets rather than arbitrary panel toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Standard,
    Library,
    Player,
    Visualizer,
    Compact,
}

impl LayoutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Library => "Library",
            Self::Player => "Player",
            Self::Visualizer => "Visualizer",
            Self::Compact => "Compact",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Standard => Self::Library,
            Self::Library => Self::Player,
            Self::Player => Self::Visualizer,
            Self::Visualizer => Self::Compact,
            Self::Compact => Self::Standard,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    ScanInput,
    SearchInput,
    /// Typing a folder whose portable library to open (`O`).
    OpenInput,
    /// Typing a folder for a new portable library (`C`).
    CreateInput,
    /// Typing semicolon-separated audio file paths (`i`/`I` copy into the library).
    ImportInput,
    /// A cancellable discovery/hashing phase is running in a worker.
    ImportScanning,
    /// Compact review of discovered candidates before any copy.
    ImportReview,
    /// A cancellable copy/preparation phase is running in a worker.
    ImportPreparing,
    /// Editing the selected track as `title|album|track|disc|artist-id`.
    TrackEditInput,
    /// Creating a stable artist by name (`P`).
    ArtistCreateInput,
    /// Editing an artist as `artist-id|name|aliases|description|image` (`Y`).
    ArtistRenameInput,
    /// Editing an album as `album-id|title|release-type|cover` (`Z`).
    AlbumEditInput,
    /// Setting/removing a Track artwork override from a path (`W`).
    TrackArtworkInput,
    /// Setting/removing managed Track lyrics from a UTF-8 file (`S`).
    TrackLyricsInput,
    /// Typing the legacy index file to migrate (`M`, step 1).
    MigrateSourceInput,
    /// Typing the migration destination folder (`M`, step 2).
    MigrateDestInput,
    /// A migration plan awaits confirmation: `y` executes, anything
    /// else cancels (planning wrote nothing).
    MigrateConfirm,
    /// A migration finished: `a` opens the result, anything else
    /// keeps the current library.
    MigratedDone,
    /// Read-only managed lyrics for the selected track (`y`).
    LyricsView,
    /// Scrollable help or import-result details.
    InfoView,
    /// Stable QueueEntryId list with playback, removal and reordering.
    QueueView,
    /// Explicit confirmation before preserving and replacing a protected index.
    ConfirmPreserveIndex,
    /// Explicit confirmation before acknowledging portable recovery.
    ConfirmAcknowledgeRecovery,
    /// A structured, labelled form. Tab moves between fields.
    FormInput,
    /// A small command palette (`:`), parsed into ordinary actions.
    CommandInput,
    /// Explicit confirmation for a destructive playlist operation.
    ConfirmPlaylist,
    /// Explicit confirmation before clearing the whole queue.
    ConfirmQueueClear,
    /// Choosing a destination album or standalone state for one track.
    TrackMovePick,
    /// Confirming a reviewed album-membership move.
    ConfirmTrackMove,
    /// Typing a replacement audio path for one track.
    TrackReplacementInput,
    /// Confirming a reviewed managed-audio replacement.
    ConfirmTrackReplacement,
    /// Confirming a reviewed single-track deletion.
    ConfirmTrackDelete,
    /// Confirming deletion of an empty album.
    ConfirmAlbumDelete,
    /// Browsing the participating Artists of the open Album.
    AlbumParticipants,
    /// Choosing an existing Artist to add to the open Album.
    AlbumParticipantPick,
    /// Confirming removal of one Album participation association.
    ConfirmAlbumParticipantRemove,
    /// Reviewing decisions for deleting an artist with affected tracks.
    ArtistDeletion,
    /// Confirming deletion of an artist with no track decisions.
    ConfirmArtistDelete,
    /// Choosing a fixed target album for import review.
    ImportAlbumPick,
    /// Reviewing additional Album participants for one import candidate.
    ImportParticipants,
    /// Choosing an existing Artist for an import candidate's Album.
    ImportParticipantPick,
    /// Choosing a destination artist for album ownership transfer.
    AlbumTransferPick,
    /// Reviewing an album ownership transfer.
    AlbumTransferReview,
    /// Choosing an explicit destination owner for one artist-deletion track.
    ArtistOwnerPick,
    /// Choosing an existing destination album or a new album.
    ArtistAlbumPick,
    /// Exporting managed lyrics to a user path.
    LyricsExportInput,
    /// Attaching import album artwork, track artwork or lyrics.
    ImportAssetInput,
}

#[derive(Debug, Clone)]
struct MoveTarget {
    album: Option<AlbumId>,
    label: String,
    tracks: usize,
    current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportAssetKind {
    AlbumArtwork,
    TrackArtwork,
    Lyrics,
}

#[derive(Debug, Clone)]
enum PlaylistAction {
    Delete {
        id: PlaylistId,
        name: String,
        tracks: Vec<TrackId>,
    },
    Clear {
        id: PlaylistId,
        tracks: Vec<TrackId>,
    },
    Remove {
        id: PlaylistId,
        position: usize,
        tracks: Vec<TrackId>,
    },
}

struct RowsCache {
    session: u64,
    revision: u64,
    generation: u64,
    view: View,
    detail: Option<Detail>,
    collection: Option<SmartCollection>,
    search: String,
    favorites_only: bool,
    rows: Rc<Vec<Row>>,
    track_rows: Rc<Vec<TrackId>>,
}

/// Waits briefly for an import worker. Cancellation is cooperative at file
/// and copy-chunk boundaries, so a blocked OS read cannot be interrupted; the
/// UI is not made to hang indefinitely waiting for it.

#[derive(Debug, Clone)]
struct FormField {
    label: &'static str,
    value: String,
    hint: &'static str,
}

#[derive(Debug, Clone)]
enum FormKind {
    Track {
        id: TrackId,
        original_credit: String,
        original_credits: Vec<ArtistId>,
    },
    Artist(ArtistId),
    Album(AlbumId),
    PlaylistNew,
    PlaylistRename {
        id: PlaylistId,
        expected_name: String,
        expected_tracks: Vec<TrackId>,
    },
    ImportCandidate(usize),
    ArtistTransferAlbum {
        track: TrackId,
        owner: ArtistId,
    },
}

#[derive(Debug, Clone)]
struct FormState {
    title: String,
    kind: FormKind,
    fields: Vec<FormField>,
    focus: usize,
    error: Option<String>,
}

impl FormState {
    fn new(title: impl Into<String>, kind: FormKind, fields: Vec<FormField>) -> Self {
        Self {
            title: title.into(),
            kind,
            fields,
            focus: 0,
            error: None,
        }
    }
}

type AudioListReceiver = Receiver<Result<Vec<AudioOutputDevice>, String>>;
type AudioOpenReceiver = Receiver<(Option<String>, Result<OpenedAudioOutput, String>)>;

struct Tui {
    controller: Controller,
    /// Compact import review/progress state shared with the GUI pipeline.
    import_ui: Option<TuiImport>,
    /// Cursor position within the currently visible (filtered) rows.
    cursor: usize,
    /// First visible row index (scroll window).
    offset: usize,
    mode: Mode,
    /// Text being typed in an input mode.
    input: String,
    /// The active library filter (empty = everything).
    search: String,
    /// Cached visible IDs for the current library/search/favorites key.
    rows_cache: RefCell<Option<RowsCache>>,
    /// Show only favorite tracks.
    favorites_only: bool,
    /// Legacy index file chosen at migration step 1.
    migrate_source: PathBuf,
    /// Planned-but-unconfirmed migration (nothing written yet).
    pending_plan: Option<CanonicalMigrationPlan>,
    /// Destination of a completed migration, offered for activation.
    migrated_dest: Option<PathBuf>,
    /// Latest library-lifecycle outcome (plan summary, errors, …).
    lifecycle_msg: Option<String>,
    /// Last reconciliation, used by the explicit `L` unique-relink action.
    last_reconcile: Option<ReconcileReport>,
    /// Read-only lyrics lines for the selected track.
    lyrics_lines: Vec<String>,
    /// First visible lyrics line.
    lyrics_scroll: usize,
    /// Human-readable lyrics title/status.
    lyrics_title: String,
    /// Whether the next lyrics update should snap to the current line.
    lyrics_follow_ready: bool,
    /// Track and timestamps backing the open lyrics view.
    lyrics_track: Option<TrackId>,
    lyrics_timestamps: Vec<Option<u64>>,
    /// Cursor within the stable queue-entry list.
    queue_cursor: usize,
    /// Whether the queue cursor follows the current playback entry.
    queue_follow: bool,
    /// First visible queue row.
    queue_offset: usize,
    /// Left offset reserved by the persistent destination sidebar.
    content_offset: usize,
    /// Cursor in the Album participant association list.
    album_participant_cursor: usize,
    /// Cursor in the add-participant Artist picker.
    album_participant_pick_cursor: usize,
    /// Cursor in the import-review participant list.
    import_participant_cursor: usize,
    /// Cursor in the import-review participant picker.
    import_participant_pick_cursor: usize,
    /// Pending participant removal confirmation.
    pending_participant_remove: Option<(AlbumId, ArtistId)>,
    /// Title and lines for help/import-detail info views.
    info_title: String,
    info_lines: Vec<String>,
    info_scroll: usize,
    /// Active primary library destination and optional entity detail.
    view: View,
    detail: Option<Detail>,
    /// Derived collection currently filtering the Tracks view.
    active_collection: Option<SmartCollection>,
    /// Coherent layout preset and visualization preferences.
    layout: LayoutMode,
    viz_visible: bool,
    viz_mode: VizMode,
    viz_gain: f32,
    scope: WaveformScope,
    follow_lyrics: bool,
    prefs_path: Option<PathBuf>,
    /// Structured form shared by common metadata and playlist edits.
    form: Option<FormState>,
    playlist_action: Option<PlaylistAction>,
    move_track: Option<TrackId>,
    move_cursor: usize,
    pending_move: Option<TrackMovePlan>,
    pending_track_delete: Option<TrackDeletionPlan>,
    pending_album_delete: Option<AlbumId>,
    import_asset: Option<ImportAssetKind>,
    asset_track: Option<TrackId>,
    pending_replacement: Option<flux_app::TrackAudioReplacementPlan>,
    artist_deletion: Option<ArtistDeletionPlan>,
    artist_delete_cursor: usize,
    artist_delete_armed: Option<TrackId>,
    artist_transfer_track: Option<TrackId>,
    artist_transfer_owner: Option<ArtistId>,
    artist_owner_cursor: usize,
    artist_album_cursor: usize,
    album_transfer: Option<AlbumTransferPlan>,
    album_transfer_cursor: usize,
    album_transfer_artist_cursor: usize,
    import_album_cursor: usize,
    /// Audio-device list and asynchronous selection state.
    audio_devices: Vec<AudioOutputDevice>,
    audio_error: Option<String>,
    audio_loading: bool,
    audio_list_rx: Option<AudioListReceiver>,
    audio_open_rx: Option<AudioOpenReceiver>,
}

impl Tui {
    /// Visible rows for the active view/detail. Only stable identities are
    /// cached; metadata is looked up for the handful of rows actually drawn.
    fn view_rows(&self) -> Rc<Vec<Row>> {
        if self.view == View::Settings {
            let mut rows = SettingRow::ALL
                .iter()
                .copied()
                .map(Row::Setting)
                .collect::<Vec<_>>();
            rows.push(Row::AudioDevice(None));
            rows.extend((0..self.audio_devices.len()).map(|index| Row::AudioDevice(Some(index))));
            return Rc::new(rows);
        }
        let session = self.controller.library_session();
        let revision = self.controller.library_revision();
        let generation = self.controller.generation();
        if let Some(cache) = self.rows_cache.borrow().as_ref()
            && cache.session == session
            && cache.revision == revision
            && cache.generation == generation
            && cache.view == self.view
            && cache.detail == self.detail
            && cache.collection == self.active_collection
            && cache.search == self.search
            && cache.favorites_only == self.favorites_only
        {
            return Rc::clone(&cache.rows);
        }
        let rows = Rc::new(self.build_rows());
        let track_rows = Rc::new(
            rows.iter()
                .filter_map(|row| match row {
                    Row::Track(id) | Row::LyricsTrack(id) => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
        *self.rows_cache.borrow_mut() = Some(RowsCache {
            session,
            revision,
            generation,
            view: self.view,
            detail: self.detail,
            collection: self.active_collection,
            search: self.search.clone(),
            favorites_only: self.favorites_only,
            rows: Rc::clone(&rows),
            track_rows,
        });
        rows
    }

    fn build_rows(&self) -> Vec<Row> {
        let query = self.search.trim().to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        let track_matches = |id: TrackId| {
            let Some(track) = self.controller.track(id) else {
                return false;
            };
            matches(track.title())
                || matches(&credit_display(&self.controller, id))
                || track
                    .album()
                    .is_some_and(|album| album.to_lowercase().contains(&query))
        };
        let favorite = |id: TrackId| {
            self.controller
                .rating(id)
                .is_some_and(|rating| rating.favorite)
        };
        let filter_track =
            |id: TrackId| track_matches(id) && (!self.favorites_only || favorite(id));

        if let Some(detail) = self.detail {
            match detail {
                Detail::Artist(artist_id) => {
                    let mut rows = Vec::new();
                    let mut albums = self.controller.albums_owned_by(artist_id);
                    for id in self.controller.albums_participating_for(artist_id) {
                        if !albums.contains(&id) {
                            albums.push(id);
                        }
                    }
                    for id in self.controller.albums_credited_for(artist_id) {
                        if !albums.contains(&id) {
                            albums.push(id);
                        }
                    }
                    let mut albums = albums
                        .into_iter()
                        .filter_map(|id| self.controller.album(id))
                        .filter(|album| matches(&album.title))
                        .collect::<Vec<_>>();
                    albums.sort_by(|left, right| {
                        left.title
                            .to_lowercase()
                            .cmp(&right.title.to_lowercase())
                            .then_with(|| left.id.cmp(&right.id))
                    });
                    rows.extend(albums.into_iter().map(|album| Row::Album(album.id)));
                    let mut tracks = self
                        .controller
                        .tracks()
                        .iter()
                        .filter(|(id, _)| {
                            self.controller.track_artist_id(*id) == Some(artist_id)
                                || self.controller.track_credits(*id).contains(&artist_id)
                        })
                        .map(|(id, _)| *id)
                        .filter(|id| filter_track(*id))
                        .collect::<Vec<_>>();
                    tracks.sort_by(|left, right| {
                        let left_track = self.controller.track(*left);
                        let right_track = self.controller.track(*right);
                        left_track
                            .map(|track| display_album(track).to_lowercase())
                            .cmp(&right_track.map(|track| display_album(track).to_lowercase()))
                            .then_with(|| {
                                left_track
                                    .and_then(Track::track_number)
                                    .cmp(&right_track.and_then(Track::track_number))
                            })
                            .then_with(|| left.cmp(right))
                    });
                    rows.extend(tracks.into_iter().map(Row::Track));
                    rows
                }
                Detail::Album(album_id) => self
                    .controller
                    .album_tracks(album_id)
                    .into_iter()
                    .filter(|id| filter_track(*id))
                    .map(Row::Track)
                    .collect(),
                Detail::Playlist(playlist_id) => self
                    .controller
                    .playlist(playlist_id)
                    .map(|playlist| {
                        playlist
                            .track_ids
                            .iter()
                            .copied()
                            .filter(|id| filter_track(*id))
                            .map(Row::Track)
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        } else {
            match self.view {
                View::Home => vec![
                    Row::Collection(SmartCollection::RecentlyPlayed),
                    Row::Collection(SmartCollection::RecentlyAdded),
                    Row::Collection(SmartCollection::MostPlayed),
                    Row::Collection(SmartCollection::Favorites),
                    Row::Collection(SmartCollection::HighlyRated),
                ],
                View::Tracks => {
                    let mut ids = if let Some(collection) = self.active_collection {
                        self.controller.smart_collection_tracks(collection)
                    } else {
                        self.controller
                            .search(&self.search)
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect()
                    };
                    ids.retain(|id| {
                        (!self.favorites_only || favorite(*id))
                            && (self.active_collection.is_none() || filter_track(*id))
                    });
                    ids.into_iter().map(Row::Track).collect()
                }
                View::Artists => {
                    let mut artists = self
                        .controller
                        .artists()
                        .into_iter()
                        .filter(|artist| {
                            matches(artist.name())
                                || artist.aliases().iter().any(|alias| matches(alias))
                        })
                        .collect::<Vec<_>>();
                    artists.sort_by(|left, right| {
                        left.name()
                            .to_lowercase()
                            .cmp(&right.name().to_lowercase())
                            .then_with(|| left.id().cmp(&right.id()))
                    });
                    artists
                        .into_iter()
                        .map(|artist| Row::Artist(artist.id()))
                        .collect()
                }
                View::Albums => {
                    let mut albums = self
                        .controller
                        .albums()
                        .into_iter()
                        .filter(|album| {
                            let owner = self
                                .controller
                                .artist(album.owner_artist_id)
                                .map(|artist| artist.name().to_owned())
                                .unwrap_or_default();
                            matches(&album.title) || matches(&owner)
                        })
                        .collect::<Vec<_>>();
                    albums.sort_by(|left, right| {
                        left.title
                            .to_lowercase()
                            .cmp(&right.title.to_lowercase())
                            .then_with(|| left.id.cmp(&right.id))
                    });
                    albums
                        .into_iter()
                        .map(|album| Row::Album(album.id))
                        .collect()
                }
                View::Playlists => {
                    let mut playlists = self
                        .controller
                        .playlists()
                        .into_iter()
                        .filter(|playlist| matches(&playlist.name))
                        .collect::<Vec<_>>();
                    playlists.sort_by(|left, right| {
                        left.name
                            .to_lowercase()
                            .cmp(&right.name.to_lowercase())
                            .then_with(|| left.id.cmp(&right.id))
                    });
                    playlists
                        .into_iter()
                        .map(|playlist| Row::Playlist(playlist.id))
                        .collect()
                }
                View::Collections => SmartCollection::ALL
                    .into_iter()
                    .filter(|collection| {
                        matches(collection.label()) || matches(collection.description())
                    })
                    .map(Row::Collection)
                    .collect(),
                View::Lyrics => {
                    let mut ids = self
                        .controller
                        .tracks()
                        .iter()
                        .map(|(id, _)| *id)
                        .filter(|id| self.controller.track_has_lyrics(*id))
                        .filter(|id| track_matches(*id))
                        .collect::<Vec<_>>();
                    ids.sort_by(|left, right| {
                        self.controller
                            .track(*left)
                            .map(|track| display_title(track).to_lowercase())
                            .cmp(
                                &self
                                    .controller
                                    .track(*right)
                                    .map(|track| display_title(track).to_lowercase()),
                            )
                            .then_with(|| left.cmp(right))
                    });
                    ids.into_iter().map(Row::LyricsTrack).collect()
                }
                View::Settings => Vec::new(),
            }
        }
    }

    /// Track IDs currently visible as tracks, in display order.
    ///
    /// The vector is shared with the row cache so large unchanged lists do not
    /// rebuild or reallocate on every playback tick.
    fn rows(&self) -> Rc<Vec<TrackId>> {
        let rows = self.view_rows();
        if let Some(cache) = self.rows_cache.borrow().as_ref()
            && Rc::ptr_eq(&cache.rows, &rows)
        {
            return Rc::clone(&cache.track_rows);
        }
        Rc::new(
            rows.iter()
                .filter_map(|row| match row {
                    Row::Track(id) | Row::LyricsTrack(id) => Some(*id),
                    _ => None,
                })
                .collect(),
        )
    }

    fn current_row(&self) -> Option<Row> {
        self.view_rows().get(self.cursor).copied()
    }

    fn current_track(&self) -> Option<TrackId> {
        match self.current_row()? {
            Row::Track(id) | Row::LyricsTrack(id) => Some(id),
            _ => None,
        }
    }

    fn selected_playlist(&self) -> Option<PlaylistId> {
        match self.detail {
            Some(Detail::Playlist(id)) => Some(id),
            _ => match self.current_row() {
                Some(Row::Playlist(id)) => Some(id),
                _ => None,
            },
        }
    }

    fn reset_selection(&mut self) {
        self.cursor = 0;
        self.offset = 0;
    }

    fn switch_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.detail = None;
            self.active_collection = None;
            self.reset_selection();
            if view == View::Settings && self.audio_devices.is_empty() {
                self.refresh_audio_devices();
            }
        }
    }

    fn library_full_identity(&self) -> String {
        match self.controller.storage_mode() {
            StorageMode::Legacy => self.controller.legacy_index_path().map_or_else(
                || "legacy index (session-only)".to_owned(),
                |path| format!("legacy index {}", path.display()),
            ),
            StorageMode::Portable => self.controller.portable_root().map_or_else(
                || "portable library".to_owned(),
                |root| format!("portable {}", root.display()),
            ),
            StorageMode::Canonical => self.controller.canonical_root().map_or_else(
                || "canonical library".to_owned(),
                |root| format!("canonical {}", root.display()),
            ),
        }
    }

    fn load_preferences(&mut self) {
        let Some(path) = self.prefs_path.as_ref() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "layout" => {
                    self.layout = match value.trim() {
                        "library" => LayoutMode::Library,
                        "player" => LayoutMode::Player,
                        "visualizer" => LayoutMode::Visualizer,
                        "compact" => LayoutMode::Compact,
                        _ => LayoutMode::Standard,
                    };
                }
                "visualizer" => self.viz_visible = value.trim() != "hidden",
                "visualization_mode" => {
                    self.viz_mode = match value.trim() {
                        "bars" => VizMode::Bars,
                        "pulse" => VizMode::Pulse,
                        _ => VizMode::Waveform,
                    };
                }
                "visualization_gain" => {
                    if let Ok(gain) = value.trim().parse::<f32>() {
                        self.viz_gain = gain.clamp(0.5, 2.5);
                    }
                }
                "follow_lyrics" => self.follow_lyrics = value.trim() != "off",
                _ => {}
            }
        }
    }

    fn save_preferences(&self) {
        let Some(path) = self.prefs_path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let layout = match self.layout {
            LayoutMode::Standard => "standard",
            LayoutMode::Library => "library",
            LayoutMode::Player => "player",
            LayoutMode::Visualizer => "visualizer",
            LayoutMode::Compact => "compact",
        };
        let mode = match self.viz_mode {
            VizMode::Waveform => "waveform",
            VizMode::Bars => "bars",
            VizMode::Pulse => "pulse",
        };
        let text = format!(
            "layout={layout}\nvisualizer={}\nvisualization_mode={mode}\nvisualization_gain={:.2}\nfollow_lyrics={}\n",
            if self.viz_visible { "shown" } else { "hidden" },
            self.viz_gain,
            if self.follow_lyrics { "on" } else { "off" },
        );
        let _ = std::fs::write(path, text);
    }

    fn refresh_audio_devices(&mut self) {
        if self.audio_loading {
            return;
        }
        self.audio_loading = true;
        self.audio_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(flux_app::enumerate_audio_output_devices());
        });
        self.audio_list_rx = Some(rx);
    }

    fn poll_audio_workers(&mut self) {
        if let Some(rx) = self.audio_list_rx.as_ref()
            && let Ok(result) = rx.try_recv()
        {
            self.audio_list_rx = None;
            self.audio_loading = false;
            match result {
                Ok(devices) => {
                    self.audio_devices = devices;
                    self.audio_error = None;
                }
                Err(error) => self.audio_error = Some(error),
            }
            *self.rows_cache.borrow_mut() = None;
        }
        if let Some(rx) = self.audio_open_rx.as_ref()
            && let Ok((target, result)) = rx.try_recv()
        {
            self.audio_open_rx = None;
            match result {
                Ok(opened) => match self.controller.install_audio_output(opened) {
                    Ok(()) => {
                        self.audio_error = None;
                        self.lifecycle_msg = Some(match target {
                            Some(id) => format!("Switched audio output to {id}."),
                            None => "Switched to the system default output.".to_owned(),
                        });
                    }
                    Err(error) => self.audio_error = Some(error),
                },
                Err(error) => self.audio_error = Some(error),
            }
        }
    }

    fn select_audio_output(&mut self, target: Option<String>) {
        if self.audio_open_rx.is_some() {
            self.audio_error = Some("Wait for the current output switch to finish.".to_owned());
            return;
        }
        if matches!(self.controller.audio_status(), AudioStatus::Recovering(_)) {
            self.audio_error =
                Some("Wait for the current audio recovery attempt to finish.".to_owned());
            return;
        }
        let request = target.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((request.clone(), flux_app::open_audio_output(request)));
        });
        self.audio_open_rx = Some(rx);
        self.audio_error = None;
        self.lifecycle_msg = Some("Opening the selected audio output…".to_owned());
    }

    fn toggle_setting(&mut self, setting: SettingRow) {
        match setting {
            SettingRow::Layout => {
                self.layout = self.layout.next();
                self.save_preferences();
                self.lifecycle_msg = Some(format!("Layout: {}.", self.layout.label()));
            }
            SettingRow::Visualizer => {
                self.viz_visible = !self.viz_visible;
                self.save_preferences();
                self.lifecycle_msg = Some(if self.viz_visible {
                    "Visualizer shown.".to_owned()
                } else {
                    "Visualizer hidden; waveform sampling is paused.".to_owned()
                });
            }
            SettingRow::VisualizationMode => {
                self.viz_mode = self.viz_mode.next();
                self.save_preferences();
                self.lifecycle_msg = Some(format!("Visualization: {}.", self.viz_mode.label()));
            }
            SettingRow::FollowPlayback => {
                self.follow_lyrics = !self.follow_lyrics;
                self.save_preferences();
                self.lifecycle_msg = Some(if self.follow_lyrics {
                    "Lyrics will follow playback.".to_owned()
                } else {
                    "Lyrics follow is off; scroll manually.".to_owned()
                });
            }
            SettingRow::RefreshDevices => self.refresh_audio_devices(),
            SettingRow::LibrarySetup => {
                self.mode = Mode::OpenInput;
                self.input.clear();
            }
        }
    }

    fn open_track_form(&mut self, id: TrackId) {
        let Some(track) = self.controller.track(id) else {
            self.lifecycle_msg = Some("The selected track is no longer present.".to_owned());
            return;
        };
        let owner = self.controller.track_artist_id(id);
        let credit = self
            .controller
            .track_credits(id)
            .first()
            .copied()
            .or(owner)
            .and_then(|artist| self.controller.artist(artist))
            .map(|artist| artist.name().to_owned())
            .unwrap_or_default();
        let fields = vec![
            FormField {
                label: "Title",
                value: track.title().to_owned(),
                hint: "Required",
            },
            FormField {
                label: "Artist credit",
                value: credit.clone(),
                hint: "Existing name or a new artist; ownership is unchanged",
            },
            FormField {
                label: "Album",
                value: track.album().unwrap_or_default().to_owned(),
                hint: "Empty keeps a standalone track",
            },
            FormField {
                label: "Track number",
                value: track
                    .track_number()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                hint: "Optional whole number",
            },
            FormField {
                label: "Disc number",
                value: track
                    .disc_number()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                hint: "Optional whole number",
            },
        ];
        self.form = Some(FormState::new(
            format!("Edit track · {}", display_title(track)),
            FormKind::Track {
                id,
                original_credit: credit,
                original_credits: self.controller.track_credits(id),
            },
            fields,
        ));
        self.mode = Mode::FormInput;
    }

    fn open_artist_form(&mut self, id: ArtistId) {
        let Some(artist) = self.controller.artist(id) else {
            self.lifecycle_msg = Some("The selected artist is no longer present.".to_owned());
            return;
        };
        let fields = vec![
            FormField {
                label: "Name",
                value: artist.name().to_owned(),
                hint: "Required",
            },
            FormField {
                label: "Aliases",
                value: self.controller.artist_aliases(id).join("; "),
                hint: "Separate aliases with semicolons",
            },
            FormField {
                label: "Description",
                value: self
                    .controller
                    .artist_description(id)
                    .unwrap_or_default()
                    .to_owned(),
                hint: "Optional",
            },
            FormField {
                label: "Image path",
                value: self
                    .controller
                    .artist_image_path(id)
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                hint: "Empty preserves; - removes",
            },
        ];
        self.form = Some(FormState::new(
            format!("Edit artist · {}", artist.name()),
            FormKind::Artist(id),
            fields,
        ));
        self.mode = Mode::FormInput;
    }

    fn open_album_form(&mut self, id: AlbumId) {
        let Some(album) = self.controller.album(id) else {
            self.lifecycle_msg = Some("The selected album is no longer present.".to_owned());
            return;
        };
        let fields = vec![
            FormField {
                label: "Title",
                value: album.title.clone(),
                hint: "Required",
            },
            FormField {
                label: "Release type",
                value: album.release_type.as_str().to_owned(),
                hint: "Album, EP, Single, Compilation or Other",
            },
            FormField {
                label: "Cover path",
                value: self
                    .controller
                    .album_cover_path(id)
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                hint: "Empty preserves; - removes",
            },
        ];
        self.form = Some(FormState::new(
            format!("Edit album · {}", album.title),
            FormKind::Album(id),
            fields,
        ));
        self.mode = Mode::FormInput;
    }

    fn open_playlist_create_form(&mut self) {
        self.form = Some(FormState::new(
            "New playlist",
            FormKind::PlaylistNew,
            vec![FormField {
                label: "Name",
                value: String::new(),
                hint: "Required",
            }],
        ));
        self.mode = Mode::FormInput;
    }

    fn open_playlist_rename_form(&mut self, id: PlaylistId) {
        let Some(playlist) = self.controller.playlist(id) else {
            self.lifecycle_msg = Some("The selected playlist is no longer present.".to_owned());
            return;
        };
        self.form = Some(FormState::new(
            format!("Rename playlist · {}", playlist.name),
            FormKind::PlaylistRename {
                id,
                expected_name: playlist.name.clone(),
                expected_tracks: playlist.track_ids.clone(),
            },
            vec![FormField {
                label: "Name",
                value: playlist.name.clone(),
                hint: "Required",
            }],
        ));
        self.mode = Mode::FormInput;
    }

    fn submit_form(&mut self) {
        let Some(form) = self.form.clone() else {
            self.mode = Mode::Normal;
            return;
        };
        let return_mode = match form.kind {
            FormKind::ImportCandidate(_) => Mode::ImportReview,
            FormKind::ArtistTransferAlbum { .. } => Mode::ArtistDeletion,
            _ => Mode::Normal,
        };
        let values = form
            .fields
            .iter()
            .map(|field| field.value.trim().to_owned())
            .collect::<Vec<_>>();
        let number = |value: &str, label: &str| -> Result<Option<u32>, String> {
            if value.is_empty() {
                Ok(None)
            } else {
                value
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| format!("{label} must be a whole number."))
            }
        };
        let result: Result<String, String> = match form.kind {
            FormKind::Track {
                id,
                original_credit,
                original_credits,
            } => {
                if values[0].is_empty() {
                    Err("Title must not be empty.".to_owned())
                } else {
                    let owner = self.controller.track_artist_id(id);
                    let artist_name = values[1].clone();
                    let credits = if artist_name == original_credit {
                        original_credits
                    } else {
                        let credit_id = if artist_name.is_empty() {
                            None
                        } else if let Some(artist) = self
                            .controller
                            .artists()
                            .into_iter()
                            .find(|artist| artist.name().eq_ignore_ascii_case(&artist_name))
                        {
                            Some(artist.id())
                        } else {
                            self.controller.create_artist(&artist_name).ok()
                        };
                        let mut credits = owner.into_iter().collect::<Vec<_>>();
                        if let Some(credit) = credit_id
                            && !credits.contains(&credit)
                        {
                            credits.push(credit);
                        }
                        credits
                    };
                    if credits.is_empty() {
                        return self.reject_form("At least one artist credit is required.");
                    }
                    let numbers = number(&values[3], "Track number").and_then(|track| {
                        number(&values[4], "Disc number").map(|disc| (track, disc))
                    });
                    match numbers {
                        Ok((track_number, disc_number)) => {
                            let edit = TrackMetadataEdit {
                                title: Some(values[0].clone()),
                                artist_id: None,
                                credited_artist_ids: Some(credits),
                                album: if self.controller.track_album_id(id).is_some() {
                                    Some(values[2].clone())
                                } else {
                                    None
                                },
                                track_number,
                                disc_number,
                            };
                            self.controller
                                .edit_track_metadata(id, edit)
                                .map(|()| format!("Saved metadata for track {id}."))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            FormKind::Artist(id) => {
                if values[0].is_empty() {
                    Err("Artist name must not be empty.".to_owned())
                } else {
                    let aliases = values[1]
                        .split(';')
                        .map(str::trim)
                        .filter(|alias| !alias.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>();
                    let description = (!values[2].is_empty()).then(|| values[2].clone());
                    let metadata = self.controller.update_artist_metadata(
                        id,
                        &values[0],
                        description.as_deref(),
                        &aliases,
                    );
                    let current_image = self
                        .controller
                        .artist_image_path(id)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                    let image = match values[3].as_str() {
                        "" => Ok(()),
                        "-" => self.controller.remove_artist_image(id),
                        path if path == current_image => Ok(()),
                        path => self.controller.set_artist_image(id, Path::new(path)),
                    };
                    metadata
                        .and(image)
                        .map(|()| format!("Updated artist {id}."))
                }
            }
            FormKind::Album(id) => {
                if values[0].is_empty() {
                    Err("Album title must not be empty.".to_owned())
                } else {
                    match parse_release_type(&values[1]) {
                        Ok(release_type) => {
                            let metadata =
                                self.controller
                                    .update_album_metadata(id, &values[0], release_type);
                            let current_cover = self
                                .controller
                                .album_cover_path(id)
                                .map(|path| path.display().to_string())
                                .unwrap_or_default();
                            let cover = match values[2].as_str() {
                                "" => Ok(()),
                                "-" => self.controller.remove_album_cover(id),
                                path if path == current_cover => Ok(()),
                                path => self.controller.set_album_cover(id, Path::new(path)),
                            };
                            metadata.and(cover).map(|()| format!("Updated album {id}."))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            FormKind::PlaylistNew => {
                if values[0].is_empty() {
                    Err("Playlist name must not be empty.".to_owned())
                } else {
                    self.controller
                        .create_playlist(&values[0])
                        .map(|id| format!("Created playlist {id}."))
                }
            }
            FormKind::PlaylistRename {
                id,
                expected_name,
                expected_tracks,
            } => {
                if values[0].is_empty() {
                    Err("Playlist name must not be empty.".to_owned())
                } else {
                    self.controller
                        .rename_playlist(id, &values[0], &expected_name, &expected_tracks)
                        .map(|()| format!("Renamed playlist {id}."))
                }
            }
            FormKind::ArtistTransferAlbum { track, owner } => {
                if values[0].is_empty() {
                    Err("Destination album title must not be empty.".to_owned())
                } else {
                    match parse_release_type(&values[1]) {
                        Ok(release_type) => {
                            let Some(plan) = self.artist_deletion.as_mut() else {
                                return self.reject_form("The artist deletion review closed.");
                            };
                            match plan.set_transfer(
                                track,
                                owner,
                                ArtistDeletionDestination::New {
                                    title: values[0].clone(),
                                    release_type,
                                },
                            ) {
                                Ok(()) => {
                                    self.artist_delete_armed = None;
                                    Ok(format!(
                                        "Track {track} will transfer into a new album “{}”.",
                                        values[0]
                                    ))
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            FormKind::ImportCandidate(index) => {
                if values[0].is_empty() {
                    Err("Title must not be empty.".to_owned())
                } else {
                    let numbers = number(&values[3], "Track number").and_then(|track| {
                        number(&values[4], "Disc number").map(|disc| (track, disc))
                    });
                    match numbers {
                        Ok((track_number, disc_number)) => {
                            let Some(ui) = self.import_ui.as_mut() else {
                                return self.reject_form("The import review closed.");
                            };
                            let Some(plan) = ui.plan.as_mut() else {
                                return self.reject_form("The import review closed.");
                            };
                            let Some(candidate) = plan.candidates.get_mut(index) else {
                                return self
                                    .reject_form("The reviewed track is no longer present.");
                            };
                            candidate.set_title(&values[0]);
                            candidate.set_artist_name(
                                (!values[1].is_empty()).then(|| values[1].clone()),
                            );
                            candidate.set_album((!values[2].is_empty()).then(|| values[2].clone()));
                            candidate.set_track_number(track_number);
                            candidate.set_disc_number(disc_number);
                            Ok(format!("Updated reviewed track {}.", values[0]))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
        };
        match result {
            Ok(message) => {
                self.form = None;
                self.mode = return_mode;
                self.lifecycle_msg = Some(message);
            }
            Err(error) => self.reject_form(&error),
        }
    }

    fn reject_form(&mut self, error: &str) {
        if let Some(form) = self.form.as_mut() {
            form.error = Some(error.to_owned());
        }
    }

    fn handle_form_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c {
            return true;
        }
        match key.code {
            KeyCode::Esc => {
                let return_mode = self
                    .form
                    .as_ref()
                    .map(|form| match form.kind {
                        FormKind::ImportCandidate(_) => Mode::ImportReview,
                        FormKind::ArtistTransferAlbum { .. } => Mode::ArtistDeletion,
                        _ => Mode::Normal,
                    })
                    .unwrap_or(Mode::Normal);
                self.form = None;
                self.mode = return_mode;
                self.lifecycle_msg = Some("Edit cancelled; nothing was changed.".to_owned());
            }
            KeyCode::Enter => self.submit_form(),
            KeyCode::Tab | KeyCode::Down => {
                if let Some(form) = self.form.as_mut() {
                    form.focus = (form.focus + 1) % form.fields.len().max(1);
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(form) = self.form.as_mut() {
                    form.focus = form.focus.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(form) = self.form.as_mut()
                    && let Some(field) = form.fields.get_mut(form.focus)
                {
                    field.value.pop();
                    form.error = None;
                }
            }
            KeyCode::Char(character) => {
                if let Some(form) = self.form.as_mut()
                    && let Some(field) = form.fields.get_mut(form.focus)
                {
                    field.value.push(character);
                    form.error = None;
                }
            }
            _ => {}
        }
        false
    }

    fn library_header_identity(&self, _width: usize) -> String {
        let path = self
            .controller
            .canonical_root()
            .or_else(|| self.controller.portable_root())
            .or_else(|| self.controller.legacy_index_path());
        let Some(path) = path else {
            return self.library_full_identity();
        };
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        match self.controller.storage_mode() {
            StorageMode::Legacy => format!("legacy {name}"),
            StorageMode::Portable => format!("portable {name}"),
            StorageMode::Canonical => name,
        }
    }

    fn empty_rows_message(&self) -> String {
        if self.controller.tracks().is_empty() {
            return "Library is empty — press o to scan a folder, i to import files, or ? for help."
                .to_owned();
        }
        if self.favorites_only {
            if self.search.trim().is_empty() {
                "No favorite tracks — press F to show all tracks.".to_owned()
            } else {
                "No favorite tracks match the current search — Esc clears search, F shows all."
                    .to_owned()
            }
        } else if self.search.trim().is_empty() {
            "No tracks in this library view — press ? for help.".to_owned()
        } else {
            "No tracks match the current search — Esc clears it.".to_owned()
        }
    }

    fn open_info(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.info_title = title.into();
        self.info_lines = lines;
        self.info_scroll = 0;
        self.mode = Mode::InfoView;
    }

    fn open_queue_view(&mut self) {
        self.queue_cursor = self
            .controller
            .current_queue_entry_id()
            .and_then(|entry| self.controller.queue_entry_index(entry))
            .unwrap_or(0)
            .min(self.controller.queue_len().saturating_sub(1));
        self.queue_offset = self.queue_cursor;
        self.queue_follow = true;
        self.mode = Mode::QueueView;
        self.lifecycle_msg = Some(
            "Queue view · Enter play · d/Delete remove · Ctrl+↑/↓ reorder · Esc back".to_owned(),
        );
    }

    fn event_loop(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        loop {
            self.poll_import_worker();
            self.poll_audio_workers();
            self.controller.poll();
            self.draw(out)?;
            let active_visualizer = self.viz_visible
                && self.layout != LayoutMode::Compact
                && self.controller.player_state() == PlaybackState::Playing
                && self.controller.audio_status().is_ready();
            let interval = if active_visualizer { 50 } else { 120 };
            // Windows terminals report key releases too; act only on
            // presses (and repeats, for held keys).
            if event::poll(Duration::from_millis(interval))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && self.handle_key(key)
            {
                return Ok(());
            }
        }
    }

    fn handle_info_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c || key.code == KeyCode::Char('q') {
            return true;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('Q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.info_scroll = self.info_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.info_scroll = self.info_scroll.saturating_add(1);
            }
            KeyCode::PageUp => self.info_scroll = self.info_scroll.saturating_sub(10),
            KeyCode::PageDown => self.info_scroll = self.info_scroll.saturating_add(10),
            KeyCode::Home | KeyCode::Char('g') => self.info_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.info_scroll = self.info_lines.len().saturating_sub(1);
            }
            _ => {}
        }
        false
    }

    fn handle_queue_view_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c {
            return true;
        }
        let len = self.controller.queue_len();
        self.queue_cursor = self.queue_cursor.min(len.saturating_sub(1));
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            let delta = if key.code == KeyCode::Up { -1 } else { 1 };
            if let Some(entry) = self
                .controller
                .queue_entries()
                .get(self.queue_cursor)
                .copied()
            {
                match self.controller.move_queue_entry_by(entry.id(), delta) {
                    Ok(true) => {
                        if let Some(index) = self.controller.queue_entry_index(entry.id()) {
                            self.queue_cursor = index;
                        }
                        self.lifecycle_msg = Some(format!(
                            "Moved queue entry #{} by one position.",
                            entry.id()
                        ));
                    }
                    Ok(false) => {
                        self.lifecycle_msg = Some(
                            "That queue entry cannot move further in that direction.".to_owned(),
                        );
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            } else {
                self.lifecycle_msg = Some("The queue is empty.".to_owned());
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc | KeyCode::Char('Q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.queue_follow = false;
                self.queue_cursor = self.queue_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.queue_follow = false;
                self.queue_cursor = (self.queue_cursor + 1).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => self.queue_cursor = self.queue_cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.queue_cursor = (self.queue_cursor + 10).min(len.saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.queue_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.queue_cursor = len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(entry) = self
                    .controller
                    .queue_entries()
                    .get(self.queue_cursor)
                    .copied()
                {
                    match self.controller.play_queue_entry(entry.id()) {
                        Ok(()) => {
                            self.lifecycle_msg =
                                Some(format!("Playing queue entry #{}.", entry.id()));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("The queue is empty.".to_owned());
                }
            }
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some(entry) = self
                    .controller
                    .queue_entries()
                    .get(self.queue_cursor)
                    .copied()
                {
                    match self.controller.remove_queue_entry(entry.id()) {
                        Ok(true) => {
                            self.queue_cursor = self
                                .queue_cursor
                                .min(self.controller.queue_len().saturating_sub(1));
                            self.lifecycle_msg =
                                Some(format!("Removed queue entry #{}.", entry.id()));
                        }
                        Ok(false) => {
                            self.lifecycle_msg =
                                Some("That queue entry was already removed.".to_owned());
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("The queue is empty.".to_owned());
                }
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
            KeyCode::Char('s') => self.controller.stop(),
            KeyCode::Char('n') => {
                self.controller.next();
                self.follow_current_queue();
            }
            KeyCode::Char('p') => {
                self.controller.previous();
                self.follow_current_queue();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.controller.adjust_volume(0.05),
            KeyCode::Char('-') => self.controller.adjust_volume(-0.05),
            KeyCode::Char('J') => {
                self.queue_follow = true;
                self.follow_current_queue();
                self.lifecycle_msg = Some("Queue cursor follows the current entry.".to_owned());
            }
            KeyCode::Char('f') => {
                self.queue_follow = !self.queue_follow;
                self.lifecycle_msg = Some(if self.queue_follow {
                    "Queue follows playback.".to_owned()
                } else {
                    "Queue follow paused.".to_owned()
                });
            }
            KeyCode::Char('C') => {
                if len == 0 {
                    self.lifecycle_msg = Some("The queue is already empty.".to_owned());
                } else {
                    self.mode = Mode::ConfirmQueueClear;
                    self.lifecycle_msg = Some(format!(
                        "Clear all {len} queue entr(ies)? y confirms, any other key cancels."
                    ));
                }
            }
            _ => {}
        }
        false
    }

    /// Returns true when the user asked to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if self.mode == Mode::LyricsView {
            if ctrl_c || key.code == KeyCode::Char('q') {
                return true;
            }
            match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_sub(1);
                    self.lyrics_follow_ready = false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_add(1);
                    self.lyrics_follow_ready = false;
                }
                KeyCode::PageUp => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_sub(10);
                    self.lyrics_follow_ready = false;
                }
                KeyCode::PageDown => {
                    self.lyrics_scroll = self.lyrics_scroll.saturating_add(10);
                    self.lyrics_follow_ready = false;
                }
                KeyCode::Char('f') => {
                    self.follow_lyrics = !self.follow_lyrics;
                    self.lyrics_follow_ready = true;
                    self.save_preferences();
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.lyrics_scroll = 0;
                    self.lyrics_follow_ready = false;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.lyrics_scroll = self.lyrics_lines.len().saturating_sub(1);
                }
                KeyCode::Char('x') => self.begin_lyrics_export(),
                _ => {}
            }
            return false;
        }
        match self.mode {
            Mode::InfoView => return self.handle_info_key(key, ctrl_c),
            Mode::QueueView => return self.handle_queue_view_key(key, ctrl_c),
            Mode::ImportScanning | Mode::ImportPreparing => {
                return self.handle_import_progress_key(key, ctrl_c);
            }
            Mode::ImportReview => return self.handle_import_review_key(key, ctrl_c),
            Mode::ImportParticipants => {
                if ctrl_c {
                    return true;
                }
                let participants = self.import_participant_ids();
                match key.code {
                    KeyCode::Esc => self.mode = Mode::ImportReview,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.import_participant_cursor =
                            self.import_participant_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.import_participant_cursor = (self.import_participant_cursor + 1)
                            .min(participants.len().saturating_sub(1));
                    }
                    KeyCode::Char('a') => {
                        self.import_participant_pick_cursor = 0;
                        self.mode = Mode::ImportParticipantPick;
                    }
                    KeyCode::Char('r') => {
                        if let Some(artist) =
                            participants.get(self.import_participant_cursor).copied()
                        {
                            self.remove_import_participant(artist);
                        }
                    }
                    _ => {}
                }
                return false;
            }
            Mode::ImportParticipantPick => {
                if ctrl_c {
                    return true;
                }
                let artists = self.import_participant_candidates();
                match key.code {
                    KeyCode::Esc => self.mode = Mode::ImportParticipants,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.import_participant_pick_cursor =
                            self.import_participant_pick_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.import_participant_pick_cursor = (self.import_participant_pick_cursor
                            + 1)
                        .min(artists.len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if let Some(artist) =
                            artists.get(self.import_participant_pick_cursor).copied()
                        {
                            self.add_import_participant(artist);
                        }
                    }
                    _ => {}
                }
                return false;
            }
            Mode::FormInput => return self.handle_form_key(key, ctrl_c),
            Mode::CommandInput => return self.handle_command_key(key, ctrl_c),
            Mode::ConfirmPlaylist => return self.handle_playlist_confirm(key, ctrl_c),
            Mode::AlbumParticipants => {
                if ctrl_c {
                    return true;
                }
                let participants = self.album_participant_ids();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::Normal;
                        self.lifecycle_msg = Some("Album participants closed.".to_owned());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.album_participant_cursor =
                            self.album_participant_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.album_participant_cursor = (self.album_participant_cursor + 1)
                            .min(participants.len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if let Some(artist) =
                            participants.get(self.album_participant_cursor).copied()
                        {
                            self.mode = Mode::Normal;
                            self.detail = Some(Detail::Artist(artist));
                            self.reset_selection();
                        }
                    }
                    KeyCode::Char('a') => {
                        self.album_participant_pick_cursor = 0;
                        self.mode = Mode::AlbumParticipantPick;
                    }
                    KeyCode::Char('r') => {
                        if let Some(artist) =
                            participants.get(self.album_participant_cursor).copied()
                            && let Some(Detail::Album(album)) = self.detail
                        {
                            self.pending_participant_remove = Some((album, artist));
                            self.mode = Mode::ConfirmAlbumParticipantRemove;
                            self.lifecycle_msg = Some(
                                "Remove this Album participation? The Album, its tracks, audio and Track credits are unchanged. y confirms."
                                    .to_owned(),
                            );
                        }
                    }
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::AlbumParticipantPick => {
                if ctrl_c {
                    return true;
                }
                let artists = self.album_participant_candidates();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::AlbumParticipants;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.album_participant_pick_cursor =
                            self.album_participant_pick_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.album_participant_pick_cursor = (self.album_participant_pick_cursor
                            + 1)
                        .min(artists.len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if let Some(artist) =
                            artists.get(self.album_participant_pick_cursor).copied()
                        {
                            self.add_album_participant(artist);
                        }
                    }
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ConfirmAlbumParticipantRemove => {
                if ctrl_c {
                    return true;
                }
                if key.code == KeyCode::Char('y') {
                    self.remove_album_participant();
                } else {
                    self.pending_participant_remove = None;
                    self.mode = Mode::AlbumParticipants;
                    self.lifecycle_msg = Some("Participant removal cancelled.".to_owned());
                }
                return false;
            }
            Mode::TrackMovePick => {
                if ctrl_c {
                    return true;
                }
                let targets = self.move_targets();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::Normal;
                        self.move_track = None;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.move_cursor = self.move_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.move_cursor =
                            (self.move_cursor + 1).min(targets.len().saturating_sub(1));
                    }
                    KeyCode::Home | KeyCode::Char('g') => self.move_cursor = 0,
                    KeyCode::End | KeyCode::Char('G') => {
                        self.move_cursor = targets.len().saturating_sub(1);
                    }
                    KeyCode::Char('s') => {
                        if let Some(index) =
                            targets.iter().position(|target| target.album.is_none())
                        {
                            self.move_cursor = index;
                            self.plan_move_target();
                        }
                    }
                    KeyCode::Enter => self.plan_move_target(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::AlbumTransferPick => {
                if ctrl_c {
                    return true;
                }
                let count = self.album_transfer_artists().len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::Normal;
                        self.lifecycle_msg = Some("Album transfer cancelled.".to_owned());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.album_transfer_artist_cursor =
                            self.album_transfer_artist_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.album_transfer_artist_cursor =
                            (self.album_transfer_artist_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Enter => self.apply_album_transfer_artist(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::AlbumTransferReview => {
                if ctrl_c {
                    return true;
                }
                let count = self
                    .album_transfer
                    .as_ref()
                    .map_or(0, |plan| plan.tracks.len());
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.album_transfer = None;
                        self.mode = Mode::Normal;
                        self.lifecycle_msg = Some("Album transfer cancelled.".to_owned());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.album_transfer_cursor = self.album_transfer_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.album_transfer_cursor =
                            (self.album_transfer_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Char('c') => self.toggle_album_transfer_credit(),
                    KeyCode::Char('y') => self.execute_album_transfer(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ArtistOwnerPick => {
                if ctrl_c {
                    return true;
                }
                let count = self
                    .artist_deletion_track()
                    .map_or(0, |track| track.eligible_owners.len());
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.artist_transfer_owner = None;
                        self.mode = Mode::ArtistDeletion;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.artist_owner_cursor = self.artist_owner_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.artist_owner_cursor =
                            (self.artist_owner_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Enter => self.apply_artist_owner(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ArtistAlbumPick => {
                if ctrl_c {
                    return true;
                }
                let count = self.artist_transfer_albums().len() + 1;
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::ArtistOwnerPick;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.artist_album_cursor = self.artist_album_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.artist_album_cursor =
                            (self.artist_album_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Enter => self.apply_artist_album(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ImportAlbumPick => {
                if ctrl_c {
                    return true;
                }
                let count = self.import_album_targets().len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.mode = Mode::ImportReview;
                        self.lifecycle_msg = Some(
                            "Fixed-target selection cancelled; the review is unchanged.".to_owned(),
                        );
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.import_album_cursor = self.import_album_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.import_album_cursor =
                            (self.import_album_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Home | KeyCode::Char('g') => self.import_album_cursor = 0,
                    KeyCode::End | KeyCode::Char('G') => {
                        self.import_album_cursor = count.saturating_sub(1);
                    }
                    KeyCode::Enter => self.apply_import_album_target(),
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ArtistDeletion => {
                if ctrl_c {
                    return true;
                }
                let count = self.artist_deletion_track_count();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.artist_deletion = None;
                        self.artist_delete_armed = None;
                        self.mode = Mode::Normal;
                        self.lifecycle_msg = Some("Artist deletion cancelled.".to_owned());
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.artist_delete_cursor = self.artist_delete_cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.artist_delete_cursor =
                            (self.artist_delete_cursor + 1).min(count.saturating_sub(1));
                    }
                    KeyCode::Home | KeyCode::Char('g') => self.artist_delete_cursor = 0,
                    KeyCode::End | KeyCode::Char('G') => {
                        self.artist_delete_cursor = count.saturating_sub(1);
                    }
                    KeyCode::Char('t') => self.open_artist_owner_picker(),
                    KeyCode::Char('r') => self.resolve_artist_credit(),
                    KeyCode::Char('d') => self.resolve_artist_track_delete(),
                    KeyCode::Char('b') => {
                        if let Some(plan) = self.artist_deletion.as_mut() {
                            let confirmed = !plan.delete_owned_albums;
                            plan.set_delete_owned_albums(confirmed);
                            self.lifecycle_msg = Some(if confirmed {
                                "Owned Albums without other participants are selected for deletion."
                                    .to_owned()
                            } else {
                                "Owned-Album deletion is no longer selected.".to_owned()
                            });
                        }
                    }
                    KeyCode::Char('u') => self.reset_artist_track_decisions(),
                    KeyCode::Char('a') => self.auto_resolve_artist(),
                    KeyCode::Char('y') => {
                        if self
                            .artist_deletion
                            .as_ref()
                            .is_some_and(ArtistDeletionPlan::is_ready)
                        {
                            self.confirm_artist_deletion();
                        } else {
                            self.lifecycle_msg = Some(
                                self.artist_deletion
                                    .as_ref()
                                    .and_then(ArtistDeletionPlan::blocking_reason)
                                    .unwrap_or("Resolve every affected track first.")
                                    .to_owned(),
                            );
                        }
                    }
                    _ => {}
                }
                return key.code == KeyCode::Char('q');
            }
            Mode::ConfirmTrackMove
            | Mode::ConfirmTrackReplacement
            | Mode::ConfirmTrackDelete
            | Mode::ConfirmAlbumDelete
            | Mode::ConfirmArtistDelete => {
                if ctrl_c {
                    return true;
                }
                if key.code == KeyCode::Char('y') {
                    match self.mode {
                        Mode::ConfirmTrackMove => self.confirm_track_move(),
                        Mode::ConfirmTrackReplacement => self.confirm_track_replacement(),
                        Mode::ConfirmTrackDelete => self.confirm_track_deletion(),
                        Mode::ConfirmAlbumDelete => self.confirm_album_deletion(),
                        Mode::ConfirmArtistDelete => self.confirm_artist_deletion(),
                        _ => {}
                    }
                } else {
                    self.pending_move = None;
                    self.pending_replacement = None;
                    self.pending_track_delete = None;
                    self.pending_album_delete = None;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg = Some("Destructive action cancelled.".to_owned());
                }
                return false;
            }
            Mode::ConfirmQueueClear => {
                if ctrl_c {
                    return true;
                }
                if key.code == KeyCode::Char('y') {
                    let count = self.controller.clear_queue();
                    self.queue_cursor = 0;
                    self.queue_offset = 0;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg = Some(format!("Cleared {count} queue entr(ies)."));
                } else {
                    self.mode = Mode::QueueView;
                    self.lifecycle_msg = Some("Queue clear cancelled.".to_owned());
                }
                return false;
            }
            _ => {}
        }
        if self.mode != Mode::Normal {
            if ctrl_c {
                return true;
            }
            return self.handle_input_key(key, ctrl_c);
        }
        if ctrl_c {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    if let Some(Detail::Album(album)) = self.detail {
                        if let Some(track) = self.current_track() {
                            let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                            match self.controller.reorder_album_track(album, track, delta) {
                                Ok(()) => {
                                    if key.code == KeyCode::Up {
                                        self.cursor = self.cursor.saturating_sub(1);
                                    } else {
                                        self.cursor = (self.cursor + 1)
                                            .min(self.view_rows().len().saturating_sub(1));
                                    }
                                    self.lifecycle_msg =
                                        Some("Reordered the album track.".to_owned());
                                }
                                Err(error) => self.lifecycle_msg = Some(error),
                            }
                        }
                    } else if let Some(Detail::Playlist(id)) = self.detail {
                        if let Some(playlist) = self.controller.playlist(id) {
                            let tracks = playlist.track_ids.clone();
                            let from = self.cursor;
                            let to = if key.code == KeyCode::Up {
                                from.saturating_sub(1)
                            } else {
                                (from + 1).min(tracks.len().saturating_sub(1))
                            };
                            match self
                                .controller
                                .reorder_playlist_entry(id, from, to, &tracks)
                            {
                                Ok(()) => {
                                    self.cursor = to;
                                    self.lifecycle_msg =
                                        Some("Reordered the playlist entry.".to_owned());
                                }
                                Err(error) => self.lifecycle_msg = Some(error),
                            }
                        }
                    } else if let Some(entry) = self.controller.current_queue_entry_id() {
                        let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                        let _ = self.controller.move_queue_entry_by(entry, delta);
                    } else {
                        self.lifecycle_msg = Some("The queue is empty.".to_owned());
                    }
                }
                _ => {}
            }
            return false;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('v') => {
                    self.viz_mode = self.viz_mode.next();
                    self.save_preferences();
                    self.lifecycle_msg = Some(format!("Visualization: {}.", self.viz_mode.label()));
                }
                KeyCode::Up => {
                    self.viz_gain = (self.viz_gain + 0.25).min(2.5);
                    self.save_preferences();
                    self.lifecycle_msg =
                        Some(format!("Visualizer sensitivity: {:.2}×.", self.viz_gain));
                }
                KeyCode::Down => {
                    self.viz_gain = (self.viz_gain - 0.25).max(0.5);
                    self.save_preferences();
                    self.lifecycle_msg =
                        Some(format!("Visualizer sensitivity: {:.2}×.", self.viz_gain));
                }
                KeyCode::Char('t') => self.open_album_transfer_picker(),
                KeyCode::Char('p') => self.open_album_participants(),
                KeyCode::Char('m') => self.open_move_picker(),
                KeyCode::Char('r') => {
                    if let Some(id) = self.current_track() {
                        self.asset_track = Some(id);
                        self.input.clear();
                        self.mode = Mode::TrackReplacementInput;
                    } else {
                        self.lifecycle_msg =
                            Some("Select a track to repair or replace.".to_owned());
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(id) = self.current_track()
                        && let Some(track) = self.controller.track(id)
                    {
                        self.input = track_edit_form(&self.controller, id, track);
                        self.mode = Mode::TrackEditInput;
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(artist) = self
                        .current_track()
                        .and_then(|id| self.controller.track_artist_id(id))
                    {
                        self.input = artist_edit_form(&self.controller, artist);
                        self.mode = Mode::ArtistRenameInput;
                    }
                }
                KeyCode::Char('z') => {
                    if let Some(album) = self
                        .current_track()
                        .and_then(|id| self.controller.track_album_id(id))
                        && let Some(album) = self.controller.album(album)
                    {
                        self.input = album_edit_form(album.id, &album.title, album.release_type);
                        self.mode = Mode::AlbumEditInput;
                    }
                }
                _ => {}
            }
            return false;
        }
        let len = self.view_rows().len();
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.cursor = (self.cursor + 10).min(len.saturating_sub(1));
            }
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = len.saturating_sub(1),
            KeyCode::Tab => {
                let index = View::ALL
                    .iter()
                    .position(|view| *view == self.view)
                    .unwrap_or(0);
                self.switch_view(View::ALL[(index + 1) % View::ALL.len()]);
            }
            KeyCode::BackTab => {
                let index = View::ALL
                    .iter()
                    .position(|view| *view == self.view)
                    .unwrap_or(0);
                self.switch_view(View::ALL[(index + View::ALL.len() - 1) % View::ALL.len()]);
            }
            KeyCode::F(number @ 1..=8) => {
                if let Some(view) = View::ALL.get(usize::from(number) - 1) {
                    self.switch_view(*view);
                }
            }
            KeyCode::Enter => self.activate_row(),
            KeyCode::Left => {
                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    30
                } else {
                    5
                };
                let position = self
                    .controller
                    .position()
                    .saturating_sub(Duration::from_secs(delta));
                if let Err(error) = self.controller.seek(position) {
                    self.lifecycle_msg = Some(error);
                }
            }
            KeyCode::Right => {
                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    30
                } else {
                    5
                };
                let position = self.controller.position() + Duration::from_secs(delta);
                if let Err(error) = self.controller.seek(position) {
                    self.lifecycle_msg = Some(error);
                }
            }
            KeyCode::Char('m') => {
                self.layout = self.layout.next();
                self.save_preferences();
                self.lifecycle_msg = Some(format!(
                    "Layout: {} · m cycles layouts.",
                    self.layout.label()
                ));
            }
            KeyCode::Char('v') => {
                self.viz_visible = !self.viz_visible;
                self.save_preferences();
                self.lifecycle_msg = Some(if self.viz_visible {
                    "Visualizer shown.".to_owned()
                } else {
                    "Visualizer hidden; waveform sampling is paused.".to_owned()
                });
            }
            KeyCode::Char(':') => {
                self.mode = Mode::CommandInput;
                self.input.clear();
            }
            KeyCode::Char(' ') => self.controller.toggle_play_pause(),
            KeyCode::Char('a') => {
                if let Some(id) = self.current_track() {
                    match self.controller.enqueue_track(id) {
                        Ok(entry) => {
                            self.lifecycle_msg = Some(format!("Added queue entry #{entry}."));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to add to the queue.".to_owned());
                }
            }
            KeyCode::Char('x') => {
                if (self.view == View::Playlists
                    || matches!(self.detail, Some(Detail::Playlist(_))))
                    && let Some(playlist) = self.selected_playlist()
                    && let Some(found) = self.controller.playlist(playlist)
                {
                    self.playlist_action = Some(PlaylistAction::Delete {
                        id: found.id,
                        name: found.name.clone(),
                        tracks: found.track_ids.clone(),
                    });
                    self.mode = Mode::ConfirmPlaylist;
                    self.lifecycle_msg = Some(format!(
                        "Delete playlist “{}” without deleting tracks? y confirms.",
                        found.name
                    ));
                } else if let Some(id) = self.current_track() {
                    match self.controller.play_next_track(id) {
                        Ok(entry) => {
                            self.lifecycle_msg =
                                Some(format!("Queue entry #{entry} will play next."));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to play next.".to_owned());
                }
            }
            KeyCode::Char('s') => self.controller.stop(),
            KeyCode::Char('n') => {
                self.controller.next();
                self.follow_current();
            }
            KeyCode::Char('p') => {
                self.controller.previous();
                self.follow_current();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.controller.adjust_volume(0.05),
            KeyCode::Char('-') => self.controller.adjust_volume(-0.05),
            KeyCode::Char('R') => {
                let playlist = match self.detail {
                    Some(Detail::Playlist(id)) => Some(id),
                    _ if self.view == View::Playlists => match self.current_row() {
                        Some(Row::Playlist(id)) => Some(id),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(playlist) = playlist {
                    self.open_playlist_rename_form(playlist);
                } else {
                    self.controller.cycle_repeat_mode();
                }
            }
            KeyCode::Char('K') => self.toggle_crossfade(),
            KeyCode::Char(']') => self.adjust_crossfade_up(),
            KeyCode::Char('[') => self.adjust_crossfade_down(),
            KeyCode::Char('o') => {
                self.mode = Mode::ScanInput;
                self.input.clear();
            }
            KeyCode::Char('i') => {
                self.mode = Mode::ImportInput;
                self.input.clear();
            }
            KeyCode::Char('I') => {
                self.mode = Mode::ImportInput;
                self.input.clear();
            }
            KeyCode::Char('e') => {
                if let Some(id) = self.current_track() {
                    self.open_track_form(id);
                } else {
                    self.lifecycle_msg = Some("Select a track to edit.".to_owned());
                }
            }
            KeyCode::Char('y') => {
                let id = self
                    .current_track()
                    .or_else(|| self.controller.current_id());
                let Some(id) = id else {
                    self.lifecycle_msg = Some("Select a track to inspect lyrics.".to_owned());
                    return false;
                };
                self.open_lyrics_view(id);
            }
            KeyCode::Char('P') => {
                self.mode = Mode::ArtistCreateInput;
                self.input.clear();
            }
            KeyCode::Char('Y') => {
                let artist = match self.detail {
                    Some(Detail::Artist(id)) => Some(id),
                    _ => match self.current_row() {
                        Some(Row::Artist(id)) => Some(id),
                        _ => self
                            .current_track()
                            .and_then(|id| self.controller.track_artist_id(id)),
                    },
                };
                if let Some(artist) = artist {
                    self.open_artist_form(artist);
                } else {
                    self.lifecycle_msg = Some("Select an artist or track first.".to_owned());
                }
            }
            KeyCode::Char('Z') => {
                let album = match self.detail {
                    Some(Detail::Album(id)) => Some(id),
                    _ => match self.current_row() {
                        Some(Row::Album(id)) => Some(id),
                        _ => self
                            .current_track()
                            .and_then(|id| self.controller.track_album_id(id)),
                    },
                };
                if let Some(album) = album {
                    self.open_album_form(album);
                } else {
                    self.lifecycle_msg =
                        Some("Select an album or a track that belongs to one.".to_owned());
                }
            }
            KeyCode::Char('W') => {
                if let Some(id) = self.current_track() {
                    self.asset_track = Some(id);
                    self.input.clear();
                    self.mode = Mode::TrackArtworkInput;
                } else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                }
            }
            KeyCode::Char('S') => {
                if let Some(id) = self.current_track() {
                    self.asset_track = Some(id);
                    self.input.clear();
                    self.mode = Mode::TrackLyricsInput;
                } else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                }
            }
            KeyCode::Char('V') => {
                let artists = self.controller.artists();
                if artists.is_empty() {
                    self.lifecycle_msg = Some(
                        "No stable artists yet (artist metadata requires a canonical library)."
                            .to_owned(),
                    );
                } else {
                    let mut lines = vec![
                        "Artist relationships are derived from stable ArtistId references. Renaming \
                         an artist updates every Album and Track view without rewriting audio."
                            .to_owned(),
                        String::new(),
                    ];
                    for artist in artists {
                        let owned = self.controller.albums_owned_by(artist.id());
                        let participating = self.controller.albums_participating_for(artist.id());
                        let credited = self.controller.albums_credited_for(artist.id());
                        let track_count = self
                            .controller
                            .tracks()
                            .iter()
                            .filter(|(id, _)| {
                                self.controller.track_artist_id(*id) == Some(artist.id())
                                    || self.controller.track_credits(*id).contains(&artist.id())
                            })
                            .count();
                        lines.push(format!(
                            "ArtistId {} · {}{}",
                            artist.id(),
                            artist.name(),
                            if artist.aliases().is_empty() {
                                String::new()
                            } else {
                                format!(" · aliases: {}", artist.aliases().join(", "))
                            }
                        ));
                        lines.push(format!(
                            "  description: {} · image: {} · owned albums: {} · participating albums: {} · track-credit-only albums: {} · tracks: {}",
                            if self
                                .controller
                                .artist_description(artist.id())
                                .is_some_and(|text| !text.trim().is_empty())
                            {
                                "yes"
                            } else {
                                "no"
                            },
                            if self
                                .controller
                                .artist_image_path(artist.id())
                                .is_some_and(|path| path.is_file())
                            {
                                "yes"
                            } else {
                                "no"
                            },
                            owned.len(),
                            participating.len(),
                            credited.len(),
                            track_count,
                        ));
                        for album_id in owned.iter().chain(credited.iter()) {
                            if let Some(album) = self.controller.album(*album_id) {
                                lines.push(format!(
                                    "  · AlbumId {} “{}” · {} track(s){}",
                                    album.id,
                                    album.title,
                                    album.track_order.len(),
                                    if owned.contains(album_id) {
                                        ""
                                    } else {
                                        " · credited only"
                                    }
                                ));
                            }
                        }
                        lines.push(String::new());
                    }
                    lines.push(
                        "TUI: Y edits name, aliases, description and image of an existing \
                         artist; P creates an artist. Ownership transfer and deletion require \
                         the GUI."
                            .to_owned(),
                    );
                    self.open_info("Artists", lines);
                }
            }
            KeyCode::Char('d') if self.view == View::Settings && self.detail.is_none() => {
                self.refresh_audio_devices();
                self.lifecycle_msg = Some("Refreshing audio devices…".to_owned());
            }
            KeyCode::Char('c') => {
                if let Some(Detail::Playlist(id)) = self.detail
                    && let Some(playlist) = self.controller.playlist(id)
                {
                    self.playlist_action = Some(PlaylistAction::Clear {
                        id,
                        tracks: playlist.track_ids.clone(),
                    });
                    self.mode = Mode::ConfirmPlaylist;
                    self.lifecycle_msg =
                        Some("Clear this playlist without deleting tracks? y confirms.".to_owned());
                } else {
                    self.controller.dismiss_notice();
                    self.lifecycle_msg = None;
                }
            }
            KeyCode::Char('f') => {
                if let Some(id) = self.current_track() {
                    match self.controller.toggle_track_favorite(id) {
                        Ok(favorite) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some(format!(
                                "Track {id} {} favorite.",
                                if favorite { "marked" } else { "removed from" }
                            ));
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to toggle favorite.".to_owned());
                }
            }
            KeyCode::Char(digit @ '0'..='5') => {
                if let Some(id) = self.current_track() {
                    let rating = (digit != '0').then(|| digit.to_digit(10).unwrap_or(0) as u8);
                    match self.controller.set_track_rating(id, rating) {
                        Ok(()) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some(match rating {
                                Some(stars) => format!("Rated track {id} {stars}★."),
                                None => format!("Cleared rating for track {id}."),
                            });
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to rate.".to_owned());
                }
            }
            KeyCode::Char('F') => {
                self.favorites_only = !self.favorites_only;
                self.cursor = 0;
                self.offset = 0;
                self.lifecycle_msg = Some(if self.favorites_only {
                    "Showing favorites only (F toggles).".to_owned()
                } else {
                    "Showing all tracks (F toggles favorites).".to_owned()
                });
            }
            KeyCode::Char('H') => {
                let history = self.controller.playback_history();
                if history.is_empty() {
                    self.lifecycle_msg = Some(
                        "No meaningful playback history yet; history starts when this feature is used."
                            .to_owned(),
                    );
                } else {
                    let mut entries = history.entries().collect::<Vec<_>>();
                    entries.sort_by(|(left_id, left), (right_id, right)| {
                        right
                            .play_count
                            .cmp(&left.play_count)
                            .then_with(|| right.last_played_unix_ms.cmp(&left.last_played_unix_ms))
                            .then_with(|| left_id.cmp(right_id))
                    });
                    let summary = entries
                        .into_iter()
                        .take(5)
                        .map(|(id, entry)| {
                            format!(
                                "{}={}",
                                self.controller
                                    .track(id)
                                    .map(display_title)
                                    .unwrap_or_else(|| format!("Track {id}")),
                                entry.play_count
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.lifecycle_msg = Some(format!(
                        "Most played: {summary}. Total meaningful plays: {}.",
                        history.total_plays()
                    ));
                }
            }
            KeyCode::Char('L') => self.relink_first_unique(),
            KeyCode::Char('r') => self.controller.refresh(),
            KeyCode::Char('O') => {
                self.mode = Mode::OpenInput;
                self.input.clear();
            }
            KeyCode::Char('C') => {
                self.mode = Mode::CreateInput;
                self.input.clear();
            }
            KeyCode::Char('M') => {
                self.mode = Mode::MigrateSourceInput;
                self.input.clear();
            }
            KeyCode::Char('E') => {
                let albums = self.controller.albums();
                let standalone = self
                    .controller
                    .tracks()
                    .iter()
                    .filter(|(id, _)| self.controller.track_album_id(*id).is_none())
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                if albums.is_empty() && standalone.is_empty() {
                    self.lifecycle_msg =
                        Some("No canonical albums or standalone tracks.".to_owned());
                } else {
                    let mut lines = vec![
                        "Album membership and order are canonical; Track.album_id and \
                         Album.track_order are validated together."
                            .to_owned(),
                        String::new(),
                    ];
                    for album in albums {
                        let owner = self
                            .controller
                            .artist(album.owner_artist_id)
                            .map(|artist| format!("{} (ArtistId {})", artist.name(), artist.id()))
                            .unwrap_or_else(|| format!("ArtistId {}", album.owner_artist_id));
                        lines.push(format!(
                            "AlbumId {} · “{}” · {} · owner {} · artwork {} · {} track(s)",
                            album.id,
                            album.title,
                            album.release_type.as_str(),
                            owner,
                            if album.cover.is_some() { "yes" } else { "no" },
                            album.track_order.len(),
                        ));
                        for (position, track_id) in album.track_order.iter().enumerate() {
                            let title = self
                                .controller
                                .track(*track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  {}. {} · TrackId {}{}{}",
                                position + 1,
                                title,
                                track_id,
                                if self.controller.track_artwork_path(*track_id).is_some() {
                                    " · Track artwork override"
                                } else {
                                    ""
                                },
                                if self.controller.track_lyrics_path(*track_id).is_some() {
                                    " · lyrics"
                                } else {
                                    ""
                                },
                            ));
                        }
                        lines.push(String::new());
                    }
                    if !standalone.is_empty() {
                        lines.push(format!(
                            "Standalone Tracks · no Album membership · {} track(s)",
                            standalone.len()
                        ));
                        for track_id in standalone {
                            let title = self
                                .controller
                                .track(track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  · {} · TrackId {}{}{}",
                                title,
                                track_id,
                                if self.controller.track_artwork_path(track_id).is_some() {
                                    " · Track artwork"
                                } else {
                                    ""
                                },
                                if self.controller.track_lyrics_path(track_id).is_some() {
                                    " · lyrics"
                                } else {
                                    ""
                                },
                            ));
                        }
                        lines.push(String::new());
                    }
                    lines.push(
                        "Empty albums are retained deliberately. TUI: Z edits title, release \
                         type and cover. Track reorder, add/move/remove, ownership transfer and \
                         deletion require the GUI."
                            .to_owned(),
                    );
                    self.open_info("Albums", lines);
                }
            }
            KeyCode::Char('D') => {
                if let Some(id) = self.rows().get(self.cursor).copied() {
                    if let Some(album) = self
                        .controller
                        .track_album_id(id)
                        .and_then(|album_id| self.controller.album(album_id))
                    {
                        let owner = self
                            .controller
                            .artist(album.owner_artist_id)
                            .map(|artist| artist.name().to_owned())
                            .unwrap_or_else(|| format!("Artist {}", album.owner_artist_id));
                        let credited = self.controller.albums_credited_for(album.owner_artist_id);
                        let mut lines = vec![
                            format!("AlbumId {} · “{}”", album.id, album.title),
                            format!(
                                "{} · owner {} (ArtistId {}) · artwork {}",
                                album.release_type.as_str(),
                                owner,
                                album.owner_artist_id,
                                if album.cover.is_some() { "yes" } else { "no" },
                            ),
                            format!(
                                "{} ordered track(s) · participating: {} · credited-only albums for owner: {}",
                                album.track_order.len(),
                                album
                                    .participant_artist_ids
                                    .iter()
                                    .map(|artist| self
                                        .controller
                                        .artist(*artist)
                                        .map(|artist| artist.name().to_owned())
                                        .unwrap_or_else(|| format!("Artist {artist}")))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                credited.len()
                            ),
                            String::new(),
                        ];
                        for (position, track_id) in album.track_order.iter().enumerate() {
                            let title = self
                                .controller
                                .track(*track_id)
                                .map(display_title)
                                .unwrap_or_else(|| format!("missing TrackId {track_id}"));
                            lines.push(format!(
                                "  {}. {} · TrackId {} · artwork {} · lyrics {}",
                                position + 1,
                                title,
                                track_id,
                                if self.controller.track_artwork_path(*track_id).is_some() {
                                    "Track override"
                                } else if album.cover.is_some() {
                                    "inherited Album"
                                } else {
                                    "none"
                                },
                                if self.controller.track_lyrics_path(*track_id).is_some() {
                                    "yes"
                                } else {
                                    "no"
                                },
                            ));
                        }
                        lines.push(String::new());
                        lines.push(
                            "TUI: Z edits album metadata/cover. Track order, membership and \
                         Track movement use the GUI transactional workflows."
                                .to_owned(),
                        );
                        self.open_info("Selected album", lines);
                    } else if let Some(track) = self.controller.track(id) {
                        let mut lines = vec![
                            format!("Standalone Track · TrackId {id}"),
                            format!(
                                "Title: {} · artist: {} · artwork: {} · lyrics: {}",
                                display_title(track),
                                credit_display(&self.controller, id),
                                if self.controller.track_artwork_path(id).is_some() {
                                    "Track override"
                                } else {
                                    "generated fallback (no Album inheritance)"
                                },
                                if self.controller.track_lyrics_path(id).is_some() {
                                    "yes"
                                } else {
                                    "no"
                                },
                            ),
                            String::new(),
                            "This Track has no Album membership. Album artwork cannot be inherited; \
                             playback, queue, history, ratings and playlists remain available."
                                .to_owned(),
                            "TUI: W sets/removes Track artwork and S sets/removes managed \
                             lyrics. Adding it to an Album or moving it use the GUI membership \
                             workflow."
                                .to_owned(),
                        ];
                        lines.push(String::new());
                        self.open_info("Selected standalone Track", lines);
                    } else {
                        self.lifecycle_msg =
                            Some("The selected track is no longer present.".to_owned());
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                }
            }
            KeyCode::Char('T') => {
                self.lifecycle_msg = Some(
                    "Ownership transfer and artist deletion require the GUI review/confirm \
                     workflow; the TUI never bypasses canonical transaction validation."
                        .to_owned(),
                );
            }
            KeyCode::Char('B') => {
                self.lifecycle_msg = Some(
                    "Legacy runtime switching is disabled. Migrate an old index with M; \
                     the source is left untouched."
                        .to_owned(),
                );
            }
            KeyCode::Char('U') => {
                if !self.controller.legacy_index_write_protected() {
                    self.lifecycle_msg =
                        Some("The active library index is not write-protected.".to_owned());
                } else {
                    self.lifecycle_msg = Some(
                        "U will move the unreadable index to a preserved backup and enable \
                         scanning/saving. Press y to confirm, Esc to cancel."
                            .to_owned(),
                    );
                    self.mode = Mode::ConfirmPreserveIndex;
                }
            }
            KeyCode::Char('A') => {
                if self.controller.portable_recovery().is_none() {
                    self.lifecycle_msg =
                        Some("There is no portable recovery marker to acknowledge.".to_owned());
                } else {
                    self.lifecycle_msg = Some(
                        "A will replace the portable catalogue from its preserved backup and \
                         re-enable writes. Press y to confirm, Esc to cancel."
                            .to_owned(),
                    );
                    self.mode = Mode::ConfirmAcknowledgeRecovery;
                }
            }
            KeyCode::Char('w') => {
                if let Some(id) = self.current_track() {
                    let playlists = self.controller.playlists();
                    match playlists.as_slice() {
                        [] => {
                            self.lifecycle_msg = Some(
                                "No playlists yet — open Playlists (F4) and press N.".to_owned(),
                            );
                        }
                        [playlist] => {
                            match self.controller.add_tracks_to_playlist(
                                playlist.id,
                                &[id],
                                &playlist.track_ids,
                            ) {
                                Ok(_) => {
                                    self.lifecycle_msg =
                                        Some(format!("Added the track to “{}”.", playlist.name));
                                }
                                Err(error) => self.lifecycle_msg = Some(error),
                            }
                        }
                        _ => {
                            self.lifecycle_msg =
                                Some("Choose a playlist with :playlist <name>.".to_owned());
                        }
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to add to a playlist.".to_owned());
                }
            }
            KeyCode::Delete => {
                if matches!(self.detail, Some(Detail::Artist(_))) {
                    self.open_artist_deletion();
                } else if matches!(self.detail, Some(Detail::Album(_)))
                    && self.current_track().is_none()
                {
                    self.open_album_deletion();
                } else if self.current_track().is_some()
                    && !matches!(self.detail, Some(Detail::Playlist(_)))
                {
                    self.open_track_deletion();
                } else {
                    self.lifecycle_msg =
                        Some("Select a track, empty album or artist to delete.".to_owned());
                }
            }
            KeyCode::Char('N') => {
                if self.view == View::Playlists {
                    self.open_playlist_create_form();
                } else {
                    self.lifecycle_msg =
                        Some("Open Playlists (F4) and press N to create one.".to_owned());
                }
            }
            KeyCode::Char('l') => {
                let playlist = self.selected_playlist();
                if let Some(playlist) = playlist {
                    let start = self
                        .current_track()
                        .and_then(|track| {
                            self.controller.playlist(playlist).and_then(|playlist| {
                                playlist.track_ids.iter().position(|id| *id == track)
                            })
                        })
                        .unwrap_or(0);
                    match self.controller.play_playlist(playlist, start) {
                        Ok(()) => {
                            self.lifecycle_msg =
                                Some("Playing playlist from the selected position.".to_owned());
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a playlist first.".to_owned());
                }
            }
            KeyCode::Char('d') => {
                if let Some(Detail::Playlist(id)) = self.detail
                    && let Some(playlist) = self.controller.playlist(id)
                    && let Some(track) = self.current_track()
                    && let Some(position) = playlist
                        .track_ids
                        .iter()
                        .position(|candidate| *candidate == track)
                {
                    self.playlist_action = Some(PlaylistAction::Remove {
                        id,
                        position,
                        tracks: playlist.track_ids.clone(),
                    });
                    self.mode = Mode::ConfirmPlaylist;
                    self.lifecycle_msg = Some(
                        "Remove the selected entry from this playlist? y confirms.".to_owned(),
                    );
                } else {
                    self.lifecycle_msg =
                        Some("Select a playlist-entry track to remove.".to_owned());
                }
            }
            KeyCode::Char('?') => {
                let library = self.library_full_identity();
                let lines = help_lines(&self.controller, &library);
                self.open_info("Flux TUI help", lines);
            }
            KeyCode::Char('Q') => self.open_queue_view(),
            KeyCode::Char('X') => {
                if self.controller.storage_mode() == StorageMode::Canonical {
                    self.controller.refresh();
                    self.lifecycle_msg = Some(
                        "Refreshed the canonical library and validated its resources.".to_owned(),
                    );
                } else if let Ok(report) = self.controller.reconcile_portable_library() {
                    self.lifecycle_msg = Some(reconcile_summary(&report));
                    self.last_reconcile = Some(report);
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::SearchInput;
                self.input.clone_from(&self.search);
            }
            KeyCode::Esc => {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.reset_selection();
                    self.lifecycle_msg = Some("Search cleared.".to_owned());
                } else if self.detail.is_some() {
                    self.detail = None;
                    self.reset_selection();
                    self.lifecycle_msg = Some("Back to the library view.".to_owned());
                } else if self.active_collection.is_some() {
                    self.active_collection = None;
                    self.reset_selection();
                    self.lifecycle_msg = Some("Showing all tracks.".to_owned());
                } else {
                    self.lifecycle_msg = Some(
                        "No active search. Esc cancels an input or closes a view; / starts a search."
                            .to_owned(),
                    );
                }
            }
            KeyCode::Char(command) => {
                self.lifecycle_msg =
                    Some(format!("Unknown command '{command}'. Press ? for help."));
            }
            _ => {}
        }
        false
    }

    fn activate_row(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        match row {
            Row::Track(id) => {
                let title = self
                    .controller
                    .track(id)
                    .map(display_title)
                    .unwrap_or_else(|| format!("Track {id}"));
                self.controller.play_track(id);
                self.lifecycle_msg = Some(format!("Playing {title}."));
            }
            Row::LyricsTrack(id) => self.open_lyrics_view(id),
            Row::Artist(id) => {
                self.detail = Some(Detail::Artist(id));
                self.reset_selection();
            }
            Row::Album(id) => {
                self.detail = Some(Detail::Album(id));
                self.reset_selection();
            }
            Row::Playlist(id) => {
                self.detail = Some(Detail::Playlist(id));
                self.reset_selection();
            }
            Row::Collection(collection) => {
                self.active_collection = Some(collection);
                self.view = View::Tracks;
                self.detail = None;
                self.reset_selection();
                self.lifecycle_msg = Some(format!("Collection: {}.", collection.label()));
            }
            Row::Setting(setting) => self.toggle_setting(setting),
            Row::AudioDevice(None) => self.select_audio_output(None),
            Row::AudioDevice(Some(index)) => {
                if let Some(device) = self.audio_devices.get(index) {
                    self.select_audio_output(device.id.clone());
                }
            }
        }
    }

    fn open_lyrics_view(&mut self, id: TrackId) {
        let Some(raw) = self.controller.track_lyrics(id) else {
            self.lifecycle_msg = Some("This track has no managed lyrics.".to_owned());
            return;
        };
        let document = match flux_app::LyricsDocument::parse(&raw) {
            Ok(document) => document,
            Err(error) => {
                self.lifecycle_msg = Some(format!("Lyrics cannot be parsed: {error}"));
                return;
            }
        };
        self.lyrics_title = self
            .controller
            .track(id)
            .map(|track| {
                format!(
                    "Lyrics · {} · {} · {}",
                    display_title(track),
                    credit_display(&self.controller, id),
                    match document.kind() {
                        flux_app::LyricsKind::Synced => "synchronized",
                        flux_app::LyricsKind::Plain => "plain text",
                    }
                )
            })
            .unwrap_or_else(|| format!("Lyrics for track {id}"));
        self.lyrics_lines = document
            .lines
            .iter()
            .map(|line| match line.timestamp_ms {
                Some(timestamp) => format!("{}  {}", format_lyrics_timestamp(timestamp), line.text),
                None => line.text.clone(),
            })
            .collect();
        self.lyrics_timestamps = document
            .lines
            .iter()
            .map(|line| line.timestamp_ms)
            .collect();
        if self.lyrics_lines.is_empty() {
            self.lyrics_lines.push("(empty lyrics file)".to_owned());
            self.lyrics_timestamps.push(None);
        }
        self.lyrics_track = Some(id);
        self.lyrics_scroll = 0;
        self.lyrics_follow_ready = true;
        self.mode = Mode::LyricsView;
    }

    fn handle_command_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c {
            return true;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.lifecycle_msg = Some("Command cancelled; nothing changed.".to_owned());
            }
            KeyCode::Enter => {
                let command = std::mem::take(&mut self.input);
                self.mode = Mode::Normal;
                self.run_command(&command);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            _ => {}
        }
        false
    }

    fn run_command(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            self.lifecycle_msg = Some("No command entered.".to_owned());
            return;
        };
        let argument = parts.next();
        match name.to_lowercase().as_str() {
            "tracks" => self.switch_view(View::Tracks),
            "artists" => self.switch_view(View::Artists),
            "albums" => self.switch_view(View::Albums),
            "playlists" => self.switch_view(View::Playlists),
            "collections" => self.switch_view(View::Collections),
            "lyrics" => self.switch_view(View::Lyrics),
            "settings" => self.switch_view(View::Settings),
            "queue" => self.open_queue_view(),
            "help" => {
                let library = self.library_full_identity();
                let lines = help_lines(&self.controller, &library);
                self.open_info("Flux TUI help", lines);
            }
            "viz" | "visualizer" => match argument.map(str::to_lowercase).as_deref() {
                Some("on") | None => {
                    self.viz_visible = true;
                    self.save_preferences();
                    self.lifecycle_msg = Some("Visualizer shown.".to_owned());
                }
                Some("off") => {
                    self.viz_visible = false;
                    self.save_preferences();
                    self.lifecycle_msg = Some("Visualizer hidden.".to_owned());
                }
                Some("waveform") => {
                    self.viz_mode = VizMode::Waveform;
                    self.viz_visible = true;
                    self.save_preferences();
                    self.lifecycle_msg = Some("Visualization: Waveform.".to_owned());
                }
                Some("bars") => {
                    self.viz_mode = VizMode::Bars;
                    self.viz_visible = true;
                    self.save_preferences();
                    self.lifecycle_msg = Some("Visualization: Bars.".to_owned());
                }
                Some("pulse") => {
                    self.viz_mode = VizMode::Pulse;
                    self.viz_visible = true;
                    self.save_preferences();
                    self.lifecycle_msg = Some("Visualization: Pulse.".to_owned());
                }
                Some(other) => {
                    self.lifecycle_msg = Some(format!(
                        "Unknown visualization '{other}'. Use on/off/waveform/bars/pulse."
                    ));
                }
            },
            "layout" => {
                let layout = match argument.map(str::to_lowercase).as_deref() {
                    Some("standard") => Some(LayoutMode::Standard),
                    Some("library") => Some(LayoutMode::Library),
                    Some("player") => Some(LayoutMode::Player),
                    Some("visualizer") => Some(LayoutMode::Visualizer),
                    Some("compact") => Some(LayoutMode::Compact),
                    None => {
                        self.layout = self.layout.next();
                        Some(self.layout)
                    }
                    Some(_) => None,
                };
                if let Some(layout) = layout {
                    self.layout = layout;
                    self.save_preferences();
                    self.lifecycle_msg = Some(format!("Layout: {}.", layout.label()));
                } else {
                    self.lifecycle_msg = Some(
                        "Unknown layout. Use standard, library, player, visualizer or compact."
                            .to_owned(),
                    );
                }
            }
            "play" => self.controller.toggle_play_pause(),
            "pause" => self.controller.toggle_play_pause(),
            "stop" => self.controller.stop(),
            "next" => {
                self.controller.next();
                self.follow_current();
            }
            "prev" | "previous" => {
                self.controller.previous();
                self.follow_current();
            }
            "seek" => {
                let Some(value) = argument else {
                    self.lifecycle_msg =
                        Some("Usage: :seek <seconds> or :seek +10 / -10".to_owned());
                    return;
                };
                let result = if let Some(relative) = value.strip_prefix('+') {
                    relative
                        .parse::<u64>()
                        .map(|seconds| self.controller.position() + Duration::from_secs(seconds))
                } else if let Some(relative) = value.strip_prefix('-') {
                    relative.parse::<u64>().map(|seconds| {
                        self.controller
                            .position()
                            .saturating_sub(Duration::from_secs(seconds))
                    })
                } else {
                    value.parse::<u64>().map(Duration::from_secs)
                };
                match result {
                    Ok(position) => {
                        if let Err(error) = self.controller.seek(position) {
                            self.lifecycle_msg = Some(error);
                        }
                    }
                    Err(_) => {
                        self.lifecycle_msg = Some("Seek expects whole seconds.".to_owned());
                    }
                }
            }
            "vol" | "volume" => {
                if let Some(value) = argument.and_then(|value| value.parse::<f32>().ok()) {
                    self.controller.set_volume((value / 100.0).clamp(0.0, 1.0));
                } else {
                    self.lifecycle_msg = Some("Usage: :vol <0-100>".to_owned());
                }
            }
            "rate" => {
                let rating = argument.and_then(|value| value.parse::<u8>().ok());
                if let Some(id) = self.current_track() {
                    match self
                        .controller
                        .set_track_rating(id, rating.filter(|value| *value <= 5))
                    {
                        Ok(()) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some("Rating updated.".to_owned());
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track to rate.".to_owned());
                }
            }
            "favorite" => {
                if let Some(id) = self.current_track() {
                    match self.controller.toggle_track_favorite(id) {
                        Ok(favorite) => {
                            *self.rows_cache.borrow_mut() = None;
                            self.lifecycle_msg = Some(if favorite {
                                "Marked as favorite.".to_owned()
                            } else {
                                "Removed from favorites.".to_owned()
                            });
                        }
                        Err(error) => self.lifecycle_msg = Some(error),
                    }
                } else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                }
            }
            "search" => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                self.search = match argument {
                    Some(first) if rest.is_empty() => first.to_owned(),
                    Some(first) => format!("{first} {rest}"),
                    None => rest,
                };
                self.cursor = 0;
                self.save_preferences();
            }
            "library" => {
                self.mode = Mode::OpenInput;
                self.input.clear();
            }
            "new-playlist" | "newplaylist" => self.open_playlist_create_form(),
            "playlist" => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                let name = match argument {
                    Some(first) if rest.is_empty() => first.to_owned(),
                    Some(first) => format!("{first} {rest}"),
                    None => rest,
                };
                let Some(id) = self.current_track() else {
                    self.lifecycle_msg = Some("Select a track first.".to_owned());
                    return;
                };
                let Some(playlist) = self
                    .controller
                    .playlists()
                    .into_iter()
                    .find(|playlist| playlist.name.eq_ignore_ascii_case(&name))
                else {
                    self.lifecycle_msg = Some(format!("No playlist named '{name}'."));
                    return;
                };
                match self.controller.add_tracks_to_playlist(
                    playlist.id,
                    &[id],
                    &playlist.track_ids,
                ) {
                    Ok(_) => {
                        self.lifecycle_msg = Some(format!("Added track to “{}”.", playlist.name));
                    }
                    Err(error) => self.lifecycle_msg = Some(error),
                }
            }
            _ => {
                self.lifecycle_msg = Some(format!("Unknown command '{name}'. Press ? for help."));
            }
        }
    }

    fn move_targets(&self) -> Vec<MoveTarget> {
        let Some(track) = self.move_track.or_else(|| self.current_track()) else {
            return Vec::new();
        };
        let Some(track_value) = self.controller.track(track) else {
            return Vec::new();
        };
        let owner = self.controller.track_artist_id(track);
        let current_album = self.controller.track_album_id(track);
        let mut targets = Vec::new();
        if let Some(owner) = owner {
            let mut albums = self
                .controller
                .albums_owned_by(owner)
                .into_iter()
                .filter_map(|id| self.controller.album(id))
                .collect::<Vec<_>>();
            albums.sort_by(|left, right| {
                left.title
                    .to_lowercase()
                    .cmp(&right.title.to_lowercase())
                    .then_with(|| left.id.cmp(&right.id))
            });
            for album in albums {
                targets.push(MoveTarget {
                    album: Some(album.id),
                    label: format!(
                        "{} · {}",
                        album.title,
                        count_label(album.track_order.len(), "track")
                    ),
                    tracks: album.track_order.len(),
                    current: current_album == Some(album.id),
                });
            }
        }
        targets.push(MoveTarget {
            album: None,
            label: format!("Standalone · {}", display_title(track_value)),
            tracks: 0,
            current: current_album.is_none(),
        });
        targets
    }

    fn open_move_picker(&mut self) {
        let Some(track) = self.current_track() else {
            self.lifecycle_msg = Some("Select a track to move.".to_owned());
            return;
        };
        self.move_track = Some(track);
        self.move_cursor = 0;
        self.mode = Mode::TrackMovePick;
    }

    fn plan_move_target(&mut self) {
        let Some(track) = self.move_track else {
            self.mode = Mode::Normal;
            return;
        };
        let targets = self.move_targets();
        let Some(target) = targets.get(self.move_cursor) else {
            self.mode = Mode::Normal;
            return;
        };
        let insertion = target.tracks;
        match self
            .controller
            .plan_track_move(track, target.album, insertion)
        {
            Ok(plan) => {
                self.lifecycle_msg = Some(format!(
                    "Review: {} → {} at position {}. Press y to confirm.",
                    plan.from_album_title.as_deref().unwrap_or("Standalone"),
                    plan.to_album_title.as_deref().unwrap_or("Standalone"),
                    plan.insertion_index + 1
                ));
                self.pending_move = Some(plan);
                self.mode = Mode::ConfirmTrackMove;
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn open_track_deletion(&mut self) {
        let Some(track) = self.current_track() else {
            self.lifecycle_msg = Some("Select a track to delete.".to_owned());
            return;
        };
        match self.controller.plan_track_deletion(track) {
            Ok(plan) => {
                self.lifecycle_msg = Some(format!(
                    "Delete “{}”? Managed audio, lyrics, artwork, rating and queue entries will be removed; the album and artist remain. y confirms.",
                    plan.inner.title
                ));
                self.pending_track_delete = Some(plan);
                self.mode = Mode::ConfirmTrackDelete;
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn open_album_participants(&mut self) {
        let Some(Detail::Album(_)) = self.detail else {
            self.lifecycle_msg = Some("Open an Album to manage participants.".to_owned());
            return;
        };
        self.album_participant_cursor = 0;
        self.mode = Mode::AlbumParticipants;
        self.lifecycle_msg = Some(
            "Album participants — ↑/↓ select · Enter opens Artist · a adds · r removes · Esc back"
                .to_owned(),
        );
    }

    fn album_participant_ids(&self) -> Vec<ArtistId> {
        let Some(Detail::Album(album)) = self.detail else {
            return Vec::new();
        };
        self.controller.album_participants(album)
    }

    fn album_participant_candidates(&self) -> Vec<ArtistId> {
        let participants = self.album_participant_ids();
        self.controller
            .artists()
            .into_iter()
            .map(|artist| artist.id())
            .filter(|artist| !participants.contains(artist))
            .collect()
    }

    fn add_album_participant(&mut self, artist: ArtistId) {
        let Some(Detail::Album(album)) = self.detail else {
            return;
        };
        match self.controller.add_album_participant(album, artist) {
            Ok(()) => {
                self.album_participant_cursor = 0;
                self.mode = Mode::AlbumParticipants;
                let name = self
                    .controller
                    .artist(artist)
                    .map(|artist| artist.name().to_owned())
                    .unwrap_or_else(|| format!("Artist {artist}"));
                self.lifecycle_msg = Some(format!(
                    "Added {name} to the Album participants. Track credits are unchanged."
                ));
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn remove_album_participant(&mut self) {
        let Some((album, artist)) = self.pending_participant_remove.take() else {
            self.mode = Mode::AlbumParticipants;
            return;
        };
        match self.controller.remove_album_participant(album, artist) {
            Ok(()) => {
                self.album_participant_cursor = self.album_participant_cursor.saturating_sub(1);
                self.mode = Mode::AlbumParticipants;
                self.lifecycle_msg = Some(
                    "Removed the Album participation association. The Album, audio and Track credits are unchanged."
                        .to_owned(),
                );
            }
            Err(error) => {
                self.mode = Mode::AlbumParticipants;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn open_album_deletion(&mut self) {
        let Some(Detail::Album(album)) = self.detail else {
            self.lifecycle_msg = Some("Open an album to delete it.".to_owned());
            return;
        };
        let Some(value) = self.controller.album(album) else {
            self.lifecycle_msg = Some("The album is no longer present.".to_owned());
            return;
        };
        if !value.track_order.is_empty() {
            self.lifecycle_msg = Some(
                "Only empty albums can be deleted. Move or delete their tracks first.".to_owned(),
            );
            return;
        }
        self.pending_album_delete = Some(album);
        self.mode = Mode::ConfirmAlbumDelete;
        self.lifecycle_msg = Some(format!(
            "Delete the empty album “{}”? AlbumId is retired; no tracks or files are touched. y confirms.",
            value.title
        ));
    }

    fn open_artist_deletion(&mut self) {
        let Some(Detail::Artist(artist)) = self.detail else {
            self.lifecycle_msg = Some("Open an artist to delete it.".to_owned());
            return;
        };
        match self.controller.plan_artist_deletion(artist) {
            Ok(plan) => {
                let name = plan.artist_name.clone();
                let affected = plan.owned_tracks.len() + plan.credited_tracks.len();
                let owned_albums = plan.owned_albums.len();
                let participant_albums = plan.participant_albums.len();
                let has_albums =
                    !plan.owned_albums.is_empty() || !plan.participant_albums.is_empty();
                self.artist_deletion = Some(plan);
                self.artist_delete_cursor = 0;
                self.artist_delete_armed = None;
                if affected == 0 && !has_albums {
                    self.mode = Mode::ConfirmArtistDelete;
                    self.lifecycle_msg = Some(format!(
                        "Delete artist “{name}”? No tracks or albums are affected. y confirms."
                    ));
                } else {
                    self.mode = Mode::ArtistDeletion;
                    self.lifecycle_msg = Some(format!(
                        "{affected} track(s) plus {} owned and {} shared Album association(s) need review. a auto-resolves safe track decisions; b toggles owned-Album deletion; y publishes when ready.",
                        owned_albums, participant_albums
                    ));
                }
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn confirm_track_move(&mut self) {
        let Some(plan) = self.pending_move.take() else {
            self.mode = Mode::Normal;
            return;
        };
        match self.controller.execute_track_move(&plan) {
            Ok(report) => {
                self.detail = report.to_album_id.map(Detail::Album);
                self.mode = Mode::Normal;
                self.reset_selection();
                self.lifecycle_msg = Some("Moved the track and preserved its identity.".to_owned());
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn plan_track_replacement(&mut self, path_text: &str) {
        let Some(id) = self.asset_track else {
            self.mode = Mode::Normal;
            return;
        };
        let path = PathBuf::from(path_text.trim());
        if path.as_os_str().is_empty() {
            self.lifecycle_msg = Some("Enter a replacement audio path.".to_owned());
            self.mode = Mode::TrackReplacementInput;
            return;
        }
        match self.controller.plan_track_audio_replacement(id, &path) {
            Ok(plan) => {
                let title = self
                    .controller
                    .track(id)
                    .map(display_title)
                    .unwrap_or_else(|| format!("Track {id}"));
                self.lifecycle_msg = Some(format!(
                    "Review: replace managed audio for “{title}” from {}. TrackId and references are preserved. y confirms.",
                    path.display()
                ));
                self.pending_replacement = Some(plan);
                self.mode = Mode::ConfirmTrackReplacement;
            }
            Err(error) => {
                self.lifecycle_msg = Some(error);
                self.mode = Mode::TrackReplacementInput;
            }
        }
    }

    fn confirm_track_replacement(&mut self) {
        let Some(plan) = self.pending_replacement.take() else {
            self.mode = Mode::Normal;
            return;
        };
        match self.controller.execute_track_audio_replacement(&plan) {
            Ok(report) => {
                self.mode = Mode::Normal;
                self.asset_track = None;
                self.reset_selection();
                self.lifecycle_msg = Some(if report.restored_missing_audio {
                    "Repaired the missing managed audio; the track kept its identity.".to_owned()
                } else {
                    "Replaced the managed audio; the track kept its identity.".to_owned()
                });
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn confirm_track_deletion(&mut self) {
        let Some(plan) = self.pending_track_delete.take() else {
            self.mode = Mode::Normal;
            return;
        };
        match self.controller.delete_track(&plan) {
            Ok(report) => {
                self.mode = Mode::Normal;
                self.reset_selection();
                self.lifecycle_msg = Some(if report.cleanup_errors.is_empty() {
                    format!("Deleted track {}.", report.track_id)
                } else {
                    format!(
                        "Track {} was deleted; managed-resource cleanup needs attention: {}",
                        report.track_id,
                        report.cleanup_errors.join("; ")
                    )
                });
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn confirm_album_deletion(&mut self) {
        let Some(album) = self.pending_album_delete.take() else {
            self.mode = Mode::Normal;
            return;
        };
        match self.controller.delete_empty_album(album) {
            Ok(()) => {
                self.detail = None;
                self.mode = Mode::Normal;
                self.reset_selection();
                self.lifecycle_msg = Some("Deleted the empty album.".to_owned());
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn artist_deletion_track_count(&self) -> usize {
        self.artist_deletion.as_ref().map_or(0, |plan| {
            plan.owned_tracks.len() + plan.credited_tracks.len()
        })
    }

    fn album_transfer_artists(&self) -> Vec<ArtistId> {
        let Some(Detail::Album(album)) = self.detail else {
            return Vec::new();
        };
        let Some(current) = self.controller.album_owner(album) else {
            return Vec::new();
        };
        self.controller
            .artists()
            .into_iter()
            .map(|artist| artist.id())
            .filter(|artist| *artist != current)
            .collect()
    }

    fn open_album_transfer_picker(&mut self) {
        if !matches!(self.detail, Some(Detail::Album(_))) {
            self.lifecycle_msg = Some("Open an album to transfer ownership.".to_owned());
            return;
        }
        self.album_transfer_artist_cursor = 0;
        self.mode = Mode::AlbumTransferPick;
    }

    fn apply_album_transfer_artist(&mut self) {
        let Some(Detail::Album(album)) = self.detail else {
            self.mode = Mode::Normal;
            return;
        };
        let artists = self.album_transfer_artists();
        let Some(target) = artists.get(self.album_transfer_artist_cursor).copied() else {
            self.mode = Mode::Normal;
            return;
        };
        match self.controller.plan_album_transfer(album, target) {
            Ok(plan) => {
                self.album_transfer_cursor = 0;
                self.album_transfer = Some(plan);
                self.mode = Mode::AlbumTransferReview;
                self.lifecycle_msg = Some(
                    "Review the new owner and per-track credit decisions. c toggles a required destination credit; y executes when ready."
                        .to_owned(),
                );
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn toggle_album_transfer_credit(&mut self) {
        let Some(plan) = self.album_transfer.as_mut() else {
            return;
        };
        let Some(track) = plan.tracks.get(self.album_transfer_cursor) else {
            return;
        };
        if !track.requires_credit {
            self.lifecycle_msg =
                Some("This track already credits the destination artist.".to_owned());
            return;
        }
        let allowed = !track.credit_allowed;
        let id = track.track_id;
        match plan.set_credit_allowed(id, allowed) {
            Ok(()) => {
                self.lifecycle_msg = Some(if allowed {
                    "Destination artist will be added to this track's credits.".to_owned()
                } else {
                    "Destination artist will own the album but not appear in this track's credits."
                        .to_owned()
                });
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn execute_album_transfer(&mut self) {
        let Some(plan) = self.album_transfer.clone() else {
            self.mode = Mode::Normal;
            return;
        };
        if !plan.is_ready() {
            self.lifecycle_msg = Some(format!(
                "Album transfer is blocked: {}",
                plan.blocking_reason()
                    .unwrap_or("unresolved credit decisions")
            ));
            return;
        }
        match self.controller.execute_album_transfer(&plan) {
            Ok(report) => {
                self.album_transfer = None;
                self.mode = Mode::Normal;
                self.reset_selection();
                self.lifecycle_msg = Some(format!(
                    "Moved album {} from owner {} to owner {} while preserving {} track(s).",
                    report.album_id, report.from_owner, report.to_owner, report.tracks
                ));
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn artist_deletion_track(&self) -> Option<flux_app::ArtistDeletionTrack> {
        let plan = self.artist_deletion.as_ref()?;
        let index = self.artist_delete_cursor;
        plan.owned_tracks
            .get(index)
            .or_else(|| {
                plan.credited_tracks
                    .get(index.saturating_sub(plan.owned_tracks.len()))
            })
            .cloned()
    }

    fn open_artist_owner_picker(&mut self) {
        let Some(track) = self.artist_deletion_track() else {
            return;
        };
        if track.owner_artist_id
            != self
                .artist_deletion
                .as_ref()
                .map(|plan| plan.artist_id)
                .unwrap_or(track.owner_artist_id)
        {
            self.lifecycle_msg =
                Some("Credited-only tracks use remove-credit, not ownership transfer.".to_owned());
            return;
        }
        if track.eligible_owners.is_empty() {
            self.lifecycle_msg =
                Some("No surviving credited artist can become the owner.".to_owned());
            return;
        }
        self.artist_transfer_track = Some(track.track_id);
        self.artist_owner_cursor = 0;
        self.mode = Mode::ArtistOwnerPick;
    }

    fn apply_artist_owner(&mut self) {
        let Some(plan) = self.artist_deletion.as_ref() else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let Some(track) = self.artist_deletion_track() else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let Some(owner) = track.eligible_owners.get(self.artist_owner_cursor).copied() else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let _ = plan;
        self.artist_transfer_owner = Some(owner);
        self.artist_album_cursor = 0;
        self.mode = Mode::ArtistAlbumPick;
    }

    fn artist_transfer_albums(&self) -> Vec<(AlbumId, String)> {
        let Some(plan) = self.artist_deletion.as_ref() else {
            return Vec::new();
        };
        let Some(owner) = self.artist_transfer_owner else {
            return Vec::new();
        };
        plan.destination_albums
            .iter()
            .filter(|album| album.owner_artist_id == owner)
            .map(|album| (album.id, album.title.clone()))
            .collect()
    }

    fn apply_artist_album(&mut self) {
        let Some(track) = self.artist_transfer_track else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let Some(owner) = self.artist_transfer_owner else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let albums = self.artist_transfer_albums();
        eprintln!(
            "DEBUG apply_artist_album track={track:?} owner={owner:?} cursor={} albums={albums:?}",
            self.artist_album_cursor
        );
        if self.artist_album_cursor >= albums.len() {
            let owner_name = self
                .controller
                .artist(owner)
                .map(|artist| artist.name().to_owned())
                .unwrap_or_else(|| format!("Artist {owner}"));
            let track_title = self
                .controller
                .track(track)
                .map(display_title)
                .unwrap_or_else(|| format!("Track {track}"));
            self.form = Some(FormState::new(
                "New destination album",
                FormKind::ArtistTransferAlbum { track, owner },
                vec![
                    FormField {
                        label: "Title",
                        value: format!("Transferred from {owner_name} — {track_title}"),
                        hint: "Required unique album title",
                    },
                    FormField {
                        label: "Release type",
                        value: "album".to_owned(),
                        hint: "Album, EP, Single, Compilation or Other",
                    },
                ],
            ));
            self.mode = Mode::FormInput;
            return;
        }
        let Some((album_id, title)) = albums.get(self.artist_album_cursor).cloned() else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        let Some(plan) = self.artist_deletion.as_mut() else {
            self.mode = Mode::ArtistDeletion;
            return;
        };
        match plan.set_transfer(track, owner, ArtistDeletionDestination::Existing(album_id)) {
            Ok(()) => {
                self.artist_delete_armed = None;
                self.mode = Mode::ArtistDeletion;
                self.lifecycle_msg = Some(format!("Track will transfer into “{title}”."));
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn resolve_artist_credit(&mut self) {
        let Some(plan) = self.artist_deletion.as_mut() else {
            return;
        };
        let index = self.artist_delete_cursor;
        let Some(track) = plan
            .credited_tracks
            .get(index.saturating_sub(plan.owned_tracks.len()))
            .cloned()
        else {
            self.lifecycle_msg = Some("Select a credited-only track.".to_owned());
            return;
        };
        let remaining = track
            .credited_artist_ids
            .iter()
            .filter(|artist| **artist != plan.artist_id)
            .count();
        let replacement = (remaining == 0)
            .then(|| track.eligible_owners.first().copied())
            .flatten();
        match plan.set_remove_credit(track.track_id, replacement) {
            Ok(()) => {
                self.artist_delete_armed = None;
                self.lifecycle_msg = Some(format!(
                    "Credit for track {} will be removed.",
                    track.track_id
                ));
            }
            Err(error) => self.lifecycle_msg = Some(error),
        }
    }

    fn resolve_artist_track_delete(&mut self) {
        let Some(plan) = self.artist_deletion.as_mut() else {
            return;
        };
        let index = self.artist_delete_cursor;
        let Some(track) = plan.owned_tracks.get(index).cloned() else {
            self.lifecycle_msg = Some("Select an owned track to delete.".to_owned());
            return;
        };
        if self.artist_delete_armed == Some(track.track_id) {
            if let Err(error) = plan.set_delete(track.track_id, true) {
                self.lifecycle_msg = Some(error);
            } else {
                self.artist_delete_armed = None;
                self.lifecycle_msg = Some(format!(
                    "Track {} is marked for deletion; y publishes when all decisions are ready.",
                    track.track_id
                ));
            }
        } else {
            self.artist_delete_armed = Some(track.track_id);
            self.lifecycle_msg = Some(format!(
                "Press d again to confirm deleting track {} and its managed resources.",
                track.track_id
            ));
        }
    }

    fn reset_artist_track_decisions(&mut self) {
        let Some(plan) = self.artist_deletion.as_mut() else {
            return;
        };
        let index = self.artist_delete_cursor;
        let id = plan
            .owned_tracks
            .get(index)
            .map(|track| track.track_id)
            .or_else(|| {
                plan.credited_tracks
                    .get(index.saturating_sub(plan.owned_tracks.len()))
                    .map(|track| track.track_id)
            });
        if let Some(id) = id {
            plan.set_unresolved(id);
            self.artist_delete_armed = None;
            self.lifecycle_msg = Some(format!("Track {id} is unresolved again."));
        }
    }

    fn auto_resolve_artist(&mut self) {
        let Some(plan) = self.artist_deletion.as_mut() else {
            return;
        };
        let owned = plan.owned_tracks.clone();
        let credited = plan.credited_tracks.clone();
        for track in owned {
            if matches!(track.decision, ArtistDeletionDecision::Unresolved)
                && let Some(new_owner) = track.eligible_owners.first().copied()
            {
                let destination = plan
                    .destination_albums
                    .iter()
                    .find(|album| album.owner_artist_id == new_owner)
                    .map_or_else(
                        || ArtistDeletionDestination::New {
                            title: format!(
                                "Transferred from {} — {}",
                                plan.artist_name, track.title
                            ),
                            release_type: ReleaseType::Album,
                        },
                        |album| ArtistDeletionDestination::Existing(album.id),
                    );
                let _ = plan.set_transfer(track.track_id, new_owner, destination);
            }
        }
        for track in credited {
            if matches!(track.decision, ArtistDeletionDecision::Unresolved) {
                let _ = plan.set_remove_credit(track.track_id, None);
            }
        }
        self.artist_delete_armed = None;
        self.lifecycle_msg = Some(if plan.is_ready() {
            "All safe decisions resolved. Press y to publish the artist deletion.".to_owned()
        } else {
            format!(
                "Decisions remain unresolved: {}",
                plan.blocking_reason().unwrap_or("review required")
            )
        });
    }

    fn confirm_artist_deletion(&mut self) {
        let Some(plan) = self.artist_deletion.take() else {
            self.mode = Mode::Normal;
            return;
        };
        if !plan.is_ready() {
            self.lifecycle_msg = Some(format!(
                "Artist deletion is not ready: {}",
                plan.blocking_reason().unwrap_or("unresolved decisions")
            ));
            self.artist_deletion = Some(plan);
            self.mode = Mode::ArtistDeletion;
            return;
        }
        match self.controller.execute_artist_deletion(&plan) {
            Ok(_) => {
                self.detail = None;
                self.mode = Mode::Normal;
                self.reset_selection();
                self.lifecycle_msg = Some(
                    "Deleted the artist and removed its Album associations; shared Albums and audio remain."
                        .to_owned(),
                );
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.lifecycle_msg = Some(error);
            }
        }
    }

    fn begin_lyrics_export(&mut self) {
        if self.lyrics_track.is_none() {
            self.lifecycle_msg = Some("Open managed lyrics first.".to_owned());
            return;
        }
        self.input.clear();
        self.mode = Mode::LyricsExportInput;
    }

    fn export_lyrics(&mut self, path_text: &str) {
        let Some(track) = self.lyrics_track else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(lyrics) = self.controller.track_lyrics(track) else {
            self.mode = Mode::Normal;
            self.lifecycle_msg = Some("This track has no managed lyrics.".to_owned());
            return;
        };
        let path = PathBuf::from(path_text.trim());
        if path.as_os_str().is_empty() {
            self.lifecycle_msg = Some("Enter an export path.".to_owned());
            self.mode = Mode::LyricsExportInput;
            return;
        }
        if path.exists() {
            self.lifecycle_msg =
                Some("That file already exists; choose a new export path.".to_owned());
            self.mode = Mode::LyricsExportInput;
            return;
        }
        match std::fs::write(&path, lyrics) {
            Ok(()) => {
                self.mode = Mode::LyricsView;
                self.lifecycle_msg = Some(format!("Exported lyrics to {}.", path.display()));
            }
            Err(error) => {
                self.lifecycle_msg = Some(format!(
                    "Could not export lyrics to {}: {error}",
                    path.display()
                ));
                self.mode = Mode::LyricsExportInput;
            }
        }
    }

    fn handle_playlist_confirm(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if ctrl_c {
            return true;
        }
        if key.code != KeyCode::Char('y') {
            self.playlist_action = None;
            self.mode = Mode::Normal;
            self.lifecycle_msg = Some("Playlist change cancelled.".to_owned());
            return false;
        }
        let Some(action) = self.playlist_action.take() else {
            self.mode = Mode::Normal;
            return false;
        };
        let result = match action {
            PlaylistAction::Delete { id, name, tracks } => self
                .controller
                .delete_playlist(id, &name, &tracks)
                .map(|()| {
                    self.detail = None;
                    format!("Deleted playlist “{name}”; tracks were kept.")
                }),
            PlaylistAction::Clear { id, tracks } => self
                .controller
                .clear_playlist(id, &tracks)
                .map(|()| "Cleared the playlist; tracks were kept.".to_owned()),
            PlaylistAction::Remove {
                id,
                position,
                tracks,
            } => self
                .controller
                .remove_playlist_entries(id, &[position], &tracks)
                .map(|_| "Removed the playlist entry.".to_owned()),
        };
        self.mode = Mode::Normal;
        match result {
            Ok(message) => self.lifecycle_msg = Some(message),
            Err(error) => self.lifecycle_msg = Some(error),
        }
        false
    }

    fn crossfade_state_message(&self) -> String {
        if self.controller.crossfade_enabled() {
            format!(
                "Crossfade on · {:.1} s · shared with the GUI.",
                self.controller.crossfade_duration_ms() as f64 / 1_000.0
            )
        } else {
            format!(
                "Crossfade off · remembered {:.1} s · natural transitions are gapless.",
                self.controller.crossfade_duration_ms() as f64 / 1_000.0
            )
        }
    }

    fn toggle_crossfade(&mut self) {
        let enabled = !self.controller.crossfade_enabled();
        self.lifecycle_msg = Some(match self.controller.set_crossfade_enabled(enabled) {
            Ok(()) => self.crossfade_state_message(),
            Err(error) => error,
        });
    }

    fn adjust_crossfade_up(&mut self) {
        if !self.controller.crossfade_enabled() {
            self.lifecycle_msg = Some(match self.controller.set_crossfade_enabled(true) {
                Ok(()) => self.crossfade_state_message(),
                Err(error) => error,
            });
            return;
        }
        let next = self
            .controller
            .crossfade_duration_ms()
            .saturating_add(250)
            .min(flux_app::CROSSFADE_MAX_MS);
        self.lifecycle_msg = Some(match self.controller.set_crossfade_duration_ms(next) {
            Ok(_) => self.crossfade_state_message(),
            Err(error) => error,
        });
    }

    fn adjust_crossfade_down(&mut self) {
        if !self.controller.crossfade_enabled() {
            self.lifecycle_msg = Some("Crossfade is already off.".to_owned());
            return;
        }
        let current = self.controller.crossfade_duration_ms();
        let next = current.saturating_sub(250);
        self.lifecycle_msg = Some(if next == 0 {
            match self.controller.set_crossfade_enabled(false) {
                Ok(()) => self.crossfade_state_message(),
                Err(error) => error,
            }
        } else {
            match self.controller.set_crossfade_duration_ms(next) {
                Ok(_) => self.crossfade_state_message(),
                Err(error) => error,
            }
        });
    }

    fn handle_input_key(&mut self, key: KeyEvent, ctrl_c: bool) -> bool {
        if self.mode == Mode::LyricsExportInput {
            if ctrl_c {
                return true;
            }
            match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::LyricsView;
                    self.lifecycle_msg = Some("Lyrics export cancelled.".to_owned());
                }
                KeyCode::Enter => {
                    let text = std::mem::take(&mut self.input);
                    self.export_lyrics(&text);
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(character) => self.input.push(character),
                _ => {}
            }
            return false;
        }
        if self.mode == Mode::TrackReplacementInput {
            if ctrl_c {
                return true;
            }
            match key.code {
                KeyCode::Esc => {
                    self.asset_track = None;
                    self.mode = Mode::Normal;
                    self.lifecycle_msg = Some("Audio replacement cancelled.".to_owned());
                }
                KeyCode::Enter => {
                    let text = std::mem::take(&mut self.input);
                    self.plan_track_replacement(&text);
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(character) => self.input.push(character),
                _ => {}
            }
            return false;
        }
        if self.mode == Mode::ImportAssetInput {
            if ctrl_c {
                return true;
            }
            match key.code {
                KeyCode::Esc => {
                    self.import_asset = None;
                    self.mode = Mode::ImportReview;
                    self.lifecycle_msg = Some("Asset attachment cancelled.".to_owned());
                }
                KeyCode::Enter => {
                    let text = std::mem::take(&mut self.input);
                    self.apply_import_asset(&text);
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(character) => self.input.push(character),
                _ => {}
            }
            return false;
        }
        // The two migration decision modes are single-key, not text.
        match self.mode {
            Mode::MigrateConfirm => {
                if key.code == KeyCode::Char('y') {
                    self.execute_pending_migration();
                } else {
                    self.pending_plan = None;
                    self.lifecycle_msg =
                        Some("Migration cancelled — nothing was written.".to_owned());
                    self.mode = Mode::Normal;
                }
                return false;
            }
            Mode::MigratedDone => {
                let mut activated = false;
                let mut activation_error = None;
                if key.code == KeyCode::Char('a')
                    && let Some(destination) = self.migrated_dest.clone()
                {
                    match self.controller.switch_to_canonical(&destination) {
                        Ok(()) => {
                            self.after_switch();
                            activated = true;
                        }
                        Err(error) => {
                            activation_error = Some(format!(
                                "Migrated library activation failed: {error}. The current \
                                 library remains active."
                            ));
                        }
                    }
                }
                if !activated {
                    self.lifecycle_msg = activation_error.or_else(|| {
                        Some(
                            "Migrated library was not activated; the current library remains \
                             active."
                                .to_owned(),
                        )
                    });
                }
                self.migrated_dest = None;
                self.mode = Mode::Normal;
                return false;
            }
            Mode::ConfirmPreserveIndex => {
                if key.code == KeyCode::Char('y') {
                    self.confirm_preserve_index();
                } else {
                    self.lifecycle_msg = Some(
                        "Index replacement cancelled; the protected file is unchanged.".to_owned(),
                    );
                    self.mode = Mode::Normal;
                }
                return false;
            }
            Mode::ConfirmAcknowledgeRecovery => {
                if key.code == KeyCode::Char('y') {
                    self.confirm_acknowledge_recovery();
                } else {
                    self.lifecycle_msg = Some(
                        "Recovery acknowledgement cancelled; the library remains read-only."
                            .to_owned(),
                    );
                    self.mode = Mode::Normal;
                }
                return false;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Esc => {
                // Cancelling an input never changes the active
                // library and abandons any in-flight plan.
                let cancelled = self.mode;
                self.pending_plan = None;
                self.mode = Mode::Normal;
                if let Some(message) = cancel_message(cancelled) {
                    self.lifecycle_msg = Some(message.to_owned());
                }
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                let mut next = Mode::Normal;
                match self.mode {
                    Mode::AlbumParticipants
                    | Mode::AlbumParticipantPick
                    | Mode::ConfirmAlbumParticipantRemove
                    | Mode::ImportParticipants
                    | Mode::ImportParticipantPick => {}
                    Mode::ScanInput => {
                        let path = text.trim();
                        if path.is_empty() {
                            self.lifecycle_msg =
                                Some("No folder path entered; nothing was scanned.".to_owned());
                        } else {
                            match self.start_import_review(vec![PathBuf::from(path)], true) {
                                Ok(()) => next = self.mode,
                                Err(err) => {
                                    self.lifecycle_msg =
                                        Some(format!("Folder import failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::SearchInput => {
                        self.search = text;
                        self.cursor = 0;
                    }
                    Mode::FormInput
                    | Mode::CommandInput
                    | Mode::ConfirmPlaylist
                    | Mode::ConfirmQueueClear
                    | Mode::TrackMovePick
                    | Mode::ConfirmTrackMove
                    | Mode::ConfirmTrackDelete
                    | Mode::ConfirmAlbumDelete
                    | Mode::ConfirmArtistDelete => {}
                    Mode::ImportInput => {
                        let paths: Vec<PathBuf> = text
                            .split(';')
                            .map(str::trim)
                            .filter(|path| !path.is_empty())
                            .map(PathBuf::from)
                            .collect();
                        if paths.is_empty() {
                            self.lifecycle_msg =
                                Some("Enter at least one audio file path.".to_owned());
                        } else {
                            match self.start_import_review(paths, false) {
                                Ok(()) => next = self.mode,
                                Err(err) => {
                                    self.lifecycle_msg = Some(format!("File import failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::TrackEditInput => match parse_track_edit_form(&text) {
                        Ok(edit) => {
                            let id = self.rows().get(self.cursor).copied();
                            if let Some(id) = id {
                                match self.controller.edit_track_metadata(id, edit) {
                                    Ok(()) => {
                                        self.lifecycle_msg =
                                            Some(format!("Saved metadata for track {id}."));
                                    }
                                    Err(err) => self.lifecycle_msg = Some(err),
                                }
                            } else {
                                self.lifecycle_msg = Some("The selected track is gone.".to_owned());
                            }
                        }
                        Err(err) => {
                            self.lifecycle_msg = Some(format!(
                                "{err} The edit form remains open so you can correct it."
                            ));
                            self.input = text;
                            next = Mode::TrackEditInput;
                        }
                    },
                    Mode::ArtistCreateInput => {
                        if let Err(err) = self.controller.create_artist(text.trim()) {
                            self.lifecycle_msg = Some(err);
                        } else {
                            self.lifecycle_msg = Some(format!("Created artist {}.", text.trim()));
                        }
                    }
                    Mode::ArtistRenameInput => match parse_artist_edit_form(&text) {
                        Ok(edit) => {
                            let current_description = self
                                .controller
                                .artist_description(edit.id)
                                .map(str::to_owned);
                            let description = edit
                                .description
                                .as_ref()
                                .map(|value| value.as_deref())
                                .unwrap_or(current_description.as_deref());
                            let aliases = edit.aliases.unwrap_or_else(|| {
                                self.controller.artist_aliases(edit.id).to_vec()
                            });
                            let metadata = self.controller.update_artist_metadata(
                                edit.id,
                                &edit.name,
                                description,
                                &aliases,
                            );
                            let image = match edit.image {
                                ImageEdit::Preserve => Ok(()),
                                ImageEdit::Remove => self.controller.remove_artist_image(edit.id),
                                ImageEdit::Set(path) => {
                                    self.controller.set_artist_image(edit.id, &path)
                                }
                            };
                            match (metadata, image) {
                                (Ok(()), Ok(())) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Updated artist {} ({} alias(es)).",
                                        edit.id.get(),
                                        aliases.len()
                                    ));
                                }
                                (Err(error), _) => {
                                    self.lifecycle_msg = Some(error);
                                }
                                (_, Err(error)) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Artist metadata saved; image failed: {error}"
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            self.lifecycle_msg = Some(format!(
                                "{err} The form remains open so you can correct it."
                            ));
                            self.input = text;
                            next = Mode::ArtistRenameInput;
                        }
                    },
                    Mode::AlbumEditInput => match parse_album_edit_form(&text) {
                        Ok(edit) => {
                            let metadata = self.controller.update_album_metadata(
                                edit.id,
                                &edit.title,
                                edit.release_type,
                            );
                            let cover = match edit.cover {
                                ImageEdit::Preserve => Ok(()),
                                ImageEdit::Remove => self.controller.remove_album_cover(edit.id),
                                ImageEdit::Set(path) => {
                                    self.controller.set_album_cover(edit.id, &path)
                                }
                            };
                            match (metadata, cover) {
                                (Ok(()), Ok(())) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Updated album {} ({}).",
                                        edit.id.get(),
                                        edit.release_type.as_str()
                                    ));
                                }
                                (Err(error), _) => self.lifecycle_msg = Some(error),
                                (_, Err(error)) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Album metadata saved; cover failed: {error}"
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            self.lifecycle_msg = Some(format!(
                                "{err} The form remains open so you can correct it."
                            ));
                            self.input = text;
                            next = Mode::AlbumEditInput;
                        }
                    },
                    Mode::TrackArtworkInput => {
                        let Some(id) = self.asset_track else {
                            self.lifecycle_msg = Some("Select a track first.".to_owned());
                            return false;
                        };
                        let path = text.trim();
                        let result = if path == "-" {
                            self.controller.remove_track_artwork(id)
                        } else if path.is_empty() {
                            Err("Enter an image path, or - to remove the override.".to_owned())
                        } else {
                            self.controller.set_track_artwork(id, Path::new(path))
                        };
                        match result {
                            Ok(()) => {
                                self.asset_track = None;
                                self.lifecycle_msg =
                                    Some(format!("Updated artwork override for track {id}."));
                            }
                            Err(error) => {
                                self.lifecycle_msg = Some(error);
                                self.input = text;
                                next = Mode::TrackArtworkInput;
                            }
                        }
                    }
                    Mode::TrackLyricsInput => {
                        let Some(id) = self.asset_track else {
                            self.lifecycle_msg = Some("Select a track first.".to_owned());
                            return false;
                        };
                        let path = text.trim();
                        let result = if path == "-" {
                            self.controller.set_lyrics(id, None).map(|()| None)
                        } else if path.is_empty() {
                            Err("Enter a UTF-8 LRC/TXT path, or - to remove lyrics.".to_owned())
                        } else {
                            match std::fs::read_to_string(path) {
                                Ok(text) if text.len() <= 1024 * 1024 => self
                                    .controller
                                    .set_lyrics(id, Some(text.as_str()))
                                    .map(|()| Some(text)),
                                Ok(_) => {
                                    Err("Lyrics file is larger than 1 MiB; nothing was changed."
                                        .to_owned())
                                }
                                Err(error) => {
                                    Err(format!("Could not read lyrics file {path}: {error}"))
                                }
                            }
                        };
                        match result {
                            Ok(Some(text)) => {
                                self.asset_track = None;
                                self.lifecycle_msg = Some(format!(
                                    "Replaced managed lyrics for track {id} ({} line(s)).",
                                    text.lines().count()
                                ));
                            }
                            Ok(None) => {
                                self.asset_track = None;
                                self.lifecycle_msg =
                                    Some(format!("Removed managed lyrics for track {id}."));
                            }
                            Err(error) => {
                                self.lifecycle_msg = Some(error);
                                self.input = text;
                                next = Mode::TrackLyricsInput;
                            }
                        }
                    }
                    Mode::OpenInput => {
                        if text.trim().is_empty() {
                            self.lifecycle_msg = Some(
                                "No library folder entered; active library unchanged.".to_owned(),
                            );
                        } else {
                            let path = PathBuf::from(text.trim());
                            match self.controller.switch_to_canonical(&path) {
                                Ok(()) => self.after_switch(),
                                Err(error) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Open failed: {error}. Active library unchanged."
                                    ));
                                }
                            }
                        }
                    }
                    Mode::CreateInput => {
                        if text.trim().is_empty() {
                            self.lifecycle_msg = Some(
                                "No destination folder entered; active library unchanged."
                                    .to_owned(),
                            );
                        } else {
                            let path = PathBuf::from(text.trim());
                            match self.controller.switch_to_new_canonical_library(&path) {
                                Ok(_) => self.after_switch(),
                                Err(error) => {
                                    self.lifecycle_msg = Some(format!(
                                        "Create failed: {error}. Active library unchanged."
                                    ));
                                }
                            }
                        }
                    }
                    Mode::MigrateSourceInput => {
                        let source = text.trim();
                        if source.is_empty() {
                            self.lifecycle_msg =
                                Some("No legacy index path entered; nothing planned.".to_owned());
                        } else {
                            self.migrate_source = PathBuf::from(source);
                            next = Mode::MigrateDestInput;
                        }
                    }
                    Mode::MigrateDestInput => {
                        let destination = text.trim();
                        if destination.is_empty() {
                            self.lifecycle_msg =
                                Some("No destination folder entered; nothing planned.".to_owned());
                        } else {
                            let destination = PathBuf::from(destination);
                            match flux_app::plan_canonical_migration(
                                &self.migrate_source,
                                &destination,
                            ) {
                                Ok(plan) => {
                                    self.lifecycle_msg = Some(migration_plan_summary(&plan));
                                    self.pending_plan = Some(plan);
                                    self.migrated_dest = None;
                                    next = Mode::MigrateConfirm;
                                }
                                Err(err) => {
                                    self.lifecycle_msg =
                                        Some(format!("Migration planning failed: {err}"));
                                }
                            }
                        }
                    }
                    Mode::Normal
                    | Mode::MigrateConfirm
                    | Mode::MigratedDone
                    | Mode::LyricsView
                    | Mode::InfoView
                    | Mode::QueueView
                    | Mode::ImportScanning
                    | Mode::ImportReview
                    | Mode::ImportPreparing
                    | Mode::ConfirmPreserveIndex
                    | Mode::ConfirmAcknowledgeRecovery
                    | Mode::LyricsExportInput
                    | Mode::ImportAssetInput
                    | Mode::TrackReplacementInput
                    | Mode::ConfirmTrackReplacement
                    | Mode::ArtistDeletion
                    | Mode::ImportAlbumPick
                    | Mode::AlbumTransferPick
                    | Mode::AlbumTransferReview
                    | Mode::ArtistOwnerPick
                    | Mode::ArtistAlbumPick => {}
                }
                self.mode = next;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(_) if ctrl_c => self.mode = Mode::Normal,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(c);
            }
            _ => {}
        }
        false
    }

    fn confirm_preserve_index(&mut self) {
        self.mode = Mode::Normal;
        match self.controller.replace_unreadable_legacy_index() {
            Ok(backup) => {
                self.lifecycle_msg = Some(format!(
                    "Unreadable index preserved as {}; scanning and saving are enabled.",
                    backup.display()
                ));
            }
            Err(err) => self.lifecycle_msg = Some(format!("Index preservation failed: {err}")),
        }
    }

    fn confirm_acknowledge_recovery(&mut self) {
        self.mode = Mode::Normal;
        match self.controller.acknowledge_portable_recovery() {
            Ok(()) => {
                self.lifecycle_msg = Some(
                    "Portable recovery acknowledged; scanning and saving are enabled.".to_owned(),
                );
            }
            Err(err) => {
                self.lifecycle_msg = Some(format!("Recovery acknowledgement failed: {err}"));
            }
        }
    }

    /// Runs the migration the user confirmed with `y`. Execution
    /// never switches the active library by itself; success moves to
    /// [`Mode::MigratedDone`], where `a` activates the result.
    fn execute_pending_migration(&mut self) {
        let Some(plan) = self.pending_plan.take() else {
            self.mode = Mode::Normal;
            return;
        };
        if !plan.is_ready() {
            self.lifecycle_msg = Some(format!(
                "Migration blocked by {} issue(s); nothing was executed. Fix the source or \
                 destination and plan again.",
                plan.issues().len()
            ));
            self.mode = Mode::Normal;
            return;
        }
        let destination = plan.destination().to_path_buf();
        match flux_app::execute_canonical_migration(&plan) {
            Ok(report) => {
                self.lifecycle_msg = Some(format!(
                    "Canonical migration complete: {} track(s), {} artist(s), {} album(s), \
                     validated. The source is untouched. Press a to activate the \
                     migrated library, Esc to keep the current one.",
                    report.tracks(),
                    report.artists(),
                    report.albums(),
                ));
                self.migrated_dest = Some(destination);
                self.mode = Mode::MigratedDone;
            }
            Err(err) => {
                self.lifecycle_msg = Some(format!("Migration failed: {err}"));
                self.mode = Mode::Normal;
            }
        }
    }

    /// Applies the safe relink suggested by the last reconciliation when
    /// exactly one candidate exists for a missing track. Multiple
    /// candidates remain a manual choice (the GUI exposes each one).
    fn relink_first_unique(&mut self) {
        let Some(report) = self.last_reconcile.take() else {
            self.lifecycle_msg =
                Some("Reconcile with X before relinking a missing file.".to_owned());
            return;
        };
        let Some(candidate) = report
            .relink_candidates
            .iter()
            .find(|candidates| candidates.is_unique())
            .and_then(|candidates| candidates.candidates.first())
            .cloned()
        else {
            self.lifecycle_msg = Some(
                "No unique relink candidate is available; X reports ambiguous ones.".to_owned(),
            );
            return;
        };
        let track = report
            .relink_candidates
            .iter()
            .find(|candidates| candidates.is_unique())
            .map(|candidates| candidates.id);
        if let Some(id) = track {
            match self.controller.relink_portable_track(id, &candidate) {
                Ok(()) => {
                    self.lifecycle_msg = Some(format!(
                        "Relinked track {id} to {}.",
                        candidate.display_lossy()
                    ));
                }
                Err(err) => self.lifecycle_msg = Some(err),
            }
        }
    }

    /// Housekeeping after a successful library switch: cursor,
    /// search, and pending migration state referred to the old
    /// library. (The controller already stopped playback and rebuilt
    /// its queue from the new library.)
    fn after_switch(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        self.mode = Mode::Normal;
        self.input.clear();
        self.search.clear();
        self.pending_plan = None;
        self.migrated_dest = None;
        self.last_reconcile = None;
        self.lyrics_lines.clear();
        self.lyrics_scroll = 0;
        self.lyrics_title.clear();
        self.queue_cursor = 0;
        self.queue_offset = 0;
        self.info_title.clear();
        self.info_lines.clear();
        self.info_scroll = 0;
        self.lifecycle_msg = Some(format!("Active library: {}", self.library_full_identity()));
    }

    /// After next/previous, put the cursor on the player's selection.
    fn follow_current(&mut self) {
        if let Some(current) = self.controller.current_id()
            && let Some(pos) = self.rows().iter().position(|id| *id == current)
        {
            self.cursor = pos;
        }
    }

    fn follow_current_queue(&mut self) {
        if let Some(entry) = self.controller.current_queue_entry_id()
            && let Some(index) = self.controller.queue_entry_index(entry)
        {
            self.queue_cursor = index;
        }
    }
}

impl ImageEdit {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") => Ok(Self::Preserve),
            Some("-") => Ok(Self::Remove),
            Some(path) => Ok(Self::Set(PathBuf::from(path))),
        }
    }
}

fn artist_display_name(controller: &Controller, id: ArtistId) -> String {
    let Some(artist) = controller.artist(id) else {
        return format!("Artist {id}");
    };
    let duplicate = controller
        .artists()
        .iter()
        .filter(|candidate| candidate.name().eq_ignore_ascii_case(artist.name()))
        .count()
        > 1;
    if duplicate {
        format!("{} [ID {}]", artist.name(), id)
    } else {
        artist.name().to_owned()
    }
}

fn credit_display(controller: &Controller, id: TrackId) -> String {
    let credits = controller.track_credits(id);
    if credits.is_empty() {
        return controller
            .track(id)
            .and_then(Track::artist)
            .unwrap_or("Unknown artist")
            .to_owned();
    }
    credits
        .iter()
        .map(|artist| artist_display_name(controller, *artist))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn tui_prefs_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FLUX_TUI_CONFIG") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("flux").join("tui.conf"))
}

fn cancel_message(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::ScanInput => Some("Scan cancelled; no folder was scanned."),
        Mode::SearchInput => Some("Search input cancelled; the previous filter is unchanged."),
        Mode::ImportInput => Some("Import cancelled; no files were imported."),
        Mode::ImportScanning | Mode::ImportPreparing => {
            Some("Import cancelled; no catalogue changes were made.")
        }
        Mode::ImportReview => Some("Import review closed; nothing was copied or published."),
        Mode::ImportParticipants | Mode::ImportParticipantPick => {
            Some("Import participant selection cancelled; the review is unchanged.")
        }
        Mode::TrackEditInput => Some("Track edit cancelled; no changes were made."),
        Mode::FormInput => Some("Form cancelled; no changes were made."),
        Mode::CommandInput => Some("Command cancelled; nothing changed."),
        Mode::ConfirmPlaylist => Some("Playlist change cancelled."),
        Mode::ConfirmQueueClear => Some("Queue clear cancelled."),
        Mode::AlbumParticipants => Some("Album participants closed."),
        Mode::AlbumParticipantPick => Some("Album participant selection cancelled."),
        Mode::ConfirmAlbumParticipantRemove => Some("Participant removal cancelled."),
        Mode::TrackMovePick => Some("Track move cancelled."),
        Mode::TrackReplacementInput => Some("Audio replacement cancelled."),
        Mode::ConfirmTrackReplacement => Some("Audio replacement cancelled."),
        Mode::ConfirmTrackMove => Some("Track move cancelled."),
        Mode::ConfirmTrackDelete => Some("Track deletion cancelled."),
        Mode::ConfirmAlbumDelete => Some("Album deletion cancelled."),
        Mode::ArtistDeletion => Some("Artist deletion cancelled."),
        Mode::ConfirmArtistDelete => Some("Artist deletion cancelled."),
        Mode::ImportAlbumPick => Some("Fixed-target import cancelled; the review is unchanged."),
        Mode::AlbumTransferPick => Some("Album transfer cancelled."),
        Mode::AlbumTransferReview => Some("Album transfer cancelled."),
        Mode::ArtistOwnerPick => Some("Destination owner selection cancelled."),
        Mode::ArtistAlbumPick => Some("Destination album selection cancelled."),
        Mode::LyricsExportInput => Some("Lyrics export cancelled."),
        Mode::ImportAssetInput => Some("Asset attachment cancelled."),
        Mode::ArtistCreateInput => Some("Artist creation cancelled."),
        Mode::ArtistRenameInput => Some("Artist edit cancelled; no changes were made."),
        Mode::AlbumEditInput => Some("Album edit cancelled; no changes were made."),
        Mode::TrackArtworkInput => Some("Track artwork edit cancelled; no changes were made."),
        Mode::TrackLyricsInput => Some("Lyrics edit cancelled; no changes were made."),
        Mode::OpenInput | Mode::CreateInput => {
            Some("Library switch cancelled; the active library is unchanged.")
        }
        Mode::MigrateSourceInput | Mode::MigrateDestInput => {
            Some("Migration cancelled before planning; nothing was written.")
        }
        Mode::Normal
        | Mode::MigrateConfirm
        | Mode::MigratedDone
        | Mode::LyricsView
        | Mode::InfoView
        | Mode::QueueView
        | Mode::ConfirmPreserveIndex
        | Mode::ConfirmAcknowledgeRecovery => None,
    }
}

fn run(initial_dir: Option<PathBuf>) -> std::io::Result<()> {
    let controller =
        Controller::open_default().map_err(|err| std::io::Error::other(err.to_string()))?;
    let created_default = controller.default_library_created();
    let default_root = controller
        .canonical_root()
        .map(|root| root.display().to_string());

    let guard = TerminalGuard::enter()?;
    // Restore the terminal even if a panic unwinds through the loop.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut tui = Tui {
        controller,
        import_ui: None,
        cursor: 0,
        offset: 0,
        mode: Mode::Normal,
        input: String::new(),
        search: String::new(),
        rows_cache: RefCell::new(None),
        favorites_only: false,
        migrate_source: PathBuf::new(),
        lyrics_lines: Vec::new(),
        lyrics_scroll: 0,
        lyrics_title: String::new(),
        lyrics_follow_ready: true,
        lyrics_track: None,
        lyrics_timestamps: Vec::new(),
        queue_cursor: 0,
        queue_follow: true,
        queue_offset: 0,
        content_offset: 0,
        album_participant_cursor: 0,
        album_participant_pick_cursor: 0,
        import_participant_cursor: 0,
        import_participant_pick_cursor: 0,
        pending_participant_remove: None,
        info_title: String::new(),
        info_lines: Vec::new(),
        info_scroll: 0,
        pending_plan: None,
        migrated_dest: None,
        lifecycle_msg: None,
        last_reconcile: None,
        view: View::Home,
        detail: None,
        active_collection: None,
        layout: LayoutMode::Standard,
        viz_visible: true,
        viz_mode: VizMode::Waveform,
        viz_gain: 1.0,
        scope: WaveformScope::default(),
        follow_lyrics: true,
        prefs_path: tui_prefs_path(),
        form: None,
        playlist_action: None,
        move_track: None,
        move_cursor: 0,
        pending_move: None,
        pending_track_delete: None,
        pending_album_delete: None,
        import_asset: None,
        asset_track: None,
        pending_replacement: None,
        artist_deletion: None,
        artist_delete_cursor: 0,
        artist_delete_armed: None,
        artist_transfer_track: None,
        artist_transfer_owner: None,
        artist_owner_cursor: 0,
        artist_album_cursor: 0,
        album_transfer: None,
        album_transfer_cursor: 0,
        album_transfer_artist_cursor: 0,
        import_album_cursor: 0,
        audio_devices: Vec::new(),
        audio_error: None,
        audio_loading: false,
        audio_list_rx: None,
        audio_open_rx: None,
    };
    tui.load_preferences();
    if created_default {
        tui.lifecycle_msg = Some(format!(
            "Created a new empty library at {}. Use i/o to import music; ? lists commands.",
            default_root.unwrap_or_else(|| "the per-user data directory".to_owned())
        ));
    }
    if let Some(dir) = initial_dir
        && let Err(error) = tui.start_import_review(vec![dir], true)
    {
        tui.lifecycle_msg = Some(format!("Startup folder import failed: {error}"));
    }
    let mut stdout = std::io::stdout();
    let result = tui.event_loop(&mut stdout);

    drop(guard);
    tui.stop_import_worker();
    tui.controller.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        AlbumTransferPlan, ArtistDeletionPlan, Detail, ImageEdit, ImportWorkerMessage, LayoutMode,
        Mode, Row, StorageMode, Track, Tui, View, VizMode, WaveformScope, album_edit_form,
        artist_display_name, clamp_terminal_size, detail_view_fits, display_width, fit_input_line,
        help_lines, normal_help, pad_with_right, parse_album_edit_form, parse_artist_edit_form,
        parse_release_type, resource_label, terminal_too_small, truncate_to_width,
        visualization_canvas,
    };
    use crate::import::candidate_status_detail;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use flux_app::{CandidateStatus, ImportDecision};
    use flux_core::TrackId;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Encodes a filesystem path as JSON string contents. Windows paths contain
    /// backslashes, which are escape characters in JSON.
    fn json_path(path: &std::path::Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-tui-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            #[cfg(target_os = "macos")]
            let path = std::fs::canonicalize(&path).expect("canonical temp dir");
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

    /// A Tui over a hermetic legacy controller — the key-handling
    /// wiring is what is under test; lifecycle semantics themselves
    /// live in `flux-app`.
    fn tui(dir: &TempDir) -> Tui {
        let (engine, audio) = flux_app::default_engine();
        Tui {
            controller: flux_app::Controller::new(
                engine,
                audio,
                Some(dir.path().join("library-index.json")),
            )
            .expect("controller opens the test library"),
            import_ui: None,
            cursor: 0,
            offset: 0,
            mode: Mode::Normal,
            input: String::new(),
            search: String::new(),
            rows_cache: RefCell::new(None),
            favorites_only: false,
            migrate_source: std::path::PathBuf::new(),
            lyrics_lines: Vec::new(),
            lyrics_scroll: 0,
            lyrics_title: String::new(),
            lyrics_follow_ready: true,
            lyrics_track: None,
            lyrics_timestamps: Vec::new(),
            queue_cursor: 0,
            queue_follow: true,
            queue_offset: 0,
            content_offset: 0,
            album_participant_cursor: 0,
            album_participant_pick_cursor: 0,
            import_participant_cursor: 0,
            import_participant_pick_cursor: 0,
            pending_participant_remove: None,
            info_title: String::new(),
            info_lines: Vec::new(),
            info_scroll: 0,
            pending_plan: None,
            migrated_dest: None,
            lifecycle_msg: None,
            last_reconcile: None,
            view: View::Tracks,
            detail: None,
            active_collection: None,
            layout: LayoutMode::Standard,
            viz_visible: false,
            viz_mode: VizMode::Waveform,
            viz_gain: 1.0,
            scope: WaveformScope::default(),
            follow_lyrics: true,
            prefs_path: None,
            form: None,
            playlist_action: None,
            move_track: None,
            move_cursor: 0,
            pending_move: None,
            pending_track_delete: None,
            pending_album_delete: None,
            import_asset: None,
            asset_track: None,
            pending_replacement: None,
            artist_deletion: None,
            artist_delete_cursor: 0,
            artist_delete_armed: None,
            artist_transfer_track: None,
            artist_transfer_owner: None,
            artist_owner_cursor: 0,
            artist_album_cursor: 0,
            album_transfer: None,
            album_transfer_cursor: 0,
            album_transfer_artist_cursor: 0,
            import_album_cursor: 0,
            audio_devices: Vec::new(),
            audio_error: None,
            audio_loading: false,
            audio_list_rx: None,
            audio_open_rx: None,
        }
    }

    fn key(tui: &mut Tui, code: KeyCode) {
        assert!(
            !tui.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
            "test keys must not quit"
        );
    }

    fn key_mod(tui: &mut Tui, code: KeyCode, mods: KeyModifiers) {
        assert!(
            !tui.handle_key(KeyEvent::new(code, mods)),
            "test keys must not quit"
        );
    }

    fn type_path(tui: &mut Tui, path: &std::path::Path) {
        for c in path.display().to_string().chars() {
            key(tui, KeyCode::Char(c));
        }
        key(tui, KeyCode::Enter);
    }

    fn type_text(tui: &mut Tui, text: &str) {
        for c in text.chars() {
            key(tui, KeyCode::Char(c));
        }
        key(tui, KeyCode::Enter);
    }

    fn wait_for_mode(tui: &mut Tui, mode: Mode) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            tui.poll_import_worker();
            if tui.mode == mode {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "import worker did not reach {mode:?}: {:?}",
            tui.lifecycle_msg
        );
    }

    fn wait_for_tracks(tui: &mut Tui, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            tui.poll_import_worker();
            if tui.controller.tracks().len() == count {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "import did not publish {count} track(s): {:?}",
            tui.lifecycle_msg
        );
    }

    fn wait_for_import_done(tui: &mut Tui) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            tui.poll_import_worker();
            if !matches!(
                tui.mode,
                Mode::ImportScanning | Mode::ImportReview | Mode::ImportPreparing
            ) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "import did not finish: mode {:?}, message {:?}",
            tui.mode, tui.lifecycle_msg
        );
    }

    fn finish_import(tui: &mut Tui) {
        wait_for_mode(tui, Mode::ImportReview);
        key(tui, KeyCode::Char('y'));
        wait_for_import_done(tui);
    }

    #[test]
    fn crossfade_keys_show_toggle_adjust_and_persist_shared_settings() {
        let dir = TempDir::new("crossfade-keys");
        let settings = dir.path().join("config").join("settings.json");
        {
            let mut app = tui(&dir);
            app.controller.load_settings_from_path(&settings);
            assert!(!app.controller.crossfade_enabled());
            assert!(app.now_playing_line(120).contains("Xfade off"));
            let help = help_lines(&app.controller, "test library").join("\n");
            assert!(help.contains("K crossfade"), "{help}");
            assert!(help.contains("[ ] crossfade duration"), "{help}");
            assert!(normal_help().contains("? help"));

            key(&mut app, KeyCode::Char('K'));
            assert!(app.controller.crossfade_enabled());
            assert_eq!(app.controller.crossfade_duration_ms(), 3_000);
            assert_eq!(app.controller.crossfade_ms(), 3_000);
            assert!(
                app.lifecycle_msg
                    .as_deref()
                    .is_some_and(|message| message.contains("Crossfade on"))
            );
            assert!(app.now_playing_line(120).contains("Xfade 3.0s"));

            key(&mut app, KeyCode::Char(']'));
            assert_eq!(app.controller.crossfade_duration_ms(), 3_250);
            assert_eq!(app.controller.crossfade_ms(), 3_250);
            key(&mut app, KeyCode::Char('['));
            assert_eq!(app.controller.crossfade_duration_ms(), 3_000);

            let saved = std::fs::read_to_string(&settings).expect("shared settings saved");
            assert!(saved.contains("\"enabled\": true"));
            assert!(saved.contains("\"duration_ms\": 3000"));
            app.controller.shutdown();
        }

        let restarted_dir = TempDir::new("crossfade-keys-restarted");
        let mut restarted = tui(&restarted_dir);
        restarted.controller.load_settings_from_path(&settings);
        assert!(restarted.controller.crossfade_enabled());
        assert_eq!(restarted.controller.crossfade_duration_ms(), 3_000);
        assert_eq!(restarted.controller.crossfade_ms(), 3_000);
        key(&mut restarted, KeyCode::Char('K'));
        assert!(!restarted.controller.crossfade_enabled());
        assert_eq!(restarted.controller.crossfade_ms(), 0);
        assert!(restarted.now_playing_line(120).contains("Xfade off"));
        restarted.controller.shutdown();
    }

    #[test]
    fn create_key_flow_creates_and_switches_and_esc_cancels() {
        use flux_app::StorageMode;

        let dir = TempDir::new("create-flow");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);

        // Esc during path entry cancels: nothing happens.
        key(&mut tui, KeyCode::Char('C'));
        assert_eq!(tui.mode, Mode::CreateInput);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert!(!root.join(".flux").exists());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );

        // A rejected destination preserves the active library.
        let occupied = dir.path().join("occupied");
        std::fs::write(&occupied, b"not a directory").expect("occupied file");
        tui.search = "preserve me".to_owned();
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &occupied);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(tui.search, "preserve me");

        // Full flow: C, type folder, Enter → created + switched. State tied
        // to the old library is cleared, while the global filter survives.
        let old_session = tui.controller.library_session();
        tui.search = "old library".to_owned();
        tui.input = "stale input".to_owned();
        tui.lyrics_lines = vec!["old lyric".to_owned()];
        tui.lyrics_title = "old title".to_owned();
        tui.favorites_only = true;
        key(&mut tui, KeyCode::Char('C'));
        tui.queue_cursor = 5;
        tui.queue_offset = 3;
        tui.info_lines = vec!["stale info".to_owned()];
        type_path(&mut tui, &root);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Canonical,
            "{:?}",
            tui.lifecycle_msg
        );
        assert_ne!(tui.controller.library_session(), old_session);
        assert!(tui.search.is_empty());
        assert!(tui.input.is_empty());
        assert!(tui.lyrics_lines.is_empty());
        assert!(tui.lyrics_title.is_empty());
        assert_eq!(tui.queue_cursor, 0);
        assert_eq!(tui.queue_offset, 0);
        assert!(tui.info_lines.is_empty());
        assert!(tui.info_title.is_empty());
        assert!(tui.favorites_only);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("Active library")),
            "{:?}",
            tui.lifecycle_msg
        );
        assert!(
            root.join("catalog.flux").exists(),
            "canonical catalogue created"
        );
        assert!(!root.join(".flux").exists(), "no portable runtime store");
        tui.controller.shutdown();
    }

    #[test]
    fn import_edit_and_artist_keys_use_the_shared_controller() {
        let dir = TempDir::new("tui-manage");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Tui Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);

        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Canonical,
            "{:?}",
            tui.lifecycle_msg
        );

        key(&mut tui, KeyCode::Char('i'));
        assert_eq!(tui.mode, Mode::ImportInput);
        type_path(&mut tui, &source);
        wait_for_mode(&mut tui, Mode::ImportReview);
        assert_eq!(tui.mode, Mode::ImportReview);
        assert_eq!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .map(|plan| plan.candidates.len()),
            Some(1)
        );
        key(&mut tui, KeyCode::Char('e'));
        assert_eq!(tui.mode, Mode::FormInput);
        tui.form.as_mut().expect("import form").fields[0].value = "Edited Import Title".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ImportReview, "editing returns to review");
        assert_eq!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .and_then(|plan| plan.candidates.first())
                .map(|candidate| candidate.title().to_owned()),
            Some("Edited Import Title".to_owned())
        );
        key(&mut tui, KeyCode::Char('y'));
        wait_for_tracks(&mut tui, 1);
        assert_eq!(tui.mode, Mode::Normal);
        let id = tui.controller.tracks()[0].0;
        let artist = tui.controller.track_artist_id(id).expect("inferred artist");
        let source_message = tui.lifecycle_msg.clone().expect("import summary");
        assert!(source_message.contains("1 new"), "{source_message}");

        key(&mut tui, KeyCode::Char('f'));
        assert!(
            tui.controller
                .rating(id)
                .is_some_and(|rating| rating.favorite)
        );
        key(&mut tui, KeyCode::Char('4'));
        assert_eq!(
            tui.controller.rating(id).and_then(|rating| rating.rating),
            Some(4)
        );
        key(&mut tui, KeyCode::Char('0'));
        assert_eq!(
            tui.controller.rating(id).and_then(|rating| rating.rating),
            None
        );

        key(&mut tui, KeyCode::Char('D'));
        assert_eq!(tui.mode, Mode::InfoView);
        let album_details = tui.info_lines.join("\n");
        assert!(
            album_details.contains("AlbumId") && album_details.contains("owner"),
            "{album_details}"
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('P'));
        type_text(&mut tui, "Extra Artist");
        assert_eq!(tui.controller.artists().len(), 2);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|m| m.contains("Extra Artist"))
        );

        key(&mut tui, KeyCode::Char('e'));
        assert_eq!(tui.mode, Mode::FormInput);
        let form = tui.form.as_mut().expect("track form");
        form.fields[0].value = "Tui Title".to_owned();
        form.fields[1].value = "Tui Artist".to_owned();
        form.fields[2].value = "Tui Album".to_owned();
        form.fields[3].value = "3".to_owned();
        form.fields[4].value = "1".to_owned();
        key(&mut tui, KeyCode::Enter);
        let track = tui.controller.track(id).expect("edited");
        assert_eq!(track.title(), "Tui Title");
        assert_eq!(track.album(), Some("Tui Album"));
        assert_eq!(track.artist(), Some("Tui Artist"));
        assert_eq!(tui.controller.track_artist_id(id), Some(artist));
        assert_eq!(tui.controller.track_credits(id), vec![artist]);

        key(&mut tui, KeyCode::Char('e'));
        assert_eq!(tui.mode, Mode::FormInput);
        let form = tui.form.as_mut().expect("track form");
        form.fields[0].value = "Bad Title".to_owned();
        form.fields[3].value = "not-a-number".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(
            tui.mode,
            Mode::FormInput,
            "invalid input keeps the edit form open"
        );
        assert!(
            tui.form
                .as_ref()
                .and_then(|form| form.error.as_deref())
                .is_some_and(|error| error.contains("whole number")),
            "validation error is shown in the form"
        );
        assert_eq!(
            tui.controller.track(id).map(Track::title),
            Some("Tui Title")
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('V'));
        assert_eq!(tui.mode, Mode::InfoView);
        let artist_details = tui.info_lines.join("\n");
        assert!(
            artist_details.contains(&format!("ArtistId {artist} · Tui Artist")),
            "{artist_details}"
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('Y'));
        assert_eq!(tui.mode, Mode::FormInput);
        let form = tui.form.as_mut().expect("artist form");
        form.fields[0].value = "Renamed Tui Artist".to_owned();
        form.fields[1].value = "Tui AKA;Former Tui".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(
            tui.controller.track(id).and_then(Track::artist),
            Some("Renamed Tui Artist")
        );
        assert_eq!(
            tui.controller.artist_aliases(artist),
            ["Tui AKA".to_owned(), "Former Tui".to_owned()]
        );
        key(&mut tui, KeyCode::Char('Y'));
        let form = tui.form.as_mut().expect("artist form");
        form.fields[0].value = "Tui Artist II".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(
            tui.controller.artist_aliases(artist),
            ["Tui AKA".to_owned(), "Former Tui".to_owned()],
            "the prefilled alias field preserves existing aliases"
        );

        tui.controller
            .set_lyrics(id, Some("[00:01.000]Tui lyric\nplain tail"))
            .expect("lyrics");
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::LyricsView);
        assert!(
            tui.lyrics_lines
                .iter()
                .any(|line| line.contains("Tui lyric"))
        );
        key(&mut tui, KeyCode::Down);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.set_favorites(&[id], true).expect("favorite");
        key(&mut tui, KeyCode::Char('F'));
        assert_eq!(tui.rows().len(), 1, "favorites-only filter");
        key(&mut tui, KeyCode::Char('F'));
        key(&mut tui, KeyCode::Char('H'));
        assert!(tui.lifecycle_msg.as_deref().is_some_and(
            |message| message.contains("Most played") || message.contains("No meaningful")
        ));

        let outside = dir.path().join("Other Artist/Album/outside.wav");
        std::fs::create_dir_all(outside.parent().expect("parent")).expect("outside dir");
        let mut different = minimal_wav();
        different.push(0);
        std::fs::write(&outside, different).expect("outside");
        key(&mut tui, KeyCode::Char('I'));
        type_path(&mut tui, &outside);
        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 2);
        assert!(outside.is_file(), "source is never moved or deleted");
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_review_skips_without_copying_and_cancels_discovery() {
        let dir = TempDir::new("tui-import-review");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let before = std::fs::read(&source).expect("source bytes");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);

        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        wait_for_mode(&mut tui, Mode::ImportReview);
        key(&mut tui, KeyCode::Char('s'));
        assert!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .is_some_and(|plan| plan
                    .candidates
                    .iter()
                    .all(|candidate| candidate.decision() == ImportDecision::Skip))
        );
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(
            tui.mode,
            Mode::ImportReview,
            "preparing nothing is rejected in review"
        );
        assert_eq!(tui.controller.tracks().len(), 0);
        assert_eq!(std::fs::read(&source).expect("source"), before);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.tracks().len(), 0);

        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        key(&mut tui, KeyCode::Esc);
        wait_for_import_done(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 0);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.stop_import_worker();
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_review_preserves_safe_defaults_for_exact_audio_duplicates() {
        let dir = TempDir::new("tui-import-duplicate-default");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1);
        let managed = tui.controller.tracks()[0].1.source().path().to_path_buf();
        let duplicate = dir.path().join("same-content-copy.wav");
        std::fs::copy(&managed, &duplicate).expect("duplicate content");

        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &duplicate);
        wait_for_mode(&mut tui, Mode::ImportReview);
        let candidate = tui
            .import_ui
            .as_ref()
            .and_then(|ui| ui.plan.as_ref())
            .and_then(|plan| plan.candidates.first())
            .expect("candidate");
        assert!(candidate.status().can_update_metadata());
        assert_eq!(
            candidate.decision(),
            ImportDecision::Skip,
            "same-content duplicates must not be imported again by default"
        );
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.controller.tracks().len(), 1);
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_preparation_cancelled_after_worker_completion_publishes_nothing() {
        let dir = TempDir::new("tui-import-late-cancel");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        wait_for_mode(&mut tui, Mode::ImportReview);

        let copied = dir.path().join("unpublished-copy.wav");
        std::fs::write(&copied, minimal_wav()).expect("copy");
        let (tx, rx) = std::sync::mpsc::channel();
        let mut prepared = flux_app::import::PreparedImport::default();
        prepared.created_files.push(copied.clone());
        tx.send(ImportWorkerMessage::Prepared(Box::new(prepared)))
            .expect("send prepared");
        {
            let ui = tui.import_ui.as_mut().expect("review state");
            ui.cancel.store(true, Ordering::Release);
            ui.rx = rx;
        }
        tui.mode = Mode::ImportPreparing;
        tui.poll_import_worker();
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.tracks().len(), 0);
        assert!(!copied.exists(), "unpublished copy is removed");
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_duplicate_status_text_is_precise() {
        assert_eq!(candidate_status_detail(&CandidateStatus::New), None);
        assert_eq!(
            candidate_status_detail(&CandidateStatus::ExistingPath(TrackId::new(1))),
            Some("same managed file")
        );
        assert_eq!(
            candidate_status_detail(&CandidateStatus::SameContent {
                track_id: Some(TrackId::new(1)),
                other_source: None,
            }),
            Some("same audio · unique match")
        );
        assert_eq!(
            candidate_status_detail(&CandidateStatus::SameTitleDifferentAudio(TrackId::new(1))),
            Some("same title · different audio")
        );
        assert_eq!(
            candidate_status_detail(&CandidateStatus::ProbableAudio(TrackId::new(1))),
            Some("probable duplicate · verify")
        );
    }

    #[test]
    fn tui_import_review_updates_an_exact_duplicate_without_duplicating_it() {
        let dir = TempDir::new("tui-import-update");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1);
        let id = tui.controller.tracks()[0].0;
        let managed = tui
            .controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();

        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &managed);
        wait_for_mode(&mut tui, Mode::ImportReview);
        let mut rendered = Vec::new();
        tui.draw_sized(&mut rendered, 120, 36)
            .expect("duplicate review renders");
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(
            rendered.contains("same managed file"),
            "review shows a precise exact-path state"
        );
        assert!(
            rendered.contains("u update exact"),
            "review advertises the explicit repair action"
        );
        assert!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .and_then(|plan| plan.candidates.first())
                .is_some_and(|candidate| candidate.status().can_update_metadata())
        );
        key(&mut tui, KeyCode::Char('u'));
        assert!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .and_then(|plan| plan.candidates.first())
                .is_some_and(|candidate| candidate.decision() == ImportDecision::UpdateExisting)
        );
        key(&mut tui, KeyCode::Char('y'));
        wait_for_import_done(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1, "no duplicate TrackId");
        assert_eq!(
            tui.controller.tracks()[0].0,
            id,
            "the existing stable identity is preserved"
        );
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("1 updated")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.shutdown();
    }

    #[test]
    fn canonical_refresh_keeps_missing_track_identity() {
        use flux_app::StorageMode;

        let dir = TempDir::new("tui-missing");
        let root = dir.path().join("library");
        let source_dir = dir.path().join("Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("missing.wav");
        std::fs::write(&source, minimal_wav()).expect("audio");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        key(&mut tui, KeyCode::Char('I'));
        type_path(&mut tui, &source);
        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1);
        let id = tui.controller.tracks()[0].0;
        let audio = tui
            .controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();
        std::fs::remove_file(audio).expect("remove managed audio");
        key(&mut tui, KeyCode::Char('X'));
        assert!(tui.controller.track(id).is_some(), "identity is retained");
        assert_eq!(
            tui.controller.track_availability(id),
            Some(flux_app::Availability::Missing)
        );
        assert!(source.is_file(), "the original source remains untouched");
        tui.controller.shutdown();
    }

    #[test]
    fn migration_key_flow_plans_executes_and_activates() {
        use flux_app::StorageMode;

        let dir = TempDir::new("migrate-flow");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("root");
        let song = root.join("song.wav");
        std::fs::write(&song, minimal_wav()).expect("song");
        let index = dir.path().join("legacy.json");
        std::fs::write(
            &index,
            format!(
                r#"{{"version":1,"next_id":2,"tracks":[{{"id":1,"title":"Song","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}]}}"#,
                json_path(&song)
            ),
        )
        .expect("index");

        let destination = dir.path().join("CanonicalResult");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('M'));
        assert_eq!(tui.mode, Mode::MigrateSourceInput);
        type_path(&mut tui, &index);
        assert_eq!(tui.mode, Mode::MigrateDestInput);
        type_path(&mut tui, &destination);
        assert_eq!(tui.mode, Mode::MigrateConfirm);
        assert!(tui.pending_plan.is_some());
        assert!(!destination.exists(), "planning wrote nothing");
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);

        // Cancel at the confirmation step.
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.pending_plan.is_none());
        assert!(!destination.exists(), "cancel wrote nothing");

        // Again, this time confirm and activate.
        key(&mut tui, KeyCode::Char('M'));
        type_path(&mut tui, &index);
        type_path(&mut tui, &destination);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::MigratedDone);
        assert!(destination.join("catalog.flux").exists());
        assert_eq!(
            tui.controller.storage_mode(),
            StorageMode::Legacy,
            "execution alone never switches"
        );

        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(tui.mode, Mode::Normal);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        let ids: Vec<u64> = tui
            .controller
            .tracks()
            .iter()
            .map(|(id, _)| id.get())
            .collect();
        assert_eq!(ids, vec![1], "migrated ID preserved through activation");
        tui.controller.shutdown();
    }

    #[test]
    fn reconcile_key_reports_summary_and_never_switches() {
        use flux_app::StorageMode;

        let dir = TempDir::new("reconcile-key");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);

        // Legacy mode: X is refused visibly and changes nothing.
        key(&mut tui, KeyCode::Char('X'));
        assert_eq!(tui.controller.storage_mode(), StorageMode::Legacy);
        assert!(tui.controller.last_error().is_some());
        assert!(tui.lifecycle_msg.is_none(), "no false success summary");

        // Create + switch, then X reconciles and reports.
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        key(&mut tui, KeyCode::Char('X'));
        assert_eq!(tui.mode, Mode::Normal);
        let message = tui.lifecycle_msg.clone().expect("summary shown");
        assert!(
            message.contains("Refreshed the canonical library"),
            "{message}"
        );
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        tui.controller.shutdown();
    }

    /// The smallest valid PCM WAV header (no samples); nothing here
    /// plays it.
    fn wav_with_sample(sample: u8) -> Vec<u8> {
        let mut wav = minimal_wav();
        wav[4..8].copy_from_slice(&38u32.to_le_bytes());
        wav[40..44].copy_from_slice(&2u32.to_le_bytes());
        wav.extend_from_slice(&[sample, 0]);
        wav
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

    #[test]
    fn protected_index_key_preserves_and_reenables_writes() {
        let dir = TempDir::new("protected-index");
        let index = dir.path().join("library-index.json");
        std::fs::write(&index, b"not a valid index").expect("corrupt index");
        let mut tui = tui(&dir);
        assert!(tui.controller.legacy_index_write_protected());

        key(&mut tui, KeyCode::Char('U'));
        assert_eq!(tui.mode, Mode::ConfirmPreserveIndex);
        assert!(tui.controller.legacy_index_write_protected());
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.controller.legacy_index_write_protected());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("cancelled")),
            "{:?}",
            tui.lifecycle_msg
        );

        key(&mut tui, KeyCode::Char('U'));
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(!tui.controller.legacy_index_write_protected());
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("preserved")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.save_index().expect("new index writes");
        key(&mut tui, KeyCode::Char('A'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("no portable recovery")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.mode = Mode::ConfirmAcknowledgeRecovery;
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("remains read-only")),
            "{:?}",
            tui.lifecycle_msg
        );
        tui.controller.shutdown();
    }

    #[test]
    fn queue_keys_route_add_play_next_remove_and_reorder_through_controller() {
        let dir = TempDir::new("queue-keys");
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
                json_path(&one),
                json_path(&two)
            ),
        )
        .expect("index");
        let mut tui = tui(&dir);
        assert_eq!(tui.controller.queue_len(), 2);

        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(
            tui.controller.queue_len(),
            3,
            "a appends the selected track"
        );

        key(&mut tui, KeyCode::Char('Q'));
        key(&mut tui, KeyCode::Delete);
        assert_eq!(
            tui.controller.queue_len(),
            2,
            "Delete removes the selected queue entry from the queue view"
        );
        assert_eq!(
            tui.controller.player_state(),
            flux_core::PlaybackState::Stopped
        );
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::Char('x'));
        assert_eq!(
            tui.controller.queue_len(),
            3,
            "x inserts the selected track after the current entry"
        );
        let current = tui
            .controller
            .current_queue_entry_id()
            .expect("current after removal/reinsertion");
        let before = tui
            .controller
            .queue_entry_index(current)
            .expect("current index");
        assert!(
            !tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
            "Ctrl+Down is not quit"
        );
        assert_eq!(
            tui.controller.current_queue_entry_id(),
            Some(current),
            "reordering keeps the current entry identity"
        );
        assert_eq!(tui.controller.queue_entry_index(current), Some(before + 1));
        tui.controller.shutdown();
    }

    #[test]
    fn tiny_terminals_are_detected_before_clamping() {
        assert!(terminal_too_small(19, 24));
        assert!(terminal_too_small(80, 5));
        assert!(!terminal_too_small(20, 6));
        assert!(!terminal_too_small(120, 40));
    }

    #[test]
    fn escape_without_a_search_does_not_reset_the_cursor() {
        let dir = TempDir::new("escape-no-search");
        let mut tui = tui(&dir);
        tui.cursor = 3;
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.cursor, 3);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("No active search"))
        );
        tui.controller.shutdown();
    }

    #[test]
    fn pad_with_right_handles_degenerate_widths() {
        assert_eq!(pad_with_right("title", "3:45", 10), "title 3:45");
        assert_eq!(pad_with_right("title", "", 10), "title");
        assert_eq!(pad_with_right("title", "3:45", 4), "titl");
        assert_eq!(pad_with_right("title", "3:45", 0), "");
        assert_eq!(
            pad_with_right("a longer title here", "3:45", 10),
            "a lon 3:45"
        );
        assert_eq!(display_width("界"), 2);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(truncate_to_width("界界界", 5), "界界");
        assert_eq!(display_width(&pad_with_right("界界", "x", 5)), 5);
        let tail = fit_input_line("", "界界界", 4);
        assert!(display_width(&tail) <= 4, "{tail}");
        assert!(tail.starts_with('…'), "{tail}");
    }

    #[test]
    fn deletion_and_transfer_keys_refuse_to_bypass_gui_workflow() {
        let dir = TempDir::new("tui-delete-refusal");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('T'));
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("GUI"))
        );
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn help_info_and_input_lines_support_discovery_and_long_unicode_paths() {
        let dir = TempDir::new("tui-help-info");
        let mut tui = tui(&dir);
        assert!(normal_help().contains("? help"));
        assert!(
            tui.library_header_identity(40)
                .contains("library-index.json")
        );
        key(&mut tui, KeyCode::Char('?'));
        assert_eq!(tui.mode, Mode::InfoView);
        assert!(tui.info_title.contains("help"));
        let help = tui.info_lines.join("\n");
        assert!(help.contains("QUEUE"), "{help}");
        assert!(help.contains("Active library:"), "{help}");
        assert!(help.contains("system default"), "{help}");
        assert!(
            help.contains("bounded automatic recovery") || help.contains("recovery"),
            "{help}"
        );
        assert!(help.contains("SAFETY"), "{help}");
        assert!(help.contains("audio device selection"), "{help}");
        assert!(help.contains("F1–F8"), "{help}");
        assert!(
            help.contains("Source → Destination → Verify → Import"),
            "{help}"
        );
        assert!(help.contains("real post-gain waveform"), "{help}");
        assert!(
            help.contains("final canonical transaction is never cancelled"),
            "{help}"
        );
        key(&mut tui, KeyCode::Down);
        assert_eq!(tui.info_scroll, 1);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.lifecycle_msg = Some("transient notice".to_owned());
        key(&mut tui, KeyCode::Char('c'));
        assert!(tui.lifecycle_msg.is_none());

        assert_eq!(clamp_terminal_size(0, 0), (20, 6));
        assert_eq!(clamp_terminal_size(120, 40), (120, 40));
        assert!(!detail_view_fits(6));
        assert!(detail_view_fits(8));
        assert_eq!(
            resource_label(Some(flux_app::ResourceState::Missing)),
            Some("[missing]")
        );
        assert_eq!(
            resource_label(Some(flux_app::ResourceState::Unavailable(
                "permission denied".to_owned()
            ))),
            Some("[inaccessible]")
        );
        assert_eq!(resource_label(Some(flux_app::ResourceState::Present)), None);
        assert_eq!(resource_label(None), None);
        let long = format!("{}{}", "a".repeat(80), "é".repeat(20));
        let line = fit_input_line("Path: ", &long, 24);
        assert_eq!(line.chars().count(), 24);
        assert!(line.starts_with('…'), "{line}");
        assert!(line.ends_with('_'), "{line}");
        assert_eq!(fit_input_line("Path: ", "short", 40), "Path: short_");
        key(&mut tui, KeyCode::Char('?'));
        assert!(
            tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "Ctrl-C must quit from an info view"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn metadata_edit_forms_parse_optional_artist_album_and_asset_fields() {
        let artist = parse_artist_edit_form("7|New Name|Alias A;Alias B|Description|/tmp/a.png")
            .expect("artist form");
        assert_eq!(artist.id.get(), 7);
        assert_eq!(artist.name, "New Name");
        assert_eq!(
            artist.aliases.as_deref(),
            Some(["Alias A".to_owned(), "Alias B".to_owned()].as_slice())
        );
        assert_eq!(artist.description, Some(Some("Description".to_owned())));
        assert_eq!(artist.image, ImageEdit::Set(PathBuf::from("/tmp/a.png")));

        let minimal = parse_artist_edit_form("7|New Name").expect("minimal artist form");
        assert!(minimal.aliases.is_none());
        assert!(minimal.description.is_none());
        assert_eq!(minimal.image, ImageEdit::Preserve);

        let cleared = parse_artist_edit_form("7|New Name||| - ").expect("clear fields");
        assert_eq!(cleared.aliases.as_deref(), Some([].as_slice()));
        assert_eq!(cleared.description, Some(None));
        assert_eq!(cleared.image, ImageEdit::Remove);

        assert!(parse_artist_edit_form("7|").is_err());
        assert!(parse_artist_edit_form("not-an-id|Name").is_err());

        let album = parse_album_edit_form("3|Title|ep|-").expect("album form");
        assert_eq!(album.id.get(), 3);
        assert_eq!(album.title, "Title");
        assert_eq!(album.release_type, flux_app::ReleaseType::Ep);
        assert_eq!(album.cover, ImageEdit::Remove);
        assert_eq!(
            parse_release_type("single").expect("single"),
            flux_app::ReleaseType::Single
        );
        assert!(parse_release_type("mixtape").is_err());
        assert_eq!(
            album_edit_form(album.id, &album.title, album.release_type),
            "3|Title|ep|"
        );
    }

    #[test]
    fn tui_artist_artwork_and_lyrics_workflows_update_only_managed_resources() {
        let dir = TempDir::new("tui-metadata-workflows");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical library");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let artist = tui.controller.create_artist("Old Artist").expect("artist");
        key(&mut tui, KeyCode::F(3));
        assert_eq!(tui.view, View::Artists);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.detail, Some(Detail::Artist(artist)));
        key(&mut tui, KeyCode::Char('Y'));
        assert_eq!(tui.mode, Mode::FormInput);
        let form = tui.form.as_mut().expect("artist form");
        form.fields[0].value = "New Artist".to_owned();
        form.fields[1].value = "Alias;Second".to_owned();
        form.fields[2].value = "Artist description".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::Normal);
        let updated = tui.controller.artist(artist).expect("artist");
        assert_eq!(updated.name(), "New Artist");
        assert_eq!(
            tui.controller.artist_aliases(artist),
            ["Alias".to_owned(), "Second".to_owned()]
        );
        assert_eq!(
            tui.controller.artist_description(artist),
            Some("Artist description")
        );

        let source = dir.path().join("source/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source dir");
        std::fs::write(&source, minimal_wav()).expect("song");
        tui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("canonical import");
        let track = tui.controller.tracks()[0].0;
        key(&mut tui, KeyCode::F(2));
        assert_eq!(tui.view, View::Tracks);

        let artwork = dir.path().join("cover.png");
        std::fs::write(
            &artwork,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .expect("artwork");
        key(&mut tui, KeyCode::Char('W'));
        // The form is prefilled with "track-id|"; append only the path.
        type_text(&mut tui, &artwork.display().to_string());
        let stored_art = tui
            .controller
            .track_artwork_path(track)
            .expect("managed artwork path");
        assert!(stored_art.is_file());
        assert_ne!(stored_art, artwork);

        let lyrics = dir.path().join("song.lrc");
        std::fs::write(&lyrics, "[00:01.00]hello\n").expect("lyrics");
        key(&mut tui, KeyCode::Char('S'));
        type_text(&mut tui, &lyrics.display().to_string());
        assert_eq!(
            tui.controller.track_lyrics(track).as_deref(),
            Some("[00:01.00]hello\n")
        );
        key(&mut tui, KeyCode::Char('S'));
        type_text(&mut tui, "-");
        assert!(tui.controller.track_lyrics(track).is_none());
        tui.controller.shutdown();
    }

    #[test]
    fn large_track_list_reuses_cached_rows() {
        let dir = TempDir::new("tui-large-track-list");
        let tracks = (1..=5_000)
            .map(|id| {
                format!(
                    r#"{{"id":{id},"title":"Track {id}","artist":null,"album":null,"track_number":null,"disc_number":null,"duration_ms":null,"path":{{"utf8":"{}"}}}}"#,
                    json_path(&dir.path().join(format!("music/{id}.wav")))
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            dir.path().join("library-index.json"),
            format!(r#"{{"version":1,"next_id":5001,"tracks":[{tracks}]}}"#),
        )
        .expect("large index");
        let mut tui = tui(&dir);
        let started = std::time::Instant::now();
        let first = tui.rows();
        let first_elapsed = started.elapsed();
        let second = tui.rows();
        assert_eq!(first.len(), 5_000);
        assert!(
            std::rc::Rc::ptr_eq(&first, &second),
            "unchanged large list must reuse its cached rows"
        );
        println!("5,000-track TUI row build: {first_elapsed:?}");
        let mut rendered = Vec::new();
        let started = std::time::Instant::now();
        for _ in 0..50 {
            rendered.clear();
            tui.draw(&mut rendered).expect("large draw");
        }
        println!("5,000-track TUI draw: {:?}/frame", started.elapsed() / 50);
        tui.controller.shutdown();
    }

    #[test]
    fn queue_view_preserves_entry_identity_and_supports_play_move_and_remove() {
        let dir = TempDir::new("tui-queue-view");
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
                json_path(&one),
                json_path(&two)
            ),
        )
        .expect("index");
        let mut tui = tui(&dir);
        let first = tui.controller.tracks()[0].0;
        tui.controller
            .enqueue_track(first)
            .expect("duplicate entry");
        let entries = tui.controller.queue_entries().to_vec();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.track() == first)
                .count(),
            2
        );
        assert_ne!(entries[0].id(), entries[2].id());

        key(&mut tui, KeyCode::Char('Q'));
        assert_eq!(tui.mode, Mode::QueueView);
        let selected = tui.controller.queue_entries()[tui.queue_cursor].id();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.controller.current_queue_entry_id(), Some(selected));
        let before_index = tui.queue_cursor;
        assert!(
            !tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
            "Ctrl+Down must not quit"
        );
        assert_eq!(tui.controller.current_queue_entry_id(), Some(selected));
        assert_eq!(tui.queue_cursor, before_index + 1);

        let mut rendered = Vec::new();
        tui.draw(&mut rendered).expect("queue view render");
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(rendered.contains("Queue"), "{rendered}");
        assert!(rendered.contains('▶'), "{rendered}");

        let len_before = tui.controller.queue_len();
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.controller.queue_len(), len_before - 1);
        assert!(
            tui.controller
                .queue_entries()
                .iter()
                .all(|entry| entry.id() != selected)
        );
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_partial_success_preserves_every_issue_detail() {
        let dir = TempDir::new("tui-import-issues");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source = dir.path().join("Source/Album/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source dir");
        std::fs::write(&source, minimal_wav()).expect("source");
        let missing = dir.path().join("Source/Album/missing.wav");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_text(
            &mut tui,
            &format!("{};{}", source.display(), missing.display()),
        );

        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1);
        assert_eq!(tui.mode, Mode::InfoView);
        let summary = tui.lifecycle_msg.as_deref().expect("summary");
        assert!(summary.contains("1 new"), "{summary}");
        assert!(summary.contains("1 discovery issue"), "{summary}");
        let details = tui.info_lines.join("\n");
        assert!(
            details.contains(&missing.display().to_string()),
            "{details}"
        );
        assert_eq!(tui.info_title, "Import results");
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);
        tui.controller.shutdown();
    }

    #[test]
    fn tui_folder_import_reports_unsupported_files_and_missing_directories() {
        let dir = TempDir::new("tui-folder-issues");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let folder = dir.path().join("Incoming");
        std::fs::create_dir_all(&folder).expect("folder");
        std::fs::write(folder.join("good.wav"), minimal_wav()).expect("good wav");
        std::fs::write(folder.join("notes.txt"), b"not audio").expect("text file");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('o'));
        type_path(&mut tui, &folder);
        finish_import(&mut tui);
        assert_eq!(tui.controller.tracks().len(), 1);
        assert_eq!(tui.mode, Mode::InfoView);
        let details = tui.info_lines.join("\n");
        assert!(details.contains("notes.txt"), "{details}");
        key(&mut tui, KeyCode::Esc);

        let missing = dir.path().join("does-not-exist");
        key(&mut tui, KeyCode::Char('o'));
        type_path(&mut tui, &missing);
        wait_for_mode(&mut tui, Mode::InfoView);
        assert!(
            tui.lifecycle_msg
                .as_deref()
                .is_some_and(|message| message.contains("no supported audio files")),
            "{:?}",
            tui.lifecycle_msg
        );
        assert_eq!(tui.controller.storage_mode(), StorageMode::Canonical);
        tui.controller.shutdown();
    }

    #[test]
    fn empty_messages_distinguish_search_and_favorites() {
        let dir = TempDir::new("tui-empty-messages");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source = dir.path().join("Artist/Album/song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, minimal_wav()).expect("song");
        let mut tui = tui(&dir);
        assert!(tui.empty_rows_message().contains("Library is empty"));
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        finish_import(&mut tui);
        tui.search = "not present".to_owned();
        assert!(tui.empty_rows_message().contains("current search"));
        tui.favorites_only = true;
        assert!(
            tui.empty_rows_message()
                .contains("No favorite tracks match")
        );
        tui.search.clear();
        assert!(tui.empty_rows_message().contains("No favorite tracks"));
        tui.controller.shutdown();
    }

    #[test]
    fn tui_marks_and_inspects_standalone_tracks_without_album_membership() {
        let dir = TempDir::new("tui-standalone");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        let source = dir.path().join("source/standalone.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("source");
        std::fs::write(&source, minimal_wav()).expect("wav");
        let artist = tui.controller.create_artist("Solo Artist").expect("artist");
        let id = tui
            .controller
            .import_canonical_track(flux_app::CanonicalImportedTrack {
                source,
                title: "Standalone Song".to_owned(),
                artist_id: Some(artist),
                artist_name: "Solo Artist".to_owned(),
                credited_artist_ids: vec![artist],
                album_title: None,
                release_type: flux_app::ReleaseType::Single,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("standalone import");
        assert_eq!(tui.controller.track_album_id(id), None);

        let mut rendered = Vec::new();
        tui.draw(&mut rendered).expect("draw");
        let rendered = String::from_utf8_lossy(&rendered);
        assert!(rendered.contains("[standalone]"), "{rendered}");

        key(&mut tui, KeyCode::Char('E'));
        assert_eq!(tui.mode, Mode::InfoView);
        let albums = tui.info_lines.join("\n");
        assert!(albums.contains("Standalone Tracks"), "{albums}");
        assert!(albums.contains("Standalone Song"), "{albums}");
        key(&mut tui, KeyCode::Esc);
        key(&mut tui, KeyCode::Char('D'));
        assert_eq!(tui.mode, Mode::InfoView);
        assert_eq!(tui.info_title, "Selected standalone Track");
        assert!(
            tui.info_lines
                .iter()
                .any(|line| line.contains("no Album membership"))
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_lists_empty_albums_with_owner_and_count() {
        let dir = TempDir::new("tui-empty-albums");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        let artist = tui.controller.create_artist("Artist").expect("artist");
        tui.controller
            .create_album(artist, "Empty", flux_app::ReleaseType::Album)
            .expect("album");
        key(&mut tui, KeyCode::Char('E'));
        assert_eq!(tui.mode, Mode::InfoView);
        let message = tui.info_lines.join("\n");
        assert!(message.contains("Empty"), "{message}");
        assert!(message.contains("0 track(s)"), "{message}");
        assert!(message.contains("artwork no"), "{message}");
        assert!(message.contains("Empty albums are retained"), "{message}");
        tui.controller.shutdown();
    }
    #[test]
    fn tui_navigation_opens_artists_albums_playlists_and_collections() {
        let dir = TempDir::new("tui-view-navigation");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let artist = tui.controller.create_artist("Alpha").expect("artist");
        tui.controller
            .create_album(artist, "First Light", flux_app::ReleaseType::Album)
            .expect("album");

        key(&mut tui, KeyCode::F(3));
        assert_eq!(tui.view, View::Artists);
        assert!(
            tui.view_rows()
                .iter()
                .any(|row| matches!(row, Row::Artist(_)))
        );
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.detail, Some(Detail::Artist(artist)));
        assert!(
            tui.view_rows()
                .iter()
                .any(|row| matches!(row, Row::Album(_))),
            "artist detail lists owned albums"
        );
        key(&mut tui, KeyCode::Esc);
        assert!(tui.detail.is_none());

        key(&mut tui, KeyCode::F(4));
        assert_eq!(tui.view, View::Albums);
        key(&mut tui, KeyCode::Enter);
        assert!(matches!(tui.detail, Some(Detail::Album(_))));
        key(&mut tui, KeyCode::Esc);

        key(&mut tui, KeyCode::F(5));
        key(&mut tui, KeyCode::Char('N'));
        assert_eq!(tui.mode, Mode::FormInput);
        tui.form.as_mut().expect("playlist form").fields[0].value = "Night Mix".to_owned();
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.controller.playlists().len(), 1);
        assert_eq!(tui.controller.playlists()[0].name, "Night Mix");

        key(&mut tui, KeyCode::F(6));
        assert_eq!(tui.view, View::Collections);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.view, View::Tracks);
        assert_eq!(
            tui.active_collection,
            Some(flux_app::SmartCollection::Favorites)
        );
        tui.controller.shutdown();
    }

    #[test]
    fn visualization_is_derived_from_real_waveform_samples() {
        let silence = [0.0f32; 64];
        let loud = [1.0f32; 64];
        let quiet_bars = visualization_canvas(&silence, VizMode::Bars, 1.0, 16, 4);
        let loud_bars = visualization_canvas(&loud, VizMode::Bars, 1.0, 16, 4);
        assert_ne!(quiet_bars, loud_bars);
        assert!(
            loud_bars.iter().any(|line| line.contains('█')),
            "measured peaks reach the top level"
        );
        assert!(
            quiet_bars.iter().all(|line| !line.contains('█')),
            "silence stays quiet"
        );

        let quiet_pulse = visualization_canvas(&silence, VizMode::Pulse, 1.0, 8, 5);
        let loud_pulse = visualization_canvas(&loud, VizMode::Pulse, 1.0, 8, 5);
        assert_ne!(quiet_pulse, loud_pulse);

        let low_gain = visualization_canvas(&loud, VizMode::Bars, 0.5, 16, 4);
        let high_gain = visualization_canvas(&loud, VizMode::Bars, 1.5, 16, 4);
        assert_ne!(low_gain, high_gain, "sensitivity changes the display");

        let waveform = visualization_canvas(&loud, VizMode::Waveform, 1.0, 16, 5);
        assert!(waveform.iter().any(|line| line.contains('•')));
    }

    #[test]
    fn queue_clear_requires_confirmation_and_preserves_the_library() {
        let dir = TempDir::new("tui-clear-queue");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        tui.controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("import");
        let id = tui.controller.tracks()[0].0;
        tui.controller.clear_queue();
        tui.controller.enqueue_track(id).expect("queue");
        key(&mut tui, KeyCode::Char('Q'));
        key(&mut tui, KeyCode::Char('C'));
        assert_eq!(tui.mode, Mode::ConfirmQueueClear);
        assert_eq!(tui.controller.queue_len(), 1);
        key(&mut tui, KeyCode::Char('n'));
        assert_eq!(
            tui.controller.queue_len(),
            1,
            "cancel leaves the queue intact"
        );
        key(&mut tui, KeyCode::Char('C'));
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.controller.queue_len(), 0);
        assert!(
            tui.controller.track(id).is_some(),
            "catalogue track remains"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_home_destination_opens_shared_collections() {
        let dir = TempDir::new("tui-home");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::F(1));
        assert_eq!(tui.view, View::Home);
        let rows = tui.view_rows();
        assert_eq!(rows.len(), 5, "Home exposes the five shared collections");
        assert!(rows.iter().all(|row| matches!(row, Row::Collection(_))));
        assert!(tui.view_title().starts_with("Home"));
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.view, View::Tracks);
        assert!(tui.active_collection.is_some());
        tui.controller.shutdown();
    }

    #[test]
    fn required_terminal_sizes_render_with_one_primary_timeline() {
        let dir = TempDir::new("tui-required-sizes");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("Artist/Album/01 - Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, wav_with_sample(3)).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        tui.controller
            .import_files(&[source], false)
            .expect("import");
        let track = tui.controller.tracks()[0].0;
        tui.controller.play_track(track);

        for (width, height) in [(120usize, 36usize), (92, 24), (80, 24), (40, 12)] {
            let mut output = Vec::new();
            tui.draw_sized(&mut output, width as u16, height as u16)
                .unwrap_or_else(|error| panic!("{width}x{height}: {error}"));
            let rendered = String::from_utf8_lossy(&output);
            assert!(!rendered.is_empty(), "{width}x{height} rendered nothing");
            assert!(
                rendered.contains("Flux"),
                "{width}x{height} lost the product identity"
            );
            assert!(
                rendered.contains('░'),
                "{width}x{height} renders the primary timeline"
            );
            assert!(
                !rendered.contains(" / --:--"),
                "{width}x{height} duplicates duration outside the timeline"
            );
            let content_width = width.saturating_sub(tui.content_offset);
            let expected_side =
                if tui.layout == LayoutMode::Standard && content_width >= 86 && height >= 10 {
                    content_width / 3
                } else {
                    0
                };
            assert_eq!(
                tui.side_panel_width(content_width, height),
                expected_side,
                "{width}x{height} side-panel threshold"
            );
        }

        let album = tui
            .controller
            .track_album_id(track)
            .expect("album membership");
        tui.view = View::Albums;
        tui.detail = Some(Detail::Album(album));
        let mut output = Vec::new();
        tui.draw_sized(&mut output, 120, 36).expect("album layout");
        let rendered = String::from_utf8_lossy(&output);
        assert!(
            rendered.contains("▸ Albums"),
            "the active destination remains marked in detail views"
        );
        assert!(
            !rendered.contains("Playback timeline below"),
            "the side panel does not repeat playback placement copy"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn persistent_sidebar_requires_width_and_height_and_hides_for_modals() {
        let dir = TempDir::new("tui-sidebar-adaptive");
        let mut subject = tui(&dir);
        assert_eq!(subject.sidebar_width(120, 30), 24);
        assert_eq!(subject.sidebar_width(92, 30), 18);
        assert_eq!(subject.sidebar_width(91, 30), 0);
        assert_eq!(subject.sidebar_width(120, 17), 0);
        subject.layout = LayoutMode::Compact;
        assert_eq!(subject.sidebar_width(120, 30), 0);
        subject.layout = LayoutMode::Standard;
        subject.mode = Mode::ImportReview;
        assert_eq!(subject.sidebar_width(120, 30), 0);
        subject.mode = Mode::Normal;
        subject.view = View::Artists;
        assert_eq!(subject.sidebar_width(120, 30), 24);
        subject.controller.shutdown();
    }

    #[test]
    fn playback_progress_is_a_single_primary_timeline() {
        let dir = TempDir::new("tui-single-timeline");
        let subject = tui(&dir);
        let summary = subject.now_playing_line(120);
        assert!(
            !summary.contains('█') && !summary.contains('░'),
            "the header/summary must not duplicate the playback timeline: {summary}"
        );
        let mut output = Vec::new();
        subject
            .draw_timeline(&mut output, 5, 80)
            .expect("timeline draws");
        let rendered = String::from_utf8_lossy(&output);
        assert!(
            rendered.contains('█') || rendered.contains('░'),
            "the primary timeline renders a real progress bar: {rendered:?}"
        );
        subject.controller.shutdown();
    }

    #[test]
    fn tui_preferences_persist_layout_and_visualizer_choices() {
        let dir = TempDir::new("tui-preferences");
        let mut subject = tui(&dir);
        let path = dir.path().join("tui.conf");
        subject.prefs_path = Some(path.clone());
        subject.viz_visible = true;
        key(&mut subject, KeyCode::Char('m'));
        key(&mut subject, KeyCode::Char('v'));
        assert!(!subject.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)));
        let text = std::fs::read_to_string(&path).expect("preferences");
        assert!(text.contains("layout=library"), "{text}");
        assert!(text.contains("visualizer=hidden"), "{text}");
        assert!(text.contains("visualization_mode=bars"), "{text}");

        subject.controller.shutdown();
        let restored_dir = TempDir::new("tui-preferences-restored");
        let mut restored_tui = tui(&restored_dir);
        restored_tui.prefs_path = Some(path);
        restored_tui.load_preferences();
        assert_eq!(restored_tui.layout, LayoutMode::Library);
        assert!(!restored_tui.viz_visible);
        assert_eq!(restored_tui.viz_mode, VizMode::Bars);
        restored_tui.controller.shutdown();
    }
    #[test]
    fn command_palette_switches_views_and_visualizer_settings() {
        let dir = TempDir::new("tui-command-palette");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char(':'));
        assert_eq!(tui.mode, Mode::CommandInput);
        type_text(&mut tui, "artists");
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.view, View::Artists);

        key(&mut tui, KeyCode::Char(':'));
        type_text(&mut tui, "layout player");
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.layout, LayoutMode::Player);

        key(&mut tui, KeyCode::Char(':'));
        type_text(&mut tui, "viz bars");
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.viz_mode, VizMode::Bars);
        assert!(tui.viz_visible);
        tui.controller.shutdown();
    }
    #[test]
    fn tui_completes_album_order_membership_and_safe_deletion() {
        let dir = TempDir::new("tui-album-completion");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source_dir = dir.path().join("source/Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let mut sources = Vec::new();
        for (name, sample) in [("01 - One.wav", 1u8), ("02 - Two.wav", 2u8)] {
            let path = source_dir.join(name);
            std::fs::write(&path, wav_with_sample(sample)).expect("wav");
            sources.push(path);
        }
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        tui.controller
            .import_files(&sources, false)
            .expect("import");
        let album = tui
            .controller
            .tracks()
            .iter()
            .find_map(|(id, _)| tui.controller.track_album_id(*id))
            .expect("album");
        let owner = tui.controller.album_owner(album).expect("owner");
        let second = tui
            .controller
            .create_album(owner, "Second Album", flux_app::ReleaseType::Album)
            .expect("second album");

        tui.detail = Some(Detail::Album(album));
        tui.cursor = 0;
        let before = tui.controller.album_tracks(album);
        key_mod(&mut tui, KeyCode::Down, KeyModifiers::CONTROL);
        let after = tui.controller.album_tracks(album);
        assert_ne!(before, after, "album order changed");
        assert_eq!(after.len(), 2);

        let track = after[0];
        tui.cursor = 0;
        key_mod(&mut tui, KeyCode::Char('m'), KeyModifiers::ALT);
        assert_eq!(tui.mode, Mode::TrackMovePick);
        let target = tui
            .move_targets()
            .iter()
            .position(|target| target.album == Some(second))
            .expect("destination album");
        tui.move_cursor = target;
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ConfirmTrackMove);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.controller.track_album_id(track), Some(second));

        tui.detail = Some(Detail::Album(second));
        tui.cursor = 0;
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.mode, Mode::ConfirmTrackDelete);
        assert_eq!(tui.controller.album_tracks(second), vec![track]);
        key(&mut tui, KeyCode::Char('y'));
        assert!(tui.controller.track(track).is_none());
        assert!(tui.controller.album(second).is_some());

        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.mode, Mode::ConfirmAlbumDelete);
        key(&mut tui, KeyCode::Char('y'));
        assert!(tui.controller.album(second).is_none());
        tui.controller.shutdown();
    }

    #[test]
    fn tui_exports_lyrics_and_attaches_import_assets() {
        let dir = TempDir::new("tui-lyrics-export");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source_dir = dir.path().join("source/Artist/Album");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("01 - Song.wav");
        std::fs::write(&source, minimal_wav()).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        tui.controller
            .import_files(&[source], false)
            .expect("import");
        let id = tui.controller.tracks()[0].0;
        tui.controller
            .set_lyrics(id, Some("[00:01.000]line"))
            .expect("lyrics");
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::LyricsView);
        key(&mut tui, KeyCode::Char('x'));
        assert_eq!(tui.mode, Mode::LyricsExportInput);
        type_path(&mut tui, &dir.path().join("export.lrc"));
        assert_eq!(tui.mode, Mode::LyricsView);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("export.lrc")).expect("export"),
            "[00:01.000]line"
        );
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::Normal);

        // Import asset attachment uses the shared import candidate assets.
        let import_source = source_dir.join("02 - Art.wav");
        std::fs::write(&import_source, minimal_wav()).expect("wav");
        tui.controller.switch_to_canonical(&root).expect("reopen");
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &import_source);
        wait_for_mode(&mut tui, Mode::ImportReview);
        let png = dir.path().join("cover.png");
        std::fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .expect("png");
        key(&mut tui, KeyCode::Char('A'));
        assert_eq!(tui.mode, Mode::ImportAssetInput);
        type_path(&mut tui, &png);
        assert_eq!(tui.mode, Mode::ImportReview);
        assert!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .and_then(|plan| plan.candidates.first())
                .and_then(|candidate| candidate.album_artwork_path())
                .is_some(),
            "album artwork attached to review"
        );
        tui.controller.shutdown();
    }
    #[test]
    fn tui_reviews_managed_audio_replacement_through_the_shared_plan() {
        let dir = TempDir::new("tui-replace-audio");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("source/Artist/Album/01 - Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, wav_with_sample(1)).expect("wav");
        let replacement = dir.path().join("replacement.wav");
        std::fs::write(&replacement, wav_with_sample(7)).expect("replacement");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        tui.controller
            .import_files(&[source], false)
            .expect("import");
        let id = tui.controller.tracks()[0].0;
        key_mod(&mut tui, KeyCode::Char('r'), KeyModifiers::ALT);
        assert_eq!(tui.mode, Mode::TrackReplacementInput);
        type_path(&mut tui, &replacement);
        assert_eq!(tui.mode, Mode::ConfirmTrackReplacement);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.controller.track(id).is_some());
        tui.controller.shutdown();
    }
    #[test]
    fn same_named_artists_are_disambiguated_in_tui_labels() {
        let dir = TempDir::new("tui-same-name-artists");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let first = tui.controller.create_artist("Nirvana").expect("first");
        let second = tui.controller.create_artist("Nirvana").expect("second");
        let first_label = artist_display_name(&tui.controller, first);
        let second_label = artist_display_name(&tui.controller, second);
        assert_ne!(first_label, second_label);
        assert!(first_label.contains(&format!("ID {first}")));
        assert!(second_label.contains(&format!("ID {second}")));
        tui.view = View::Artists;
        let rendered = tui
            .view_rows()
            .iter()
            .map(|row| tui.row_text(*row, 100))
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|label| label.contains(&format!("ID {first}")))
        );
        assert!(
            rendered
                .iter()
                .any(|label| label.contains(&format!("ID {second}")))
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_review_adds_and_removes_album_participants() {
        let dir = TempDir::new("tui-import-participants");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).expect("root");
        let source_dir = dir.path().join("Alpha/Shared");
        std::fs::create_dir_all(&source_dir).expect("source");
        let source = source_dir.join("song.wav");
        std::fs::write(&source, minimal_wav()).expect("source");
        let mut tui = tui(&dir);
        key(&mut tui, KeyCode::Char('C'));
        type_path(&mut tui, &root);
        let alpha = tui.controller.create_artist("Alpha").expect("alpha");
        let beta = tui.controller.create_artist("Beta").expect("beta");
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        wait_for_mode(&mut tui, Mode::ImportReview);

        key(&mut tui, KeyCode::Char('p'));
        assert_eq!(tui.mode, Mode::ImportParticipants);
        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(tui.mode, Mode::ImportParticipantPick);
        assert_eq!(tui.import_participant_candidates(), vec![beta]);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ImportParticipants);
        assert_eq!(tui.import_participant_ids(), vec![beta]);
        for (width, height) in [(120u16, 36u16), (80, 24), (40, 12)] {
            let mut rendered = Vec::new();
            tui.draw_sized(&mut rendered, width, height)
                .expect("participant view renders");
            assert!(!rendered.is_empty(), "{width}x{height}");
        }
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.mode, Mode::ImportReview);
        let candidate = tui
            .import_ui
            .as_ref()
            .and_then(|ui| ui.plan.as_ref())
            .and_then(|plan| plan.candidates.first())
            .expect("candidate");
        assert_eq!(candidate.album_participants(), &[beta]);
        assert_eq!(
            tui.import_review_participant_context()
                .and_then(|(_, _, owner, _)| owner),
            Some(alpha),
            "the review identifies the unique physical owner without merging names"
        );

        key(&mut tui, KeyCode::Char('p'));
        key(&mut tui, KeyCode::Char('r'));
        assert!(tui.import_participant_ids().is_empty());
        key(&mut tui, KeyCode::Esc);
        key(&mut tui, KeyCode::Char('p'));
        key(&mut tui, KeyCode::Char('a'));
        key(&mut tui, KeyCode::Enter);
        key(&mut tui, KeyCode::Esc);
        assert_eq!(tui.import_participant_ids(), vec![beta]);
        key(&mut tui, KeyCode::Enter);
        wait_for_mode(&mut tui, Mode::Normal);
        assert_eq!(tui.controller.tracks().len(), 1);
        let id = tui.controller.tracks()[0].0;
        let album = tui.controller.track_album_id(id).expect("album");
        assert_eq!(
            tui.controller.track_credits(id),
            vec![alpha],
            "Track credits remain independent of Album participation"
        );
        assert_eq!(tui.controller.track_artist_id(id), Some(alpha));
        assert_eq!(
            tui.controller.album_participants(album),
            vec![alpha, beta],
            "the reviewed participant is published without changing the owner"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_navigates_and_edits_collaborative_album_participants() {
        let dir = TempDir::new("tui-album-participants");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let alpha = tui.controller.create_artist("Alpha").expect("alpha");
        let beta = tui.controller.create_artist("Beta").expect("beta");
        let gamma = tui.controller.create_artist("Gamma").expect("gamma");
        let album = tui
            .controller
            .create_album(alpha, "Shared Release", flux_app::ReleaseType::Album)
            .expect("album");
        tui.controller
            .set_album_participants(album, &[alpha, beta])
            .expect("participants");
        tui.view = View::Albums;
        tui.detail = Some(Detail::Album(album));
        assert!(tui.view_title().contains("Alpha"));
        assert!(tui.view_title().contains("Beta"));

        tui.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(tui.mode, Mode::AlbumParticipants);
        key(&mut tui, KeyCode::Down);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.detail, Some(Detail::Artist(beta)));
        assert_eq!(tui.mode, Mode::Normal);

        tui.detail = Some(Detail::Album(album));
        tui.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        key(&mut tui, KeyCode::Char('a'));
        assert_eq!(tui.mode, Mode::AlbumParticipantPick);
        let candidates = tui.album_participant_candidates();
        let gamma_index = candidates
            .iter()
            .position(|artist| *artist == gamma)
            .expect("gamma candidate");
        for _ in 0..gamma_index {
            key(&mut tui, KeyCode::Down);
        }
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::AlbumParticipants);
        assert_eq!(
            tui.controller.album_participants(album),
            vec![alpha, beta, gamma]
        );

        tui.album_participant_cursor = 2;
        key(&mut tui, KeyCode::Char('r'));
        assert_eq!(tui.mode, Mode::ConfirmAlbumParticipantRemove);
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::AlbumParticipants);
        assert_eq!(tui.controller.album_participants(album), vec![alpha, beta]);
        assert!(
            tui.controller.album(album).is_some(),
            "participation edits never delete the Album"
        );
        tui.view = View::Artists;
        tui.detail = Some(Detail::Artist(beta));
        assert!(
            tui.view_rows().contains(&Row::Album(album)),
            "an Artist reaches a shared Album through participation alone"
        );
        tui.controller.shutdown();
    }

    #[test]
    fn tui_deletes_an_empty_artist_with_explicit_confirmation() {
        let dir = TempDir::new("tui-delete-empty-artist");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let artist = tui
            .controller
            .create_artist("Temporary Artist")
            .expect("artist");
        tui.controller
            .create_album(artist, "Empty Album", flux_app::ReleaseType::Album)
            .expect("album");
        tui.view = View::Artists;
        tui.detail = Some(Detail::Artist(artist));
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.mode, Mode::ArtistDeletion);
        key(&mut tui, KeyCode::Esc);
        assert!(tui.controller.artist(artist).is_some(), "cancel is safe");
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.mode, Mode::ArtistDeletion);
        key(&mut tui, KeyCode::Char('b'));
        key(&mut tui, KeyCode::Char('y'));
        assert!(tui.controller.artist(artist).is_none());
        assert!(tui.controller.albums().is_empty());
        tui.controller.shutdown();
    }
    #[test]
    fn tui_resolves_populated_artist_deletion_through_shared_decisions() {
        let dir = TempDir::new("tui-populated-artist-delete");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("source/Alpha/Album/01 - Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, wav_with_sample(3)).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let alpha = tui.controller.create_artist("Alpha").expect("alpha");
        let beta = tui.controller.create_artist("Beta").expect("beta");
        tui.controller
            .create_album(beta, "Beta Album", flux_app::ReleaseType::Album)
            .expect("beta album");
        tui.controller
            .import_files(&[source], false)
            .expect("import");
        let track = tui.controller.tracks()[0].0;
        tui.controller
            .update_track_credits(track, vec![alpha, beta])
            .expect("credits");
        tui.view = View::Artists;
        tui.detail = Some(Detail::Artist(alpha));
        key(&mut tui, KeyCode::Delete);
        assert_eq!(tui.mode, Mode::ArtistDeletion);
        key(&mut tui, KeyCode::Char('t'));
        assert_eq!(tui.mode, Mode::ArtistOwnerPick);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ArtistAlbumPick);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ArtistDeletion);
        key(&mut tui, KeyCode::Char('b'));
        assert!(
            tui.artist_deletion
                .as_ref()
                .is_some_and(ArtistDeletionPlan::is_ready),
            "{:?}",
            tui.artist_deletion
                .as_ref()
                .and_then(ArtistDeletionPlan::blocking_reason)
        );
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.mode, Mode::Normal);
        assert!(tui.controller.artist(alpha).is_none());
        assert_eq!(tui.controller.track_artist_id(track), Some(beta));
        assert!(tui.controller.track(track).is_some());
        tui.controller.shutdown();
    }

    #[test]
    fn tui_import_review_applies_fixed_target_and_bulk_release_type() {
        let dir = TempDir::new("tui-import-target");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("source/Alpha/Album/01 - Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, wav_with_sample(4)).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let owner = tui
            .controller
            .create_artist("Target Artist")
            .expect("artist");
        tui.controller
            .create_album(owner, "Target Album", flux_app::ReleaseType::Album)
            .expect("album");
        key(&mut tui, KeyCode::Char('i'));
        type_path(&mut tui, &source);
        wait_for_mode(&mut tui, Mode::ImportReview);
        key(&mut tui, KeyCode::Char('F'));
        assert_eq!(tui.mode, Mode::ImportAlbumPick);
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::ImportReview);
        {
            let plan = tui
                .import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .expect("plan");
            assert_eq!(
                plan.target_album
                    .as_ref()
                    .map(|target| target.title.as_str()),
                Some("Target Album")
            );
            assert_eq!(plan.candidates[0].album(), Some("Target Album"));
        }
        key(&mut tui, KeyCode::Char('2'));
        assert_eq!(
            tui.import_ui
                .as_ref()
                .and_then(|ui| ui.plan.as_ref())
                .expect("plan")
                .candidates[0]
                .release_type(),
            flux_app::ReleaseType::Ep
        );
        key(&mut tui, KeyCode::Esc);
        tui.controller.shutdown();
    }
    #[test]
    fn tui_album_transfer_selects_destination_and_resolves_credits() {
        let dir = TempDir::new("tui-album-transfer");
        let root = dir.path().join("library");
        flux_app::create_canonical_library(&root).expect("canonical");
        let source = dir.path().join("source/Alpha/Album/01 - Song.wav");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        std::fs::write(&source, wav_with_sample(5)).expect("wav");
        let mut tui = tui(&dir);
        tui.controller
            .switch_to_canonical(&root)
            .expect("switch canonical");
        let alpha = tui.controller.create_artist("Alpha").expect("alpha");
        let beta = tui.controller.create_artist("Beta").expect("beta");
        tui.controller
            .import_files(&[source], false)
            .expect("import");
        let album = tui
            .controller
            .tracks()
            .iter()
            .find_map(|(id, _)| tui.controller.track_album_id(*id))
            .expect("album");
        tui.view = View::Albums;
        tui.detail = Some(Detail::Album(album));
        key_mod(&mut tui, KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(tui.mode, Mode::AlbumTransferPick);
        let artists = tui.album_transfer_artists();
        tui.album_transfer_artist_cursor = artists
            .iter()
            .position(|artist| *artist == beta)
            .expect("beta destination");
        key(&mut tui, KeyCode::Enter);
        assert_eq!(tui.mode, Mode::AlbumTransferReview);
        assert!(
            tui.album_transfer
                .as_ref()
                .is_some_and(|plan| !plan.is_ready()),
            "destination credit requires an explicit decision"
        );
        key(&mut tui, KeyCode::Char('c'));
        assert!(
            tui.album_transfer
                .as_ref()
                .is_some_and(AlbumTransferPlan::is_ready)
        );
        key(&mut tui, KeyCode::Char('y'));
        assert_eq!(tui.controller.album_owner(album), Some(beta));
        assert_eq!(
            tui.controller.track_artist_id(tui.controller.tracks()[0].0),
            Some(beta)
        );
        assert_ne!(alpha, beta);
        tui.controller.shutdown();
    }
}
