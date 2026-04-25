# inbx — Email Client Plan

Modal-vim email client. Rust workspace. Reuses `hjkl-editor` for composer.
Sibling to `sqeel` (DB client), `buffr` (browser), `hjkl` (modal editor lib).

**Multi-provider first-class.** Must work with any standards-compliant
mail host, plus the big proprietary stacks. Targets:

- **Generic IMAP + SMTP** (Fastmail, Proton Bridge, self-hosted dovecot,
  iCloud, Yahoo, Yandex, etc.) — baseline.
- **Gmail / Google Workspace** — IMAP + SMTP with OAuth2 (XOAUTH2).
- **Microsoft Outlook / Microsoft 365 / Exchange Online** — OAuth2
  (MSAL device-code + auth-code flows). IMAP+SMTP path today; migrate to
  **Microsoft Graph API** (`/me/messages`, `/me/sendMail`) since
  Microsoft is deprecating basic auth and pushing IMAP toward retirement
  for enterprise tenants. Tenant-aware (`common` vs `<tenant-id>`).
- **JMAP** (Fastmail, Stalwart) — preferred when available; fewer
  round-trips, push native.
- **Outlook.com personal** — same OAuth2 path as M365 with consumer
  endpoint.

Provider abstraction lives in `inbx-net` behind a `MailProvider` trait so
new backends slot in without touching `inbx-core`.

## Workspace Layout

Mirrors `buffr` (crates/ + apps/ + xtask) over `sqeel` (flat). buffr is closer
to inbx scope: multi-pane app embedding hjkl + needing config + helper procs.

```
inbx/
├── Cargo.toml                 # workspace, resolver = "2", workspace.package
├── rust-toolchain.toml        # channel "1.95.0" (match buffr) or "stable"
├── rustfmt.toml               # edition 2021, max_width 100
├── deny.toml                  # license/advisory gate (match hjkl)
├── release-plz.toml
├── crates/
│   ├── inbx-core/             # accounts, message model, store, sync
│   ├── inbx-net/              # IMAP / SMTP / JMAP / Graph / OAuth2
│   ├── inbx-store/            # Maildir + SQLite index + tantivy search
│   ├── inbx-config/           # TOML config + XDG paths + keyring
│   ├── inbx-render/           # HTML→text, sanitize, remote-content gate
│   ├── inbx-contacts/         # address book, autocomplete, CardDAV later
│   ├── inbx-ical/             # .ics parse, invite accept/decline
│   └── inbx-composer/         # hjkl-editor wrapper, MIME builder, drafts
├── apps/
│   ├── inbx/                  # TUI binary (ratatui)
│   ├── inbx-gui/              # GUI binary (egui + eframe)
│   └── inbx-sync/             # background sync daemon (optional)
├── xtask/                     # release / asset / dev tasks
├── README.md
├── LICENSE
├── CHANGELOG.md
├── CONTRIBUTING.md
├── CODE_OF_CONDUCT.md
├── SECURITY.md
└── .github/workflows/         # ci.yml, release-plz.yml, cron.yml
```

## Crate Roles

### inbx-core
- Domain types: `Account`, `Identity`, `Signature`, `Folder`, `Message`,
  `Thread`, `Draft`, `Attachment`, `Address`, `Flags`, `Uid`, `Contact`.
- Sync engine state machine. Offline queue. Conflict resolution.
- Initial sync windowed (last 90d default, configurable).
- Online/offline detection; suspend sync on metered net.
- No network, no UI. Traits for `NetTransport`, `Store`.
- Errors: `thiserror` → `inbx_core::Error`.

### inbx-net
- `MailProvider` trait — abstract fetch/send/sync. Impls per backend.
- **IMAP** via `async-imap` (fetch, IDLE push, UID search).
- **SMTP** via `lettre` (TLS, auth).
- **JMAP** via `jmap-client` or hand-rolled (Fastmail, Stalwart).
- **MS Graph** via `reqwest` + `oauth2` — Outlook/M365 native path
  (`/me/mailFolders`, `/me/messages`, `/me/sendMail`, delta queries).
  Fallback to IMAP+SMTP for tenants still allowing basic/OAuth IMAP.
- **OAuth2** via `oauth2` crate. Flows:
  - Gmail XOAUTH2 (auth-code + refresh).
  - Microsoft (auth-code + device-code; `common` + tenant-specific).
- Token storage: `keyring` (refresh tokens), in-memory access tokens.
- Parsing: `mail-parser` + `mail-builder`. RFC 2047 encoded headers.
- Rate limit + backoff (Gmail quota, Graph 429 + Retry-After).
- Connection pool: one IMAP per acct, IDLE socket separate.
- TLS: `rustls` w/ webpki roots. Reject invalid certs.
- Two connection modes per protocol, account-configurable:
  - **Implicit TLS** (default): IMAP 993, SMTP 465. Encrypted from byte 0.
  - **STARTTLS**: IMAP 143, SMTP 587. Plaintext greeting → CAPABILITY
    must advertise `STARTTLS` → upgrade. **Hard-fail on STRIPTLS**: if
    config requests starttls and capability missing OR upgrade fails,
    abort connection — never fall through to plaintext.
- No plaintext-only mode. Ever.
- Per-account proxy / SOCKS via `tokio-socks`.
- DKIM/SPF/DMARC verify on inbound for display badge.
- IMAP UIDPLUS for Drafts/Sent append. Folder ops (create/rename/
  delete/move, subscriptions).
- **Sieve** (RFC 5228) client via ManageSieve for server-side filters.
- List-Unsubscribe RFC 2369 + RFC 8058 one-click.
- Async: `tokio` full.

### inbx-store
- Maildir-style on-disk per account (`~/.local/share/inbx/<acct>/`).
- SQLite index via `sqlx` (sqlite + tokio runtime, match sqeel choice).
- Full-text via `tantivy`. Threading via JWZ algorithm.
- Schema migrations in `migrations/`.
- Sent folder append on send. Drafts folder bidirectional sync.
- Quota tracking + over-quota error UX.
- Import/export: mbox, .eml, mh.

### inbx-config
- `~/.config/inbx/config.toml`. `directories` crate for XDG.
- Creds in OS keyring via `keyring` crate.
- Keymap + theme + account list.

### inbx-composer
- Embeds `hjkl-editor`. Public API:
  - `Composer::new_reply(msg)`, `::new_forward`, `::new_blank`,
    `::from_template(name)`.
  - Holds `hjkl_editor::runtime::Editor` per draft.
  - Renders via `hjkl_buffer::BufferView` (ratatui feature on).
  - Header fields (To/Cc/Bcc/Subject) as separate single-line buffers
    sharing the same hjkl FSM. Recipient autocomplete via `inbx-contacts`.
- Per-identity signature (plain + html). Send-as / aliases.
- Templates / canned replies (TOML files in config).
- MIME assembly via `mail-builder`. Inline images (cid:) supported.
- Attachment paste via `arboard`. MIME sniff via `infer`. Size cap.
- Drafts saved local + appended to server Drafts folder.
- When hjkl 0.1.0 ships `spec::*` traits, migrate from `runtime::*`.

### inbx-render
- HTML → terminal text via `html2text` (TUI) or sanitized HTML for GUI.
- Sanitize via `ammonia` allow-list. Strip `<script>`, event handlers,
  `<meta http-equiv>`, external CSS.
- **Remote content blocked by default.** Per-sender allow-list.
  Tracking-pixel detection (1x1 imgs, known beacon hosts).
- Phishing heuristics: reply-to ≠ from domain, lookalike domains
  (homoglyph), link text-domain mismatch.
- DKIM/SPF/DMARC display badge from `inbx-net` results.

### inbx-contacts
- Local SQLite store. Recipient autocomplete (frecency-ranked).
- Auto-harvest from sent mail.
- CardDAV sync deferred (post-v1).

### inbx-ical
- `.ics` parse via `icalendar` crate.
- Display invite in message preview. Accept/decline/tentative
  generates `METHOD:REPLY` `.ics` and sends via SMTP/Graph.
- No calendar storage. Hand off to external calendar app via
  `xdg-open` for full view.

## Apps

### apps/inbx (TUI)
- `ratatui` 0.29 + `crossterm` 0.28 (match sqeel/buffr).
- Layout: folder list | thread list | message preview | composer overlay.
- Vim keys via hjkl-editor across all panes.
- Mouse via `MouseCapture` (sqeel pattern).
- Markdown render: `pulldown-cmark` for plaintext alt; HTML → text via
  `html2text`.

### apps/inbx-gui (GUI)
- `egui` 0.31 + `eframe` 0.31 (match sqeel-gui).
- Same three-pane plus native file picker for attachments.
- Optional later milestone — TUI first.

### apps/inbx-sync (daemon)
- Headless sync. IDLE connections per account. Notifies TUI/GUI via
  unix socket or shared SQLite.
- Optional. v1 can run sync inside TUI.

## Security & Privacy

- **Default-deny remote content** in HTML mail.
- **Tracking pixel** strip + report.
- **TLS**: rustls, webpki roots, no plaintext fallback.
- **Tokens**: keyring only, never on disk plaintext, redact in logs.
- **DKIM/SPF/DMARC** verify, display result badge.
- **Phishing heuristics** on display.
- **No auto-execute** attachments. Sniff MIME, never trust extension.
- **S/MIME** + **PGP** (sequoia-openpgp) for sign + encrypt.
- **Read receipts**: never auto-send; user prompt only.
- **Encryption at rest**: deferred. Threat model documented.
- **Sandbox HTML**: GUI uses sanitized blob in webview; TUI text-only.

## Notifications & Integration

- Desktop notifications via `notify-rust` (libnotify / native).
- Per-folder notification rules.
- `xdg-open` for attachment preview, calendar handoff.
- Optional MPRIS-style D-Bus iface for status (later).

## Performance Budgets

- Cold start to TUI: < 200ms.
- Folder switch render: < 50ms.
- Local search: < 100ms for 100k msgs.
- Memory cap: < 200MB resident for 100k msgs indexed.

## Accessibility & i18n

- TUI screen-reader hints; no color-only signal.
- High-contrast theme + colorblind palettes.
- UTF-8 everywhere. RTL rendering. IDN in addresses.
- Locale-aware date/time formatting.

## Testing

- Unit per crate.
- Integration via `mailcrab` or `docker-mailserver` fixture.
- MS Graph: recorded HTTP via `wiremock`.
- Fuzz `mail-parser` boundary on real corpora.
- HTML render snapshot tests.
- Property tests on threading (JWZ).
- CI matrix: stable + MSRV, Linux + macOS.

## Logging

- `tracing` + `tracing-subscriber`. JSON in headless, pretty in TUI.
- Log to `$XDG_STATE_HOME/inbx/log/`. Daily rotate. 7-day retain.
- **Redact** Authorization headers, OAuth tokens, full message bodies.

## Distribution

- `cargo install inbx`.
- Arch AUR (`inbx-bin`, `inbx-git`).
- Homebrew tap.
- Debian `.deb` via `cargo-deb`.
- Static musl release in CI (release-plz).
- Self-update opt-in (post-v1).

## CLI Surface

- `inbx send` — read RFC 5322 from stdin or compose flags.
- `inbx fetch [--account]` — one-shot sync.
- `inbx search <query>` — local index query.
- `inbx accounts {add,list,remove,test}`.
- `inbx grep <regex>` — pipe-friendly across mailboxes.
- `inbx export <folder> --mbox` / `inbx import <file>`.
- `inbx pipe` — stdin → message, useful for `mailx` replacement.
- `inbx oauth login <account>` — interactive auth flow.

## Workspace Conventions (match siblings)

- `workspace.package`: version `0.0.1`, edition `2024`, rust-version
  `1.95`, license `MIT`, authors `kryptic.sh`.
- All crates inherit via `.workspace = true` (buffr pattern).
- Workspace deps for tokio, ratatui, crossterm, anyhow, thiserror,
  serde, tracing.
- **hjkl dep gated on 0.1.0** (`spec::*` traits). Composer milestone (M6)
  blocked until hjkl ships. M1–M5 proceed without hjkl.
- When ready, add as path dep then bump on semver:
  ```toml
  hjkl-editor = { path = "../../hjkl/crates/hjkl-editor" }
  hjkl-buffer = { path = "../../hjkl/crates/hjkl-buffer", features = ["ratatui"] }
  ```
- `release.profile`: lto thin, codegen-units 1, strip.
- Errors: `thiserror` per crate, `anyhow` at app boundary.

## Milestones

| # | Goal | Crates touched |
|---|------|----------------|
| M1 | Workspace skeleton, config, keyring | core, config |
| M2 | IMAP fetch → Maildir + SQLite. CLI list. | net, store, app/inbx |
| M3 | SMTP send + Sent folder append. CLI send. | net, store, app/inbx |
| M4 | TUI: folder/thread/preview panes (read-only) | app/inbx |
| M5 | HTML render + sanitize + remote-content gate | render, app/inbx |
| M6 | Composer w/ hjkl-editor, identities, signatures | composer, app/inbx |
| M7 | Drafts sync (server append, UIDPLUS) | net, store, composer |
| M8 | Contacts + recipient autocomplete | contacts, composer |
| M9 | OAuth2 Gmail. Token refresh. | net, config |
| M10 | Microsoft OAuth2 + Outlook via IMAP+SMTP | net, config |
| M11 | MS Graph API backend | net |
| M12 | Search (tantivy) + threading (JWZ) | store |
| M13 | IDLE push, offline queue, rate limit/backoff | net, core |
| M14 | Calendar invites (.ics display + reply) | ical, render |
| M15 | DKIM/SPF/DMARC verify + phishing heuristics | net, render |
| M16 | List-Unsubscribe (RFC 8058 one-click) | net, render |
| M17 | Sieve (server-side filters) + vacation responder | net |
| M18 | Notifications (`notify-rust`) | app/inbx |
| M19 | Import/export (mbox, .eml) | store, app/inbx |
| M20 | GUI MVP | app/inbx-gui |
| M21 | JMAP. Client-side rules. | net, core |
| M22 | PGP + S/MIME (sign + encrypt) | net, composer, render |
| M23 | CardDAV contacts sync | contacts, net |
| M24 | Templates / canned replies | composer |

## Open Questions

- HTML render: `html2text` (terse) or embed `wry` webview (heavy)?
  Lean `html2text` for TUI, optional webview pane for GUI.
- Sync daemon now or v2? Lean v2 — keep TUI self-contained first.
- Per-account encryption-at-rest for Maildir? Defer.
- ~~hjkl `runtime::*` vs `spec::*`?~~ **Decided: wait for hjkl 0.1.0
  `spec::*`.** M6 (composer) blocked until hjkl ships.
- ~~OAuth from day 1?~~ **Decided: app password for MVP.** OAuth at M9.

## Non-Goals (v1)

- Full calendar app (only invite display + RSVP via `.ics`).
- Standalone contacts manager (only address book for autocomplete).
- RSS reader.
- Mobile — TUI/GUI desktop only.
- Web client.
- Built-in webmail server.

## Shared UI Crate — Deferred

No `kryptic-ui` / `krui` extraction now. Reasons:

- Only sqeel is a working impl; buffr early scaffold; inbx unstarted. Need
  2–3 real apps before the shared shape is visible.
- Domains diverge (schema browser ≠ folder tree ≠ browser tabs). Forced
  unification fights each app later.
- `hjkl` already extracts the genuinely shared piece (modal input + buffer).
- Extraction cost now: refactor sqeel + delay inbx + crate version churn.

**Approach:** copy sqeel patterns into inbx (mouse capture, three-pane
render loop, command palette). Free to diverge. Reassess after inbx M5.

**Rule of three.** Extract on evidence: when sqeel + inbx + buffr show the
same widget, pull it into `krui`. Likely candidates *eventually*: ratatui
mouse capture wrapper, command palette widget, ratatui+egui dual-frontend
trait, theme/config loader.
