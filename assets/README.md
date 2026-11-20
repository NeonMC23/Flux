# Flux application identity

These identifiers are part of the release contract. Do not change them between
patch releases; a change requires an explicit migration/identity decision.

| Field | Value |
|---|---|
| Product name | Flux |
| Executable (GUI) | `flux` |
| Executable (TUI) | `flux-tui` |
| Linux application id / desktop file | `io.github.neonmc23.flux` |
| macOS bundle identifier | `io.github.neonmc23.flux` |
| Windows installer identity | `Flux` (stable AppId GUID in the installer script) |
| Version source | workspace `Cargo.toml` (`0.1.0`) |
| Copyright | Copyright (c) 2026 NeonMC23 |
| Project license | MIT |

The canonical icon source is [`flux-icon.svg`](flux-icon.svg). Raster and
platform formats are committed under [`icons/`](icons/):

- `flux-16.png` … `flux-1024.png` for Linux hicolor and documentation;
- `flux.ico` with 16/24/32/48/64/128/256 px images for Windows;
- `flux.icns` with PNG elements from 16 through 1024 px for macOS.

The GUI embeds the 256 px PNG as its window icon. Linux uses the reverse-DNS
app id through winit; the desktop entry has the same file basename and
`StartupWMClass`. macOS uses the `.icns` in `Flux.app` and the bundle identifier
in `Info.plist`. Windows embeds the `.ico` and version resource in `flux.exe`
during a Windows MSVC build.

User data is deliberately outside these identities:

| Platform | Data | Config |
|---|---|---|
| Linux | `$XDG_DATA_HOME/flux` (default `~/.local/share/flux`) | `$XDG_CONFIG_HOME/flux` |
| macOS | `~/Library/Application Support/flux` | same |
| Windows | `%LOCALAPPDATA%\flux` | `%APPDATA%\flux` |

The default canonical library is `<data>/FluxLibrary`. Packaged executables do
not write into their installation directory.
