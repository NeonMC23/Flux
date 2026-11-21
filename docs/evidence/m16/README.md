# M16 terminal captures

These files are reconstructed terminal screens, captured from the real
`flux-tui` binary through a PTY at the stated dimensions. They contain no ANSI
control sequences; layout and text are preserved.

- `120x36-standard.txt` — Standard layout with library, now playing,
  visualizer and queue panel.
- `120x36-player.txt` — Player layout.
- `120x36-visualizer.txt` — full-area visualizer layout.
- `120x36-settings.txt` — settings, audio-device state and preference rows.
- `120x36-track-form.txt` — structured track metadata form.
- `120x36-help.txt` — scrollable help.
- `80x24-standard.txt`, `80x24-settings.txt` — smaller laptop terminal.
- `40x12-standard.txt`, `40x12-player.txt` — narrow terminal.

The visualization shown in these captures is flat because the validation host
build had no available audio output during capture. The rendering path is
identical when audio is present; automated tests verify that the canvas changes
with real waveform samples, sensitivity and mode and remains quiet for silence.
