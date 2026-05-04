# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Added

- **CalDAV pull (M24).** New `inbx-ical::caldav` module mirrors the CardDAV
  shape: `discover` walks RFC 6764 (`current-user-principal` →
  `calendar-home-set` → depth-1 PROPFIND for `<calendar/>` resourcetype); `sync`
  issues a `calendar-query` REPORT filtered for `VEVENT` and writes each event
  as `<uid>.ics` under `<data_dir>/inbx/<account>/calendar/`. CLI:
  `inbx cal caldav pull|discover` (parallel to `inbx contacts carddav`).
- **`MailProvider::create_folder` / `delete_folder` / `rename_folder` /
  `subscribe_folder`.** Four new trait methods covering folder management.
  `ImapProvider` delegates to existing `imap::create_folder` etc. free
  functions. `JmapProvider` uses `Mailbox/set` (RFC 8621 §2.5) with `create`,
  `destroy`, and `update` actions. `GraphClient` uses `/me/mailFolders` REST
  (`POST`, `DELETE`, `PATCH`); `subscribe_folder` is a no-op with a
  `tracing::debug!` (Graph has no subscription concept). `MockProvider` stubs
  added to keep the dyn-compat test compiling.
- **`MailProvider::fetch_bodies` bulk body fetch.** Default impl loops
  `fetch_body` one at a time (JMAP, Graph). `ImapProvider` overrides with a
  single `UID FETCH` for all UIDs in one round-trip, restoring the bulk-fetch
  performance the IMAP path had before the unified trait landed.
- **JMAP push in the TUI.** `App::new` spawns a long-lived `do_watch` task that
  dispatches on `account.transport`: IMAP IDLE (RFC 2177), JMAP EventSource (RFC
  8620), or a Graph poll placeholder. Push events post
  `TaskResult::WatchSignal`, which the event loop turns into a `manual_sync` on
  the current folder. Fastmail / Stalwart accounts now get live TUI updates like
  IMAP IDLE accounts. `inbx-sync` shares the same dispatch via an extracted
  `wait_for_change` helper.
- **`MailProvider::expunge_folder` for JMAP and Graph.** TUI expunge now routes
  through `connect_provider` like the other hot-path ops. JMAP runs
  `Email/query` filtered by `inMailbox + $deleted` then `Email/set { destroy }`
  and reports the server's actual destroyed count. Graph is a no-op (returns 0
  with a `tracing::debug!`) since Graph has no per-message deletion flag —
  "delete" in Graph means move-to-DeletedItems.
- **Graph delta polling in the watch path.** Replaces the 5-min sleep
  placeholder with `delta_messages(folder_id, stored_link)` against
  `/me/mailFolders/{id}/messages/delta`. Loads the persisted `delta_link` from
  Store, fetches changes, persists the new link, signals only when messages came
  back, sleeps 75 s between polls. Same shape in both `tui::do_watch` and
  `inbx-sync::wait_for_change`. Errors back off 30 s like the other transports.
  Graph accounts now get near-realtime TUI updates, finishing the JMAP/Graph
  parity sweep (push + expunge + delta).

### Changed

- **TUI watch follows the active folder.** The background push task (IMAP IDLE /
  JMAP EventSource / Graph delta poll) now rebinds whenever the user navigates
  to a different folder — previously it stayed bound to the boot folder, so push
  silently went dark on the new view. `reload_messages` is the chokepoint;
  same-folder reloads are a no-op.
- **`inbx folder` CLI routes through `MailProvider`.** `cmd_folder` previously
  called `connect_imap` directly, breaking on JMAP and Graph accounts. Now uses
  `connect_provider` for all four sub-commands (create / delete / rename /
  subscribe), so the correct backend is invoked automatically.
- **`cmd_fetch` uses bulk `fetch_bodies`.** The per-UID body loop in `cmd_fetch`
  is replaced with a single `provider.fetch_bodies(&pending)` call. IMAP gets
  one `UID FETCH` for all pending UIDs (restores pre-unification bulk perf);
  JMAP and Graph fall back to the default sequential loop with no user-visible
  change.
- **`inbx expunge` CLI routes through `MailProvider`.** Previously called
  `connect_imap` + raw `expunge_folder`, which errored on JMAP / Graph accounts.
  Now uses `connect_provider` like the TUI, so JMAP runs the `Email/set destroy`
  path and Graph reports the no-op.
- **`inbx watch` CLI dispatches on transport.** Graph accounts now run a
  `delta_messages` poll cycle (75 s sleep on no-change, immediate fetch on
  changes) via the new `graph_delta_tick` helper, matching `inbx-sync` and
  `tui::do_watch`. JMAP still forwards to `inbx jmap push`.
- **`inbx draft save` / `mark` / `flag` / `mv` route through `MailProvider`.**
  Four CLI ops ported from raw IMAP to `connect_provider` so JMAP and Graph
  accounts work without erroring. `mark`'s verb → flag mapping was restructured
  to produce `add`/`remove` slices directly. `inbx cp` keeps the raw IMAP
  `UID COPY` path and bails on JMAP / Graph with a clear message —
  `MailProvider` has no copy method (deferred).
- **`inbx fetch` / `inbx fetch --all` unified over `MailProvider`.** Drops the
  special-case forwarding to `cmd_jmap(Fetch)` / `cmd_graph(Fetch)` — the same
  `provider.list_folders` + `provider.fetch_headers` + `provider.fetch_body`
  flow now drives all three transports. `--since N` (days-ago) keeps its IMAP
  `UID SEARCH SINCE` fast path (`MailProvider` has no days-ago filter) and
  warns + falls through on JMAP / Graph. Body fetch goes through
  `provider.fetch_bodies` which IMAP overrides for bulk `UID FETCH` (see Added).

## [0.2.0] - 2026-05-04

### Added

- **JMAP backend (M21).** New `MailProvider` async trait in `inbx-net::provider`
  abstracts the hot-path mail operations (list folders, fetch headers / body,
  set flags, move, send, append draft) over IMAP and JMAP. `JmapClient`
  implements the trait via `Email/query` + `Email/get` + `Email/set` +
  `Email/import` + `Blob/download`. Per-account opt-in via
  `[transport] kind = "jmap"`; existing IMAP-only accounts unchanged. TUI hot
  path (sync, body fetch, flag toggle, move, draft append) routed through
  `connect_provider` so JMAP accounts skip the IMAP path automatically.
- **MS Graph backend (M11).** Third backend over `MailProvider` — `GraphClient`
  implements list_folders / fetch_headers / fetch_body / set_flags (`isRead`,
  `flag.flagStatus`) / move (`/me/messages/{id}/move`) / send (`/me/sendMail`) /
  append_draft. `well_known` mailbox roles map to IMAP special-use. Opt-in via
  `[transport] kind = "graph"`.
- **`provider_id` column on `messages`.** Eliminates the 500-row scan in
  `resolve_jmap_id` / `resolve_graph_id` — backends stash the opaque provider id
  directly. New migration `0006_provider_id.sql` with a partial index (only IS
  NOT NULL rows indexed, IMAP rows pay nothing). `Store::provider_id_for`
  helper; `connect_provider` gains `store: Option<&Store>` so clients carry a
  Store handle for the fast lookup, with a slow-path scan fallback for
  pre-migration rows (logged via `tracing::debug!`).
- **mbox / .eml import-export hardening (M19).** Extracted helpers into
  `apps/inbx/src/mbox.rs`. Real RFC 4155 asctime date formatter (Hinnant
  civil-from-days, no chrono dep) replaces the broken stub that always
  emitted 1970. Bidirectional `Status:` / `X-Status:` ↔ IMAP flag translation
  (R=Seen, F=Flagged, A=Answered, D=Deleted, T=Draft per mutt convention) so
  flags survive a round-trip. `inbx export --eml --uid N` exports a single
  message as raw RFC 5322. `--since` / `--limit` filters on bulk export. Import
  derives flags from headers when present, no longer hardcodes `\Seen`.
- **TUI data-driven key dispatch + auto-rendered help overlay.** New
  `tui::binds` module owns `Action` (~70 variants), `BINDS` static table (action
  / key / desc / category / contexts / gate), `Action::from_key` /
  `Action::invoke` / `Action::all_rows_in`. `handle_list_key` shrank from ~290
  to ~100 lines; pane / receipt / preview gates moved from inline `if` blocks
  into `gate: Option<fn(&App) -> bool>` on each row. Help overlay walks the same
  table — no more hardcoded help text. As a side effect, `N` in Preview now
  actually declines a pending read receipt instead of silently stepping search
  backward (latent bug).
- **Autocrypt 1.1 §4 mutual-mode end-to-end.** New
  `AutocryptPreference { Nopreference, Mutual }` enum. Outbound `Autocrypt:`
  header emits `prefer-encrypt=mutual` when the local
  `PgpConfig.prefer_encrypt_mutual` is set (default true). Incoming parse
  exposes `prefer_encrypt` on the parsed header. Composer auto-encrypt: when the
  peer's stored Autocrypt advertises `Mutual` AND the local account also opts
  in, pre-set `pgp.encrypt + pgp.sign` on a reply.
- **JWZ mailing-list bracket-tag stripping.** `normalize_subject` alternates
  bracket-tag stripping with `Re:` / `Fwd:` / `Re[N]:` prefix stripping in a
  loop, so `Re: [list-foo] Re: [list-bar] hello` collapses to `hello`. Mid-
  string brackets like `Build [#1234] failed` stay intact.
- **WKD positive integration test.** Network-free unit test generates a real
  inbx-managed OpenPGP key, serialises to binary, parses through
  `parse_key_bytes`, asserts the round-trip.
- **`SieveSession` type + `connect_and_auth` factory.** Scaffolding for
  ManageSieve session reuse (TUI overlay-state caching is a follow-up).
- **`cargo deny` CI job.** Runs alongside fmt / clippy / test on every push and
  pull request via `EmbarkStudios/cargo-deny-action`. Surfaces licence +
  advisory regressions at PR time instead of release time.
- **Windows in CI test matrix.** `windows-latest` joins ubuntu + macos as a
  blocking job after first run passed cleanly.
- **`cargo run` launches the TUI by default.** Workspace
  `default-members = ["apps/inbx"]`. `cargo build` / `cargo test` continue to
  act on the whole workspace via `--workspace`.
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

- **README brought current** for the cycle: PGP / WKD / Autocrypt highlights,
  RFC 8098 read receipts, JWZ §2 with bracket-tag stripping, mbox/.eml
  import-export, sieve wizard, Graph + JMAP noted as also reachable via the
  default `[transport]` setting.
- **`cmd_import` UID generation** now resumes from `MAX(uid)` for the folder
  instead of restarting at 1 — a second import into the same folder no longer
  collides with prior rows.
- **`zbase32` dep replaced with an inlined encoder** in `inbx-pgp::wkd`. Removes
  the LGPL-3.0+ taint from the binary; the encoder is ~25 lines of standard
  5-bit MSB chunking.
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

- **`apps/inbx-gui` crate.** GUI front-end deferred to a unified kryptic-sh GUI
  shell that will land once `hjkl-editor-gui` (hjkl#8) ships. Workspace member,
  `eframe` / `egui` deps, and release-workflow build steps all dropped. M20
  marked deferred in PLAN.
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

[Unreleased]: https://github.com/kryptic-sh/inbx/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/kryptic-sh/inbx/releases/tag/v0.2.0
[0.1.2]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.2
[0.1.1]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/inbx/releases/tag/v0.1.0
