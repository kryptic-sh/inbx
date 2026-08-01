# inbx — Email Client Plan

Modal-vim email client. Rust workspace. Reuses the `hjkl-*` stack across the
TUI, composer, contacts, and config layers. Sibling to `sqeel` (DB client),
`buffr` (browser), `hjkl` (modal editor lib).

> **How to read this file.** This is the design index — intent, architecture,
> and rationale. It is not a feature list. Items that have not shipped are
> tagged **(not implemented)**; everything else is either shipped or a
> convention the tree actually follows. `README.md` describes what the binaries
> do today; `CHANGELOG.md` records when each piece landed.

### hjkl crate adoption (workspace-wide)

| Crate                                                    | Use in inbx                                                                                                                 |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `hjkl-editor` (re-exports `hjkl-engine` + `hjkl-buffer`) | Composer body editor; per-field editors via `hjkl-form`                                                                     |
| `hjkl-config`                                            | XDG path resolution + TOML loader (adopted)                                                                                 |
| `hjkl-form`                                              | To/Cc/Bcc/Subject header fields, in-TUI account wizard, Sieve vacation wizard — each text field hosts its own `Editor`      |
| `hjkl-picker`                                            | Folder picker, account switcher, message search/jump, attachment picker, template picker, contacts overlay                  |
| `hjkl-ratatui`                                           | KeyEvent bridging + spinner widget. Form rendering is still bespoke ratatui glue in `apps/inbx/src/tui/`                    |
| `hjkl-clipboard`                                         | Attachment paste + body yank/put with OSC 52 SSH fallback — replaces `arboard`                                              |
| `hjkl-bonsai`                                            | Tree-sitter highlighting for `text/x-patch` / `text/x-diff` bodies. Shipped behind the optional `tree-sitter` cargo feature |

All `hjkl-*` crates pinned by minor caret per the dep-style memory; breaking
changes ride a major bump. The lockstep `=0.0.x` pattern is dead (matches
`buffr` / `sqeel`).

**Multi-provider first-class.** Must work with any standards-compliant mail
host, plus the big proprietary stacks. Targets:

- **Generic IMAP + SMTP** (Fastmail, Proton Bridge, self-hosted dovecot, iCloud,
  Yahoo, Yandex, etc.) — baseline.
- **Gmail / Google Workspace** — IMAP + SMTP with OAuth2 (XOAUTH2).
- **Microsoft Outlook / Microsoft 365 / Exchange Online** — OAuth2 auth-code
  flow. Both paths ship: OAuth2 over IMAP+SMTP, and the native **Microsoft Graph
  API** backend in `crates/inbx-net/src/graph.rs` (`/me/mailFolders`,
  `/me/messages`, `/me/messages/delta`, `/me/sendMail`), driven by the
  `inbx graph` subcommand or selected as the account's default transport.
  Tenant-aware (`common` vs `<tenant-id>`).
- **JMAP** (Fastmail, Stalwart) — preferred when available; fewer round-trips,
  push native.
- **Outlook.com personal** — same OAuth2 path as M365 with consumer endpoint.

Provider abstraction lives in `inbx-net` behind a `MailProvider` trait so new
backends slot in without touching the storage / config layers.

## Workspace Layout

Mirrors `buffr` (crates/ + apps/ + xtask) over `sqeel` (flat). buffr is closer
to inbx scope: multi-pane app embedding hjkl + needing config + helper procs.

```
inbx/
├── Cargo.toml                 # workspace, resolver = "2", workspace.package
├── rust-toolchain.toml        # channel "1.95.0" (match buffr)
├── rustfmt.toml               # edition 2021, max_width 100
├── deny.toml                  # license/advisory gate (match hjkl)
├── crates/
│   ├── inbx-net/              # IMAP / SMTP / JMAP / Graph / OAuth2 / Sieve
│   ├── inbx-store/            # Maildir + SQLite index + FTS5 search
│   ├── inbx-config/           # TOML config + XDG paths + keyring + theme
│   ├── inbx-pgp/              # gnupg shell-out + rpgp, WKD, PGP/MIME
│   ├── inbx-render/           # HTML→text, sanitize, remote-content gate
│   ├── inbx-dav/              # shared CalDAV/CardDAV PROPFIND helpers
│   ├── inbx-contacts/         # address book, autocomplete, CardDAV
│   ├── inbx-ical/             # .ics parse, invite accept/decline, CalDAV
│   ├── inbx-composer/         # hjkl-editor wrapper, MIME builder, drafts
│   ├── inbx-ipc/              # unix-socket event channel (daemon → TUI)
│   └── inbx-sync/             # sync engine library (IDLE loop, outbox)
├── apps/
│   ├── inbx/                  # CLI + TUI binary (ratatui)
│   └── inbx-sync/             # background sync daemon
├── xtask/                     # stub; no tasks defined yet
├── README.md
├── LICENSE
├── CHANGELOG.md
├── CONTRIBUTING.md
├── SECURITY.md
├── PLAN.md
├── docs/                      # threat-model.md, perf-budgets.md
├── pkg/                       # alpine/ aur/ homebrew/ package templates
└── .github/workflows/         # ci.yml (build + test + release, one file)
```

The Code of Conduct is inherited from the org-level `kryptic-sh/.github` repo —
there is no `CODE_OF_CONDUCT.md` in this tree.

## Crate Roles

> **Note:** `inbx-core` was originally planned as a domain-types crate
> (`Account`, `Message`, `Thread`, sync FSM, etc.) but the types ended up
> scattered across `inbx-config` (Account / AuthMethod) and `inbx-store`
> (FolderRow / MessageRow / OutboxRow). The empty crate was dropped at v0.1.3.
> If a unifying domain layer becomes useful later (e.g., when the sync FSM needs
> a home), reintroduce it then — don't hold a slot for hypothetical future code.

### inbx-net

- `MailProvider` trait — abstract fetch/send/sync. Impls per backend.
- **IMAP** via `async-imap` (fetch, IDLE push, UID search).
- **SMTP** via `lettre` (TLS, auth).
- **JMAP** hand-rolled over `reqwest` (Fastmail, Stalwart) — the `jmap-client`
  crates churn too fast to pin.
- **MS Graph** via `reqwest` + `oauth2` — Outlook/M365 native path
  (`/me/mailFolders`, `/me/messages`, `/me/sendMail`, delta queries). Fallback
  to IMAP+SMTP for tenants still allowing basic/OAuth IMAP.
- **OAuth2** via `oauth2` crate. Flows:
  - Gmail XOAUTH2 (auth-code + PKCE + refresh).
  - Microsoft auth-code (`common` + tenant-specific). Device-code flow **(not
    implemented)**.
- Token storage: `keyring` (refresh tokens), in-memory access tokens.
- Parsing: `mail-parser` + `mail-builder`. RFC 2047 encoded headers.
- Rate limit + backoff (Gmail quota, Graph 429 + Retry-After). **(not
  implemented)** — the only backoff that ships is the outbox send retry in
  `inbx-store` (30s doubling, capped at 1h).
- Connection pool: one IMAP per acct, IDLE socket separate. **(not
  implemented)** — each operation opens and tears down its own IMAP session via
  `connect_provider`; only the IDLE watch loop holds a long-lived socket.
- TLS: `rustls` w/ webpki roots. Reject invalid certs.
- Two connection modes per protocol, account-configurable:
  - **Implicit TLS** (default): IMAP 993, SMTP 465. Encrypted from byte 0.
  - **STARTTLS**: IMAP 143, SMTP 587. Plaintext greeting → CAPABILITY must
    advertise `STARTTLS` → upgrade. **Hard-fail on STRIPTLS**: if config
    requests starttls and capability missing OR upgrade fails, abort connection
    — never fall through to plaintext.
- No plaintext-only mode. Ever.
- Per-account proxy / SOCKS via `tokio-socks` (shipped v0.1.x). IMAP and Sieve
  route through the proxy at the TCP layer; Graph, JMAP, and OAuth flows use
  `reqwest`'s built-in proxy support. **SMTP (lettre 0.11) does not honor the
  proxy** — a `tracing::warn!` fires once at startup if the account has a
  `proxy` field set; workaround: route SMTP through a SOCKS-aware tunnel such as
  `proxychains` or `redsocks`.
- DKIM/SPF/DMARC — read the inbound `Authentication-Results` header for the
  display badge (see `inbx-render` below); inbx does not verify them itself.
- IMAP APPEND for Drafts/Sent. UIDPLUS `APPENDUID` handling **(not
  implemented)** — the appended UID is not read back. Folder ops
  (create/rename/delete/move, subscriptions) ship.
- **Sieve** (RFC 5228) client via ManageSieve for server-side filters.
- List-Unsubscribe RFC 2369 + RFC 8058 one-click.
- Async: `tokio` full.

### inbx-store

- Maildir-style on-disk per account (`~/.local/share/inbx/<acct>/`).
- SQLite index via `sqlx` (sqlite + tokio runtime, match sqeel choice), at
  `<acct>/index.sqlite`.
- Full-text via SQLite **FTS5** (`messages_fts`), not `tantivy` — a second index
  engine was not worth the binary size. Threading via JWZ algorithm.
- Schema migrations in `migrations/`.
- Sent folder append on send (skippable with `--no-save`). Draft APPEND to the
  server's Drafts folder. Bidirectional draft sync **(not implemented)**.
- Quota tracking + over-quota error UX. **(not implemented)**
- Import/export: mbox and single `.eml`. `mh` **(not implemented)**.

### inbx-config

- `~/.config/inbx/config.toml`, plus `~/.config/inbx/theme.toml` (six RGB
  slots). XDG paths via `hjkl-config` / `directories`.
- Creds in OS keyring via `keyring` crate.
- Account list, per-account transport / proxy / PGP / CardDAV blocks.
  User-configurable keymap **(not implemented)** — chords live in the `binds.rs`
  table and are not read from config.

### inbx-composer

- Embeds `hjkl-editor` for the body and `hjkl-form` for the header block. Public
  API:
  - `Composer::new_blank(identity)`, `::new_reply(identity, raw, reply_all)`,
    `::new_forward(identity, raw)`, `templates::from_template(...)`.
  - Holds one `hjkl_editor::runtime::Editor` for the body draft.
  - Headers (To/Cc/Bcc/Subject) live in a `hjkl_form::Form` — each field gets
    its own `Editor`, modal `:` ex commands, and tab/shift-tab focus rotation
    come for free.
  - `hjkl_ratatui::form::render_form` for the header block **(not implemented)**
    — the TUI draws the header fields itself and only pulls `spinner` + the
    crossterm key bridge from `hjkl-ratatui`.
  - Recipient autocomplete inside an address field (`<C-x><C-o>` / `@`-trigger)
    backed by a `PickerLogic` over `inbx-contacts` **(not implemented)** — the
    address book is reachable from the `C` contacts overlay instead, which
    composes to the selected contact.
- Per-identity signature (plain + html). Send-as / aliases.
- Templates / canned replies — RFC 5322 `.eml` files under
  `$XDG_DATA_HOME/inbx/<account>/templates/`, not TOML in the config dir.
- MIME assembly via `mail-builder`. Inline images (cid:) supported.
- Attachment paste via `hjkl-clipboard` (OSC 52 fallback over SSH). Outgoing
  content type is guessed from the filename extension — content sniffing via
  `infer` **(not implemented)**. Size cap **(not implemented)**.
- Drafts saved local + appended to server Drafts folder.

### inbx-render

- HTML → terminal text via `html2text` (TUI). The sanitized-HTML string is also
  returned on `Rendered` for a future GUI shell; nothing renders it today.
- Sanitize via `ammonia` allow-list. Strip `<script>`, event handlers,
  `<meta http-equiv>`, external CSS.
- **Remote content blocked.** Every call site passes `RemotePolicy::Block`;
  remote `<img src>` is rewritten to a `data:` placeholder. Per-sender
  allow-list **(not implemented)** — `RemotePolicy::Allow` exists in the API but
  no binary selects it. Tracking-pixel detection (1x1 imgs, known beacon hosts).
- Phishing heuristics: reply-to ≠ from domain, lookalike domains (homoglyph),
  link text-domain mismatch.
- DKIM/SPF/DMARC display badge parsed out of the receiving MTA's
  `Authentication-Results` header (RFC 8601). inbx runs no DKIM crypto and does
  no DNS lookups for this — a real verifier is **(not implemented)**.

### inbx-contacts

- Local SQLite store (`<acct>/contacts.sqlite`). Frecency-ranked ordering.
- A `hjkl_picker::PickerLogic` impl (`ContactsSource`) inside `inbx-contacts`
  **(not implemented)** — the TUI feeds contact rows through its own generic
  `StashedSource` picker adapter instead.
- Auto-harvest from sent mail.
- CardDAV sync shipped — `inbx contacts carddav pull|push|discover`, sharing the
  PROPFIND walk in `inbx-dav`.

### inbx-ical

- `.ics` parse via `icalendar` crate.
- Display invite in message preview. Accept/decline/tentative generates
  `METHOD:REPLY` `.ics` and sends via SMTP/Graph.
- CalDAV pull / put / delete / discover; pulled events cache as
  `<data_dir>/<acct>/calendar/<uid>.ics`.
- Hand off to an external calendar app via `xdg-open` **(not implemented)** —
  the only `xdg-open` call in the tree opens List-Unsubscribe `https:` URLs.

## Apps

### apps/inbx (TUI)

- `ratatui` + `crossterm` (versions pinned in `[workspace.dependencies]`).
- Layout: folder list | thread list | message preview | composer overlay.
- Vim keys via `hjkl-editor` across all panes.
- `hjkl-ratatui` adapters in use: the crossterm `KeyEvent` bridge and the
  `spinner` widget (IMAP fetch / SMTP send progress). `Style`/`Color`
  conversions and prompt widgets **(not implemented)** — drawn locally.
- `hjkl-picker`-backed overlays, all behind the `<Space>` leader:
  - `<Space>f` folder picker, `<Space>b` account switcher, `<Space>m` message
    picker, `<Space>a` attachment picker (saves to `~/Downloads/`), `<Space>t`
    template picker. `C` opens the contacts overlay.
- `hjkl-form` powers the in-TUI account wizard (`<Space>n`, 10 fields) and the
  Sieve vacation wizard. The CLI `inbx accounts add` is a plain stdin prompt
  loop with autoconfig suggestions, not a form.
- `hjkl-clipboard` for yank/put + OSC 52 SSH fallback (TUI users on remote
  shells get clipboard sync without `xclip`/`pbcopy`).
- Mouse via `MouseCapture` (sqeel pattern).
- HTML → text via `html2text`. Markdown render via `pulldown-cmark` **(not
  implemented)**.

### GUI (deferred)

GUI front-end was removed from this workspace. A unified GUI shell will ship
across kryptic-sh apps (sqeel, inbx, etc.) once the shared `hjkl-editor-gui`
adapter (hjkl#8) lands. inbx will plug into that shell rather than maintaining
its own egui glue.

### apps/inbx-sync (daemon)

- Headless sync. IDLE connections per account. Notifies the TUI over the
  `inbx-ipc` unix socket (`$XDG_RUNTIME_DIR/inbx-sync.sock`, chmod `0600`).
- Optional — `inbx sync` runs the same engine in-process inside the TUI binary.

## Security & Privacy

- **Remote content denied** in HTML mail (no per-sender allow-list yet).
- **Tracking pixel** strip + report.
- **TLS**: rustls, webpki roots, no plaintext mode.
- **Tokens**: keyring only, never on disk plaintext, redact in logs.
- **DKIM/SPF/DMARC** verdicts read from `Authentication-Results`, shown as a
  badge. inbx verifies nothing itself.
- **Phishing heuristics** on display.
- **No auto-execute** attachments — the picker writes the selected part to
  `~/Downloads/` and inbx spawns nothing to open it. MIME sniffing **(not
  implemented)**; outgoing content types come from the extension.
- **PGP** for sign + encrypt. **S/MIME is detection only** — `inbx-render`
  labels a signed or encrypted S/MIME part; verifying, decrypting, or producing
  one is **(not implemented)**. Crate: **`pgp`** (rpgp, pure Rust, RFC 9580,
  MIT/Apache-2.0) — covers v4+v6 keys, sign/verify/encrypt/decrypt, key gen,
  ASCII armor, passphrase-protected secrets, Autocrypt 1.1. Production-tested by
  himalaya + Delta Chat. No `sequoia-openpgp` (heavy C deps via nettle/openssl).
  Two key sources, account-configurable:
  - **`gnupg`** — keys live in the system GPG keyring at `~/.gnupg/`. Default
    for users who already manage keys via `gpg`. inbx shells out to
    `gpg --export`, `gpg --decrypt`, `gpg --sign` — preserves gpg-agent,
    pinentry, smartcard / OpenPGP card support; no private-key extraction.
  - **`inbx-managed`** — keypair lives at `~/.local/share/inbx/<acct>/pgp/`
    (armored), passphrase in the OS keyring. Crypto runs through `pgp` directly.
    For users who want a per-account email key separate from their
    identity-grade GPG key, or who are GPG-free.

  Per-account `[accounts.pgp]` block: `key_source = "gnupg" | "inbx-managed"`,
  optional `key_fingerprint` to pick a specific key, optional `managed_dir`
  override, and `prefer_encrypt_mutual` (default `true`). `detect_default()`
  picks `gnupg` when `~/.gnupg/` exists, else `inbx-managed`. There is no
  `key_id` field.

- **Read receipts**: never auto-send; user prompt only. Implemented at v0.1.x
  via `Y` (send) / `N` (decline) in the Preview pane. Detects
  `Disposition-Notification-To:` on render (`inbx-render`); generates RFC 8098
  MDN (`multipart/report; report-type=disposition-notification`) via
  `inbx_net::build_mdn` only on explicit `Y` keystroke. Responded UIDs tracked
  in-memory per session (not persisted).
- **Encryption at rest**: deferred. Threat model documented at
  [`docs/threat-model.md`](docs/threat-model.md).
- **Sandbox HTML**: TUI is text-only. The GUI-in-a-webview path is **(not
  implemented)** — no GUI ships.

## Notifications & Integration

- Desktop notifications via `notify-rust` (libnotify / native).
- Per-folder notification rules **(not implemented)** — `--notify` is
  all-or-nothing per account.
- `xdg-open` for List-Unsubscribe URLs. Attachment preview and calendar handoff
  **(not implemented)**.
- Optional MPRIS-style D-Bus iface for status (later) **(not implemented)**.

## Performance Budgets

- Cold start to TUI: < 200ms.
- Folder switch render: < 50ms.
- Local search: < 100ms for 100k msgs.
- Memory cap: < 200MB resident for 100k msgs indexed.

Measured against the `inbx-store` criterion bench — see
[`docs/perf-budgets.md`](docs/perf-budgets.md). The memory budget is not
benchmarked.

## Accessibility & i18n

Intent, none of it built yet:

- TUI screen-reader hints; no color-only signal. **(not implemented)**
- High-contrast theme + colorblind palettes. **(not implemented)** — the theme
  system exists (`theme.toml`, six RGB slots) but ships one dark palette.
- UTF-8 everywhere. RTL rendering, IDN in addresses **(not implemented)**.
- Locale-aware date/time formatting. **(not implemented)** — message rows show a
  relative age (`3m`, `2h`, `4d`).

## Testing

Shipped: unit tests per crate plus integration tests under `tests/` in
`inbx-net`, `inbx-store`, `inbx-pgp`, and `inbx-render`. CI runs
`cargo nextest run --workspace --all-features` and a `cargo test --doc` pass on
ubuntu / macos / windows, stable toolchain only.

Still intent:

- Integration via `mailcrab` or `docker-mailserver` fixture. **(not
  implemented)**
- MS Graph: recorded HTTP via `wiremock`. **(not implemented)**
- Fuzz `mail-parser` boundary on real corpora. **(not implemented)** — no
  `fuzz/` directories exist.
- HTML render snapshot tests. **(not implemented)** — no `insta` dependency.
- Property tests on threading (JWZ). **(not implemented)** — no `proptest`
  dependency.
- A dedicated MSRV job. **(not implemented)**

## Logging

- `tracing` + `tracing-subscriber`. TUI mode writes to the log file only (a
  stderr layer corrupts the alt-screen); CLI subcommands tee stderr + file. JSON
  output **(not implemented)** — both sinks use the pretty formatter.
- Log to `<data_local_dir>/inbx/log/inbx.YYYY-MM-DD` (`~/.local/share/inbx/log`
  on Linux). Daily rotate via `tracing-appender`. Retention / pruning **(not
  implemented)** — old files accumulate.
- Default `EnvFilter` is
  `info,html5ever=error,markup5ever=error,ammonia=warn,html2text=warn`;
  `RUST_LOG` overrides it.
- **Redact** Authorization headers, OAuth tokens, full message bodies. There is
  no redaction layer or filter **(not implemented)** — this holds only because
  no call site logs those values today. Adding one would be the safer design.

## Distribution

Shipped, all driven by the tag-gated jobs in `.github/workflows/ci.yml`:

- Prebuilt archives + sha256 sidecars on the GitHub release for
  `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`,
  `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`.
- Arch AUR `inbx-bin` (`pkg/aur/PKGBUILD-bin.in`).
- Alpine `.apk` (`pkg/alpine/APKBUILD.in`), built from the musl tarball.
- Homebrew tap `kryptic-sh/tap/inbx` (`pkg/homebrew/inbx.rb.in`).

Not shipped:

- `cargo install inbx` **(not implemented)** — nothing is published to
  crates.io; there is no release-plz setup.
- Arch `inbx-git` **(not implemented)** — only the `-bin` PKGBUILD exists.
- Debian `.deb` via `cargo-deb` **(not implemented)**.
- Self-update opt-in (post-v1) **(not implemented)**.

## CLI Surface

`README.md` carries the current list. Design notes on the shape:

- `inbx send` — read RFC 5322 from stdin. Doubles as the `mailx`-style pipe
  target, so a separate `inbx pipe` was never added.
- `inbx fetch [--account]` — one-shot sync.
- `inbx search <query>` — local index query (FTS5).
- `inbx accounts {add,list,test,folders,edit,remove}`.
- `inbx export` / `inbx import` — mbox or single `.eml`, over `--output` /
  `--input` paths (`-` for stdout / stdin).
- `inbx oauth login <account>` — interactive auth flow.
- `inbx grep <regex>` — pipe-friendly regex across mailboxes. **(not
  implemented)**; `inbx search` covers the common case.

## Workspace Conventions (match siblings)

- `workspace.package`: edition `2024`, rust-version `1.95` (the MSRV floor, also
  the channel pinned in `rust-toolchain.toml`), license `MIT`. One version
  string in the root `Cargo.toml` covers every crate — see the releases page for
  its current value.
- All crates inherit via `.workspace = true` (buffr pattern).
- Workspace deps for tokio, ratatui, crossterm, anyhow, thiserror, serde,
  tracing.
- **hjkl-\* deps pinned by minor caret**, resolved from crates.io. The lockstep
  `=0.0.x` regime ended at hjkl 0.1.0; each hjkl crate now versions
  independently, so each gets its own caret in `[workspace.dependencies]` — see
  the `hjkl-*` block in the root `Cargo.toml` for the live pins. Breaking
  changes ride a major bump on the affected crate; consumers pin the new caret.
  The `inbx — hjkl release watcher` Claude routine opens an integration PR per
  published release.
- `release.profile`: lto thin, codegen-units 1, strip.
- Errors: `thiserror` per crate, `anyhow` at app boundary.

## Tracking

Concrete work items live in
[GitHub Issues](https://github.com/kryptic-sh/inbx/issues). Shipped milestones
live in [`CHANGELOG.md`](CHANGELOG.md). This document is the design index —
architecture, conventions, and rationale.

## Open Questions

- HTML render: `html2text` (terse) or embed `wry` webview (heavy)? Lean
  `html2text` for TUI, optional webview pane for GUI.

## Non-Goals (v1)

- Full calendar app — no calendar UI. Invite display, RSVP, and CalDAV
  pull/put/delete over the CLI have since shipped; a grid view has not.
- Standalone contacts manager — address book plus CardDAV pull/push only.
- RSS reader.
- Mobile — TUI/GUI desktop only.
- Web client.
- Built-in webmail server.

## Shared UI Crate — Deferred

No `kryptic-ui` / `krui` extraction now. Reasons:

- Domains diverge (schema browser ≠ folder tree ≠ browser tabs). Forced
  unification fights each app later.
- `hjkl` already extracts the genuinely shared piece (modal input + buffer).
- Extraction cost: refactor sqeel + delay inbx + crate version churn.

**Rule of three.** Extract on evidence: when sqeel + inbx + buffr show the same
widget, pull it into `krui`. Tracked at
[#5](https://github.com/kryptic-sh/inbx/issues/5).
