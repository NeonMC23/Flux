//! Help, diagnostics summaries and long-form terminal guidance.

use flux_app::{AudioStatus, CanonicalMigrationPlan, Controller};

pub(crate) fn help_lines(controller: &Controller, library: &str) -> Vec<String> {
    let audio = match controller.audio_status() {
        AudioStatus::Ready => "ready".to_owned(),
        AudioStatus::DeviceUnavailable(reason) => format!("unavailable — {reason}"),
        AudioStatus::Recovering(reason) => format!("recovering — {reason}"),
        other => other.reason().unwrap_or("unknown").to_owned(),
    };
    let active = controller.active_audio_device().map_or_else(
        || "unknown device".to_owned(),
        |device| {
            format!(
                "{}{}",
                device.name,
                if device.is_default { " (default)" } else { "" }
            )
        },
    );
    let policy = if controller.selected_audio_device().is_some() {
        "explicit selection"
    } else {
        "system default"
    };
    vec![
        "Flux TUI quick reference".to_owned(),
        "A complete keyboard-first player: library, queue, playlists, lyrics, metadata and a real audio visualizer.".to_owned(),
        format!("Active library: {library}"),
        format!(
            "Audio output: {audio} · {active} · {policy} · queue {} · {}",
            controller.queue_len(),
            controller.playback_mode().label()
        ),
        String::new(),
        "NAVIGATION".to_owned(),
        "Tab / Shift+Tab cycles Tracks, Artists, Albums, Playlists, Collections, Lyrics, Settings.".to_owned(),
        "F1–F7 open a destination directly · ↑/↓ or k/j move · PgUp/PgDn · g/G first/last".to_owned(),
        "Enter opens the selected entity or plays the selected track · Esc goes back.".to_owned(),
        "/ searches the current view · Esc clears search · F filters favorites · ? opens help.".to_owned(),
        ": opens the command palette (:tracks :queue :viz bars :layout player :seek +10).".to_owned(),
        String::new(),
        "PLAYBACK".to_owned(),
        "Enter play · Space play/pause · s stop · n/p next/previous · ←/→ seek 5s (Shift 30s).".to_owned(),
        "+/- volume · R cycles repeat/shuffle modes · K crossfade · [ ] crossfade duration.".to_owned(),
        "The player remains visible in every library view; Player layout gives it the whole screen.".to_owned(),
        "Output loss triggers bounded automatic recovery; after exhaustion, Play starts a fresh retry.".to_owned(),
        String::new(),
        "LAYOUT AND VISUALIZER".to_owned(),
        "m cycles Standard, Library, Player, Visualizer and Compact layouts.".to_owned(),
        "v shows/hides the visualizer · Alt+v cycles Waveform, Bars and Pulse.".to_owned(),
        "Alt+↑/↓ changes visualization sensitivity. Preferences persist in the Flux config directory.".to_owned(),
        "The visualizer reads the engine's real post-gain waveform tap; it is not a simulated animation.".to_owned(),
        String::new(),
        "LIBRARY".to_owned(),
        "Artists: Enter opens albums and tracks · Y edits · P creates · Delete reviews deletion.".to_owned(),
        "Populated artist deletion: t chooses a surviving owner and destination album · r removes a credit · d twice explicitly deletes · a auto-resolves safe transfers/credits.".to_owned(),
        "Albums: Enter opens the track listing · Z edits · Ctrl+↑/↓ reorders · Alt+t transfers ownership · Delete removes an empty album.".to_owned(),
        "Tracks: e edits title, artist credit, album and numbering · y reads lyrics.".to_owned(),
        "Alt+m moves a track to an album or standalone state · Delete reviews track deletion.".to_owned(),
        "Album detail: Ctrl+↑/↓ reorders tracks · Delete removes an empty album.".to_owned(),
        "W sets/removes Track artwork · S sets/removes managed lyrics · 0–5 rating · f favorite.".to_owned(),
        "Collections: Favorites, Highly rated, Recently added/played, Most played, lyrics and artwork views.".to_owned(),
        String::new(),
        "PLAYLISTS".to_owned(),
        "N creates · Enter opens · R renames · x deletes after confirmation.".to_owned(),
        "Inside a playlist: Enter plays · l plays all · Alt+s plays shuffled · d removes an entry · Ctrl+↑/↓ reorders.".to_owned(),
        "c clears the playlist after confirmation; canonical tracks are never deleted.".to_owned(),
        "From Tracks: :playlist <name> appends the selected track.".to_owned(),
        String::new(),
        "QUEUE".to_owned(),
        "a adds the selected track · x plays it next · Q opens the first-class queue view.".to_owned(),
        "Enter plays the selected entry · Del/d removes it · Ctrl+↑/↓ reorders.".to_owned(),
        "J jumps to the current entry · f toggles follow · C clears the queue after confirmation.".to_owned(),
        String::new(),
        "IMPORT".to_owned(),
        "o imports a folder · i/I import files · r refreshes the active library.".to_owned(),
        "Import follows Source → Destination → Verify → Import; nothing is copied before review.".to_owned(),
        "Review: Space include/skip · e edits metadata · A/T/L attach album art, track art or lyrics · u updates an exact duplicate · a/s bulk.".to_owned(),
        "F chooses an existing fixed-target album · 1–5 set the new-album release type for all reviewed tracks.".to_owned(),
        "Esc cancels discovery or preparation. The final canonical transaction is never cancelled.".to_owned(),
        String::new(),
        "SETTINGS AND LIBRARY".to_owned(),
        "Lyrics: f toggles playback follow · x exports managed lyrics to a new file.".to_owned(),
        "Settings exposes layout, visualizer, lyrics follow and audio device selection.".to_owned(),
        "Audio devices open off the UI thread; an explicit selection never silently falls back.".to_owned(),
        "O opens a canonical library · C creates one · M migrates an old library.".to_owned(),
        "X refreshes/reconciles · L relinks a unique reported move candidate.".to_owned(),
        String::new(),
        "SAFETY".to_owned(),
        "Missing resources are retained; inaccessible paths are not treated as missing.".to_owned(),
        "Destructive library actions use reviewed shared plans and explicit confirmation; external source audio is never deleted.".to_owned(),
        "Playlist operations validate the reviewed canonical state before writing.".to_owned(),
    ]
}

pub(crate) fn migration_plan_summary(plan: &CanonicalMigrationPlan) -> String {
    if plan.is_ready() {
        format!(
            "Canonical migration: {} track(s), {} artist(s), {} album(s), about {} MiB. \
             y = execute, Esc = cancel (source untouched).",
            plan.track_count(),
            plan.artist_count(),
            plan.album_count(),
            plan.estimated_bytes() / (1024 * 1024),
        )
    } else {
        format!(
            "Canonical migration blocked by {} issue(s): {}. Resolve them before executing.",
            plan.issues().len(),
            plan.issues()
                .iter()
                .take(3)
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// What the user reviews before pressing `y`: what will migrate,
/// what is excluded and why. (IDs/counter preservation and the
/// untouched legacy index/audio are guarantees of the shared
/// migration API.)
/// The reconciliation result in one line, including whether
/// possible new locations were found for missing tracks (never
/// applied automatically).
pub(crate) fn reconcile_summary(report: &flux_app::ReconcileReport) -> String {
    let mut summary = format!(
        "Reconciled {}: {} unchanged, {} changed, {} back, {} missing (kept), {} uninspectable.",
        report.inspected_count(),
        report.unchanged_count() + report.established_count(),
        report.changed_count(),
        report.reappeared_count(),
        report.missing_count(),
        report.inspection_error_count() + report.fingerprint_unavailable_count(),
    );
    let with_candidates = report.unique_candidate_count() + report.ambiguous_candidate_count();
    if with_candidates > 0 {
        summary.push_str(&format!(
            " {with_candidates} missing track(s) have possible new locations ({} ambiguous); nothing moved.",
            report.ambiguous_candidate_count()
        ));
    }
    summary
}

#[cfg(test)]
pub(crate) fn normal_help() -> &'static str {
    "↑/↓ move · Enter open/play · Tab views · m layout · v visualizer · Q queue · ? help · q quit"
}
