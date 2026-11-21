# Terminal captures

These files are reconstructed terminal screens captured from the real
`flux-tui` binary through a PTY at the stated dimensions. ANSI control sequences
are stripped; layout and text are preserved.

- `startup-standard.txt` — Standard layout with library, now playing,
  visualizer and queue panel.
- `track-move.txt`, `track-delete.txt` — reviewed album/standalone move and
  explicit track-deletion confirmation.
- `layout-player.txt`, `layout-visualizer.txt` — Player and full visualization.
- `settings.txt` — settings, audio-device state and preference rows.
- `track-form.txt` — structured track metadata form.
- `help.txt` — scrollable help.
- `80x24-standard.txt`, `80x24-settings.txt` — smaller laptop terminal.
- `40x12-standard.txt`, `40x12-player.txt` — narrow terminal.

The visualization shown here is flat because the validation host build had no
available output stream during capture. The renderer is unchanged when audio is
present; automated tests verify that the canvas changes with real waveform
samples, sensitivity and mode, and remains quiet for silence.
