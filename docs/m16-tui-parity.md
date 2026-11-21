# M16 TUI feature parity and interaction model

This document records the M16 audit of the GUI feature set, the TUI equivalent,
the shared `flux-app::Controller` boundary, and regression coverage. It is a
working product document, not a claim of human usability validation.

## Design direction

The terminal interface is a first-class Flux frontend. It uses the same
canonical library, playback engine, queue, history, ratings, playlists, import
pipeline, lyrics and artwork records as the GUI. `flux-tui` contains no parallel
persistence or domain rules.

The visual hierarchy is:

1. Library destinations: a persistent sidebar on wide terminals, otherwise
   compact tabs.
2. Transport status and contextual title for the current view/detail.
3. Main content, with an optional now-playing/visualizer/up-next side panel.
4. One primary playback timeline.
5. Status and contextual help.

M21 added the persistent sidebar and removed duplicate progress displays from
the header, side panel and player page.

Five layout presets replace arbitrary panel configuration:

- **Standard** — library plus now-playing/visualizer/up-next panel.
- **Library** — library only.
- **Player** — large now playing, visualizer and up-next.
- **Visualizer** — full-area visualization.
- **Compact** — library and playback summary, no tabs or side panel.

`Tab`/`Shift+Tab` cycle destinations, `F1`–`F7` select one directly, `m` cycles
layouts, and `v`/`Alt+v` control the visualizer. UI preferences are stored in
`tui.conf` under the Flux config directory. User data never moves there.

The TUI is split into focused modules: `main.rs` owns application state,
dispatch and lifecycle; `render.rs` composes views; `import.rs` owns the import
worker and review; `forms.rs` owns structured form parsing; `help.rs` owns help
text and summaries; `text.rs` owns width-aware helpers; and `visualizer.rs`
renders real waveform data. Domain rules remain in `flux-app`.

## Parity matrix

| Capability | Pre-M16 TUI | M16 TUI equivalent | Shared API / boundary | Status | Tests |
|---|---|---|---|---|---|
| Browse tracks | Flat list | Tracks destination, search, favorites, collection filter | `Controller::search`, `tracks` | Complete | Existing TUI tests, large-list test |
| Browse artists | Info panel only | Artists list and artist detail with albums and tracks | `artists`, `artist`, `albums_owned_by`, `albums_credited_for`, credits | Complete | `tui_navigation_opens_artists_albums_playlists_and_collections` |
| Browse albums | Info panel only | Albums list and album track listing | `albums`, `album`, `album_tracks` | Complete | Same navigation test |
| Browse playlists | Key-only info | Playlists destination and playlist detail | `playlists`, `playlist` | Complete | Navigation and playlist tests |
| Smart collections | Not exposed | Collections destination with counts | `Controller::smart_collection_tracks` | Complete | `smart_collections_share_rating_lyrics_and_artwork_state` |
| Search | Tracks only | Current-view search | `search`, `search_artists`, `search_albums` and local name filtering over shared entities | Complete | Existing search tests |
| Empty/loading states | Basic | Per-view empty messages and audio-device loading state | Frontend presentation | Complete | Existing empty-state tests |
| Missing resources | Badges | Track badges in lists and queue | `track_audio_resource_state` | Complete | Existing resource tests |
| Artist create/edit | Delimiter form | Structured Name/Aliases/Description/Image form; stable ID preserved | `create_artist`, `update_artist_metadata`, `set/remove_artist_image` | Complete | Artist workflow test |
| Artist image status | Basic | Missing/inaccessible details remain in resource APIs and editor | `artist_image_resource_state` | Partial presentation; no destructive repair action | Existing app tests |
| Album edit | Delimiter form | Structured Title/Release type/Cover form | `update_album_metadata`, `set/remove_album_cover` | Complete | Album form tests |
| Album track order/membership | GUI-only | `Ctrl+↑/↓` reorders; `Alt+m` moves to an album or standalone | `reorder_album_track`, `plan/execute_track_move` | Complete | `tui_completes_album_order_membership_and_safe_deletion` |
| Album delete/transfer | GUI workflow | Empty albums delete with confirmation; `Alt+t` reviews album ownership transfer with explicit artist and credit decisions | `delete_empty_album`, `plan/execute_album_transfer` | Complete | Album transfer test |
| Track title/artist credit/album/numbering | Delimiter form | Structured prefilled form | `edit_track_metadata`, `create_artist` | Complete | `import_edit_and_artist_keys_use_the_shared_controller` |
| Track credits/ownership | Read-only info | Structured common fields; `Alt+e` advanced credit/owner form; ownership transfer remains GUI-only | `edit_track_metadata`, transfer plans | Limit: ownership transfer | Existing app tests |
| Track artwork | Path form | `W` managed override set/remove from a path; `-` removes | `set/remove_track_artwork`, resource state | Complete | Artwork workflow test |
| Lyrics view | Read-only | Lyrics destination, dedicated reader, follow-playback | `track_lyrics`, `LyricsDocument` | Complete | Existing lyrics tests |
| Lyrics edit/import/remove | Path form | `S` attaches/replaces/removes managed UTF-8/LRC text; `-` removes | `set_lyrics` | Complete | Lyrics tests |
| Lyrics export | GUI-only | `x` in the lyrics reader writes to a new path; existing files are never overwritten | `track_lyrics`, local write | Complete | `tui_exports_lyrics_and_attaches_import_assets` |
| Favorites and ratings | Complete | Unchanged keys, shared collection views | `set_track_rating`, `toggle_track_favorite`, `smart_collection_tracks` | Complete | Existing rating tests |
| History views | Summary message | Recently played / Most played collections | `playback_history` | Complete | Smart collection test |
| Folder/file import | Delimiter/path input | Source → Destination → Verify → Import, same pipeline | `prepare_import_plan`, `prepare_import_files` | Complete for normal path | Existing import lifecycle tests |
| Import metadata editing | None | `e` opens a structured form for title/artist/album/numbers | `ImportCandidate` setters | Complete | Import edit test |
| Import artwork/lyrics attachment | GUI-only | `A`/`T`/`L` attach album art, track art or lyrics; `-` clears | `ImportCandidate` asset APIs, `apply_lyrics_from_path` | Complete (typed paths, no native picker) | Import asset test |
| Import destination | Active library | Explicit review of active canonical destination; `F` applies an existing fixed-target album | Shared import request and `ImportAlbumTarget` | Complete | Fixed-target test |
| Bulk import release type | GUI-only | `1`–`5` apply Album/EP/Single/Compilation/Other to reviewed new albums | `ImportPlan::apply_release_type_to` | Complete | Fixed-target/bulk test |
| Duplicate decisions | Complete | Include/skip/update and bulk actions | `ImportDecision`, plan bulk helpers | Complete | Duplicate import tests |
| Import cancellation | Complete | Esc cancels discovery/preparation; final commit honest | Shared worker/cancel flag | Complete | Cancellation tests |
| Create/rename/delete playlist | None | `N`, `R`, `x` with confirmation | `create/rename/delete_playlist` | Complete | Navigation and confirmation tests |
| Add/remove/reorder playlist entries | None | `:playlist <name>`, `d`, `Ctrl+↑/↓` | `add_tracks_to_playlist`, `remove_playlist_entries`, `reorder_playlist_entry` | Complete | Queue/playlist tests |
| Play/shuffle playlist | None | `l` plays; `R` cycles shuffle modes; command palette available | `play_playlist`, `play_playlist_shuffled` | Complete | Existing app tests |
| Queue view/reorder/remove | Complete | First-class queue view, follow, jump, clear | `queue_entries`, `move_queue_entry_by`, `remove_queue_entry`, `clear_queue` | Complete | `queue_clear_requires_confirmation...` |
| Queue clear | None | `C` plus explicit confirmation | `Controller::clear_queue` | Complete | App and TUI tests |
| Play/pause/stop/next/previous | Complete | Unchanged | Engine/controller | Complete | Existing tests |
| Seek | Not exposed | `←`/`→` 5 s, Shift 30 s, `:seek` | `Controller::seek` | Complete | App seek tests |
| Volume | Complete | `+`/`-` and `:vol` | `set_volume` | Complete | Existing tests |
| Repeat/shuffle | Repeat only | `R` cycles full playback modes | `cycle_playback_mode` | Complete | Existing tests |
| Audio device selection | GUI-only | Settings lists devices, default/active markers, asynchronous open | `enumerate_audio_output_devices`, `open_audio_output`, `install_audio_output` | Complete when built with CPAL | Existing GUI/audio tests; TUI list smoke |
| Crossfade | Complete | Unchanged | Shared settings | Complete | Existing tests |
| Lyrics follow playback | None | Reader follows synchronized timestamps; manual scroll pauses follow | `position`, lyrics timestamps | Complete | Lyrics tests |
| Oscilloscope | GUI only | Standard/Player/Visualizer layouts, Waveform/Bars/Pulse | `Controller::waveform`, shared `WaveformScope` | Complete | `visualization_is_derived_from_real_waveform_samples` |
| Visualizer visibility/mode/sensitivity | None | `v`, `Alt+v`, `Alt+↑/↓`; hidden means no waveform sampling | `WaveformScope`, shared waveform tap | Complete | Visualization and preference tests |
| Layout presets | None | `m` cycles five presets; persisted | TUI preference file | Complete | Preferences test |
| Help and discovery | Large static help | Contextual footer, `?` help, command palette | Frontend presentation | Complete | Help test |
| Library open/create/migrate | Complete | Preserved | Shared lifecycle APIs | Complete | Existing migration tests |
| Track/Artist/Album deletion | GUI review | Track deletion, empty albums and populated artist deletion use shared plans and explicit confirmation; artist transfer chooses owner and destination album interactively | Shared deletion/transfer plans | Complete | Album/artist deletion tests |
| Track audio replacement | GUI review | `Alt+r` plans a replacement and confirms with `y` | `plan/execute_track_audio_replacement` | Complete | `tui_reviews_managed_audio_replacement...` |

## Genuine limitations

- Terminal cells cannot display embedded artwork. Track/Album artwork is
  represented by presence badges and state messages, never by a fake image.
- Populated artist deletion is available through the shared decision plan
  (`t` transfer, `r` remove credit, `d` twice for explicit deletion, `a`
  auto-resolve, `y` publish). Ownership/album transfer remains GUI-only because
  it needs a destination-review UI that the terminal does not approximate well.
- Import asset attachment uses typed paths in the TUI; native graphical pickers
  remain GUI-only. The ordinary Source → Destination → Verify → Import path and
  structured metadata correction are available in both frontends.
- The renderer is hand-written; no mouse support is required or provided.
- Keybindings are fixed and conflict-tested rather than user-configurable. The
  command palette covers less frequent actions without creating a second
  configuration surface.
- Native runtime audio and human usability are separate from automated tests.

## Keybindings

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next/previous destination |
| `F1`–`F7` | Tracks, Artists, Albums, Playlists, Collections, Lyrics, Settings |
| `↑`/`↓`, `k`/`j`, PgUp/PgDn, `g`/`G` | Move/scroll |
| `Enter` | Open selected entity / play selected track |
| `Esc` | Back, clear search, or cancel |
| `/` | Search current view |
| `F` | Favorites-only track filter |
| `f` | Toggle favorite (track rows) |
| `0`–`5` | Clear/set rating |
| `e` | Structured track edit |
| `Alt+m` | Move selected track to an album or standalone |
| `Alt+r` | Review managed-audio replacement or missing-file repair |
| `Alt+t` in album detail | Review album ownership transfer |
| `F` in import | Choose a fixed target album |
| `1`–`5` in import | Bulk Album/EP/Single/Compilation/Other release type |
| `Delete` | Review track, empty-album or populated artist deletion |
| `Y` / `Z` | Structured artist / album edit |
| `W` / `S` | Track artwork / managed lyrics |
| `y` | Read selected/current lyrics |
| `x` in lyrics | Export lyrics to a new path |
| `A`/`T`/`L` in import | Attach album artwork / track artwork / lyrics |
| `t`/`r`/`d`/`a` in artist deletion | Choose owner/album / remove credit / explicit delete twice / auto-resolve |
| `N` / `R` / `x` | New / rename / delete playlist (context) |
| `l` / `d` / `c` | Play playlist / remove entry / clear playlist (detail context) |
| `Ctrl+↑`/`Ctrl+↓` | Reorder queue entry or playlist entry |
| `Q` | Queue view |
| `C` | Clear queue (confirmation) |
| `J` / `f` | Jump to current / toggle queue follow |
| `Space` / `s` / `n` / `p` | Play-pause / stop / next / previous |
| `←`/`→` | Seek 5 s (Shift: 30 s) |
| `+`/`-` | Volume |
| `R` | Cycle repeat/shuffle modes outside playlist context |
| `K`, `[`, `]` | Crossfade state and duration |
| `m` | Cycle layout |
| `v` / `Alt+v` | Show-hide / cycle visualizer mode |
| `Alt+↑`/`Alt+↓` | Visualizer sensitivity |
| `:` | Command palette |
| `?` | Full help |
| `q` / `Ctrl-C` | Quit |

## Validation boundaries

Automated tests verify shared data flow, form validation, view navigation,
queue confirmation, playlist state, preference persistence, visualization
mapping and large-library performance. Real PTY captures at 120×36, 80×24 and
40×12 verify rendering and truncation. These do not constitute human usability
testing.
