# Development tools

## `tui_pty_capture.py`

Reconstructs terminal screens from a real `flux-tui` process through a PTY. It
requires Python 3 and `pexpect`, and runs entirely locally.

```bash
cargo build -p flux-tui --locked
XDG_DATA_HOME=/path/to/disposable-data \
python3 tools/tui_pty_capture.py target/debug/flux-tui 36 120 /tmp/tui-captures
```

Captures cover startup, layout presets, Settings, an edit form and help. The
script strips ANSI control sequences into plain terminal screens; it is a
runtime inspection tool, not a replacement for cargo tests or human usability
validation. Representative outputs are checked in under
`docs/evidence/m16/`.
