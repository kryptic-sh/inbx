# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Added

- `inbx --help` now renders an ASCII-art banner (figlet "ANSI Regular" font)
  with the package version inline. Banner lives in `apps/inbx/src/art.txt`,
  embedded via `include_str!`. Regenerate with
  `figlet -f "ANSI Regular" inbx > apps/inbx/src/art.txt`.
- CLI smoke tests: `--version` returns `CARGO_PKG_VERSION`, long-form help
  contains the embedded art block and the version string.

### Changed

- bump hjkl =0.0.39 — adopts Query::dirty_gen; consumer-side change is pin bump
  only. Composer migration to spec::\* still pending Editor<B,H> generic at
  0.1.0.
- bump hjkl =0.0.38 — adopts FoldOp / FoldProvider::apply pipeline;
  consumer-side change is pin bump only. Composer migration to spec::\* still
  pending Editor<B,H> generic at 0.1.0.
- bump hjkl =0.0.37 — adopts spans + search-pattern relocation out of Buffer;
  consumer-side change is pin bump only. Composer migration to spec::\* still
  pending Editor<B,H> generic at 0.1.0.
- bump hjkl =0.0.36 — adopts named-marks consolidation; consumer-side change is
  pin bump only. Composer migration to spec::\* still pending Editor<B,H>
  generic at 0.1.0.
- bump hjkl =0.0.35 — adopts search FSM migration into `hjkl_editor::Editor`;
  consumer-side change is pin bump only. Composer migration to spec::\* still
  pending Editor<B,H> generic at 0.1.0.
- bump hjkl =0.0.34 — adopts Patch C-δ.1 (viewport relocated from Buffer to
  Host); consumer-side change is pin bump only — inbx uses
  `hjkl_editor::runtime::Editor` with `DefaultHost`, no direct
  `buffer.viewport()` reaches. Composer migration to spec::\* still pending
  Editor<B,H> generic at 0.1.0.
- bump hjkl =0.0.33 — adopts Patch C-γ partial; consumer-side change is pin bump
  only. Composer migration to spec::\* still pending Editor<B,H> generic at
  0.1.0.
- bump hjkl =0.0.32 — adopts Patch C-β partial (breaking renames for rect-scoped
  mouse/cursor helpers and ratatui-flavored syntax/style interners, plus new
  `FoldProvider` trait); consumer-side change is pin bump only — inbx has no
  call sites affected. Composer migration to spec::\* still pending Editor<B,H>
  generic at 0.1.0.
- bump hjkl =0.0.30 — adopts Patch C-α (motion vocabulary relocated from
  `hjkl_buffer::Buffer` inherent methods into `hjkl_engine::motions` module);
  consumer-side change is pin bump only. Composer migration to spec::\* still
  pending Editor<B,H> generic at 0.1.0.
- TUI search overlay (`/`) now persists query + results across closes; reopening
  `/` resumes the prior session. Adds `n` / `N` from the main list to jump to
  the next / previous match without reopening the overlay, and shows a `[m/n]`
  match counter in the overlay header. Stash `stash@{0}` (TUI ? help overlay
  refactor) was left untouched: it is stale relative to the post-split TUI
  module layout (af2db79) and conflicts with current `tui/` modules.
- TUI status line now surfaces modal state (`NORMAL` / `INSERT` / `SEARCH` /
  `VISUAL`), the active account + focused folder, an unread count for the loaded
  folder, and a relative "synced Ns ago" age driven by `F` manual sync.
  Transient messages (`app.status`) trail the structured prefix.

### Added

- Workspace skeleton mirroring buffr layout: `crates/` (inbx-core, inbx-net,
  inbx-store, inbx-config, inbx-render, inbx-contacts, inbx-ical, inbx-composer)
  plus `apps/` (inbx TUI, inbx-gui) and `xtask/`.
- Multi-provider mail networking surface in `inbx-net`: generic IMAP+SMTP, Gmail
  XOAUTH2, Microsoft 365 OAuth2 device-code + auth-code flows, groundwork for
  JMAP and Microsoft Graph backends behind a `MailProvider` trait.
- Local store: Maildir-on-disk + SQLite index via sqlx, plus search hooks.
- HTML render pipeline (`inbx-render`) using ammonia + html2text for safe
  text-mode display.
- Composer crate (`inbx-composer`) embedding hjkl-editor (pinned `=0.0.15`) for
  modal message editing.
- Calendar (`inbx-ical`) and contacts (`inbx-contacts`) crates.
- Config crate with `directories` + `keyring` for secret storage.
- Release tooling: release-plz workspace config, Keep a Changelog format, GitHub
  Actions release-plz workflow (publish gated off until first dry-run pass
  clears).
