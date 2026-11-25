# Vendored Lucide icons

This directory contains the curated subset of [Lucide](https://lucide.dev) icons
used by the Flux GUI. Lucide is licensed under the ISC License; see `LICENSE`.

- Upstream repository: <https://github.com/lucide-icons/lucide>
- Pinned commit: see `COMMIT`
- Assets: SVG source files, 24×24 view box
- Rendering: compiled into `flux` with `include_bytes!` and rendered by Iced's
  SVG backend. No network access at build or runtime.

Only icons actually used by the interface are vendored. Do not add an icon
without also adding a visible label or an accessible tooltip at the control
boundary.
