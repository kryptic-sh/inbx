# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Added

- **PGP / OpenPGP support (M22).** New `inbx-pgp` crate with a `KeySource` trait
  and two backends:
  - `gnupg`: shells out to `gpg --export` / `--sign` / `--decrypt`, preserving
    gpg-agent + pinentry + smartcard / OpenPGP-card support. No private-key
    extraction.
  - `inbx-managed`: pure-Rust crypto via the `pgp` (rpgp) crate; per-account
    Ed25519 keypair under `~/.local/share/inbx/<account>/pgp/` with the
    passphrase in the OS keyring (service `inbx-pgp`). Default selection:
    `gnupg` when `~/.gnupg/` exists, else `inbx-managed`. Account TOML field:
    `pgp.key_source = "gnupg" | "inbx-managed"` plus `pgp.key_fingerprint`. RFC
    3156 PGP/MIME compose (sign / encrypt / signed-then-encrypted), render-side
    verify + decrypt.
- **Composer PGP flags + TUI toggles.**
  `Composer.pgp = PgpFlags { sign, encrypt }`. TUI chord `Ctrl-G S` toggles
  sign, `Ctrl-G E` toggles encrypt; the composer body title shows `[sign]` /
  `[encrypt]` / `[sign+encrypt]` while either is on. Send path branches to
  `to_mime_with_pgp` when set.
- **`inbx pgp` CLI subcommand**: `keygen`, `list`, `export`, `sign`, `verify`,
  `encrypt`, `decrypt`, `lookup-wkd`. `keygen` writes a default `PgpConfig` if
  the account has none; `lookup-wkd` writes the discovered pubkey to
  `<managed_dir>/<fpr>.pub.asc` so the encrypt path picks it up.
- **Autocrypt 1.1 round trip.** Outgoing mail carries an `Autocrypt:` header
  (binary public key, base64, folded at 76 chars) when an inbx-managed pubkey is
  configured. Incoming `Autocrypt:` headers are parsed, the pubkey is harvested
  into `inbx-contacts.pgp_pubkey`, and the composer pre-toggles encrypt + sign
  on reply when the sender already has a stored pubkey (mutual mode).
- **Sender-key lookup for verify.** `inbx-render::render_message_with_pgp` takes
  an `Option<&dyn PubkeyLookup>`; the contact's stored pubkey is used to verify
  signed mail. Falls back to the user's own key with a tag in `pgp_verify.error`
  when no sender pubkey is stored. `ContactsStore` implements `PubkeyLookup` so
  `inbx-render` stays decoupled from the contacts crate.
- **Web Key Directory (WKD) discovery.** Hand-rolled per
  draft-koch-openpgp-webkey-service — no `pgp-lib` dep, only `sha1` and
  `zbase32`. Tries the advanced URL first
  (`https://openpgpkey.<domain>/.well-known/openpgpkey/<domain>/hu/<hash>`) then
  falls back to direct. CLI uses the per-account proxy via
  `inbx-net::proxy::build_reqwest_client`.
- **JWZ message threading (M12 partial).** Replaces the inline parent-walk in
  `Store::set_threading` with Jamie Zawinski's algorithm. New
  `thread_containers` table (migration `0005_jwz_threading.sql`) tracks every
  Message-ID seen — real or referenced — with denormalised `root_id` for O(1)
  thread lookup. Loose-Subject grouping after normalisation (`Re:` / `RE:` /
  `Re[N]:` / `Fwd:` / `Fw:` stripping). Cycle-resistant via 1000-hop walk-up
  with cycle check. `WITH RECURSIVE` migrates `root_id` across moved subtrees.
- **Phishing heuristics on `Rendered`.** Three warning kinds:
  `ReplyToDomainMismatch`, `LookalikeFromDomain` (Levenshtein-1 against a
  built-in well-known domain list — gmail/google/microsoft/outlook/apple/
  paypal/amazon/github/kryptic.sh), `LinkTextHrefMismatch`. Pure string-walk for
  `<a>` scanning; no regex dep.
- **Read-receipt prompt + RFC 8098 MDN compose (M15+).**
  `inbx-render::Rendered.read_receipt_request` surfaces incoming
  `Disposition-Notification-To`. TUI shows
  `[receipt requested — Y to send / N to decline]` in the preview pane; `Y`
  builds an MDN via `inbx_net::build_mdn`
  (`multipart/report; report-type=disposition-notification`,
  `Disposition: manual-action/ MDN-sent-manually; displayed`) and sends. **No
  auto-send path exists.**
- **Per-account SOCKS5 proxy.** `Account.proxy = ProxyConfig { url, username }`.
  Schemes: `socks5://` (local DNS) or `socks5h://` (remote DNS); credentials
  read from the OS keyring on demand. Wired into IMAP, Sieve, Graph, JMAP, OAuth
  login + refresh. SMTP gracefully degrades with a `tracing::warn!` (lettre 0.11
  has no SOCKS hook — proxychains is the workaround).
- **TUI hjkl-picker overlays (M25).** `<Space>f` folder, `<Space>b` account
  switch, `<Space>m` message-jump, `<Space>a` attachment save. `StashedSource`
  shim adapts the editor-flavoured `PickerAction` to inbx-shaped selections
  (Arc<Mutex<Option<T>>> stash; `select` returns `PickerAction::None`).
- **TUI account-add wizard (M26 part 1).** `<Space>n` opens a 10-field
  `hjkl_form::Form` (name / email / IMAP host:port:security / SMTP
  host:port:security / username / password). Autoconfig prefills IMAP + SMTP
  defaults from `inbx_config::autoconfig::suggest()` after the email field
  blurs. `<Space>s` saves; `<Esc>` cancels. CLI `cmd_accounts_add` unchanged.
- **TUI Sieve-edit wizard (M26 part 2).** `<Space>S` connects to ManageSieve,
  lists scripts, opens a picker. Selecting a script GETs the body and opens an
  `hjkl_form::Form` with a single-line name field plus a 10-row `MultiLineText`
  body field. `<Space>s` PUTs back; on failure the wizard restores so the user
  can retry.
- **hjkl-clipboard for yank / put + attachment paste.** TUI `<Space>y` /
  `<Space>p` chords copy or replace the focused editor's text via the system
  clipboard with OSC 52 fallback for SSH. `Composer::attach_from_ clipboard()`
  accepts pasted PNG (preferred) or text (fallback).
- **hjkl-ratatui spinner during async ops.** Status line prefixes a spinner
  glyph while `app.busy`. Subsequent refactor (see Changed) made the spinner
  actually animate during long ops.
- **`docs/threat-model.md`.** Documents inbx assets, threats covered (lost
  device with / without FDE, multi-user system, network attacker), and deferred
  work (per-account at-rest encryption, plausible deniability, memory
  scrubbing). Linked from PLAN.md.
- **`CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`** at repo root.
  Adapted from sibling hjkl. Disclosure email: `mxaddict@kryptic.sh`.
- **Performance harness (criterion).** `crates/inbx-store/benches/store.rs`
  measures the four PLAN.md hot-path budgets: cold-start proxy (<200 ms), folder
  switch (<50 ms), local search FTS5 (<100 ms / 100k msgs), JWZ threader scaling
  (reference). 100k seed runs once via OnceLock; iterations clone the cheap
  `Store` handle. Baseline: cold-start 0.6 ms · folder switch 0.9 ms · search 39
  ms — all PASS. `docs/perf-budgets.md` records numbers + how to run.

### Changed

- **TUI event loop refactor.** Long ops (manual sync, body fetch, outbox drain,
  Sieve list / get / put) now spawn onto background tokio tasks and post results
  back via an mpsc channel. Event loop `tokio::select!`s over key events / task
  results / 120 ms tick (only while busy). Fixes a latent issue where the
  Phase-6 spinner only animated _between_ key handlers, never _during_ them —
  the 5-second IMAP fetch left the UI frozen. Concurrency policy:
  refuse-when-busy (`spawn_pending` returns false + status "busy — wait for
  current op"). Multi-op concurrency is a follow-up.
- **Composer headers backed by `hjkl-form`.** Subject / To / Cc / Bcc are now
  fields on a single `hjkl_form::Form` (`Composer.headers`); the body remains a
  standalone `Editor`. New `FocusedEditor<'_>` enum wraps either a
  `TextFieldEditor` (header) or `Editor` (body); callers match on the variant.
  Public getters `subject()/to()/cc()/bcc()` and setters `set_subject()/...`
  replace the four old `pub Editor` fields. `header_cursor(field)` for the
  render layer.
- **hjkl-\* deps on caret-minor pins (`"0.3"`).** Dropped the lockstep `=0.0.x`
  regime; each hjkl crate now versions independently. Workspace block:
  `hjkl-editor`/`-buffer`/`-engine` `"0.3"`, `hjkl-form` / `-picker` /
  `-ratatui` `"0.3"`, `hjkl-clipboard` `"0.5"`, `hjkl-config` `"0.2"`. Workspace
  `crossterm` bumped 0.28 → 0.29 to match what `hjkl-engine 0.3.3` pulls.
- **`ContactsStore` cached on `App`.** Replies no longer re-open the sqlite
  contacts DB on every keystroke. Lazy lookup; failed open is sticky.
- **Path resolution helpers cleanup.** Continued migration off the empty
  `inbx-core` crate (see Removed).

### Removed

- **Empty `inbx-core` crate.** Held a single 11-line `Error` enum that was never
  imported. Domain types live in `inbx-config` (`Account`, `AuthMethod`) and
  `inbx-store` (`FolderRow`, `MessageRow`, `OutboxRow`) and that's working.
  Workspace member entry, path-dep, and 10 `inbx-core.workspace = true`
  references in member manifests dropped. PLAN.md captures the decision
  rationale; if a unifying domain layer becomes useful later, reintroduce it
  then.

### Notes

- Public API of `Composer` and `Rendered` gained new fields. Anyone constructing
  them with struct literals will break. Next release MUST bump to `0.2.0` (minor
  pre-1.0 = breaking on 0.x).

## [0.1.2] - 2026-05-03

### Fixed

- Release workflow now adds the matrix target std explicitly via
  `rustup target add` after the `dtolnay/rust-toolchain` step. The action's
  `targets:` input was not actually adding `x86_64-apple-darwin` std on the
  arm64 macOS runner — `rustup toolchain install` saw the toolchain as
  already-installed and skipped the target. The Intel-mac binary failed to build
  in 0.1.0 and 0.1.1 as a result.

## [0.1.1] - 2026-05-03

### Fixed

- Release workflow now skips the `cargo fmt --check` and `cargo clippy` steps in
  the build matrix — those run in CI on every push to main. The redundant Clippy
  step was failing on `x86_64-apple-darwin` (target std issue), which prevented
  the Intel-mac binary from being published in 0.1.0. Aligns with the canonical
  sqeel / hjkl release.yml pattern (build-only).

## [0.1.0] - 2026-05-03

### Changed

- **Path resolution migrated to `hjkl-config` 0.2 (XDG-everywhere).**
  `inbx_config::config_path()` / `config_dir()` / `data_dir()` now route through
  `hjkl_config::config_dir("inbx")` / `data_dir("inbx")` instead of
  `directories::ProjectDirs::from("sh", "kryptic", "inbx")`. macOS users move
  from `~/Library/Application Support/sh.kryptic.inbx/` and
  `~/Library/Preferences/sh.kryptic.inbx/` to `~/.config/inbx/` +
  `~/.local/share/inbx/`. Windows users move from `%APPDATA%\kryptic\inbx\` to
  `~/.config/inbx/` + `~/.local/share/inbx/`. Linux paths unchanged. Replaced
  `directories` workspace dep with `hjkl-config = "0.2"`.
- `pub fn project_dirs()` removed from the `inbx-config` API; replaced with
  `pub fn config_dir()`. The `Error::NoXdg` variant now wraps
  `hjkl_config::ConfigError` (was a unit variant).

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

[Unreleased]: https://github.com/kryptic-sh/inbx/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.0
