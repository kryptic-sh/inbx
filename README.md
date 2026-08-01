# inbx

Modal-vim email client. Rust workspace.

[![CI](https://github.com/kryptic-sh/inbx/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/inbx/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/kryptic-sh/inbx)](https://github.com/kryptic-sh/inbx/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Website](https://img.shields.io/badge/website-kryptic.sh%2Finbx-7ee787)](https://www.kryptic.sh/inbx/)

Sibling to [sqeel](https://github.com/kryptic-sh/sqeel),
[buffr](https://github.com/kryptic-sh/buffr),
[hjkl](https://github.com/kryptic-sh/hjkl).

## Status

Pre-1.0. Working CLI + TUI. Shipped milestones live in
[CHANGELOG.md](CHANGELOG.md); the current version is whatever the
[releases page](https://github.com/kryptic-sh/inbx/releases) shows.

## Providers

| Provider                | Status  | Path                                   |
| ----------------------- | ------- | -------------------------------------- |
| Generic IMAP + SMTP     | Working | TLS + STARTTLS, app password / OAuth2  |
| Gmail / Workspace       | Working | OAuth2 (XOAUTH2 SASL) over IMAP + SMTP |
| Microsoft 365 / Outlook | Working | OAuth2 IMAP/SMTP, or native MS Graph   |
| Fastmail / Stalwart     | Working | JMAP (basic + Bearer auth)             |

## Workspace

```
crates/
  inbx-net        IMAP / SMTP / JMAP / Graph / OAuth2 / Sieve / IDLE
  inbx-store      Maildir + SQLite + FTS5 + outbox
  inbx-config     TOML config + XDG + keyring + theme
  inbx-pgp        gnupg shell-out + rpgp key sources, WKD, PGP/MIME
  inbx-render     HTML sanitize + remote-content gate + auth + PGP
  inbx-dav        shared CalDAV / CardDAV PROPFIND + XML scrape
  inbx-contacts   address book + autocomplete + CardDAV
  inbx-ical       calendar invite display + RSVP
  inbx-composer   hjkl-editor wrapper, MIME builder, templates
  inbx-ipc        unix-socket event channel (sync daemon → TUI)
  inbx-sync       sync engine library (IDLE loop, outbox drain)
apps/
  inbx            CLI + TUI binary (ratatui)
  inbx-sync       background sync daemon
xtask/            cargo-xtask stub (no tasks defined yet)
```

## Highlights

- **TUI** with vim navigation (j/k, h/l, gg/G, Tab), a `<Space>` leader for
  pickers and message ops, and a modal composer overlay (`c`/`r`/`R`/`f`, Ctrl-S
  send, Ctrl-D save draft, Ctrl-Q discard); animated splash screen on startup
  (skip with any key). The status line shows one of three modes — `NORMAL`,
  `INSERT` (a composer is open), `SEARCH` (the `/` overlay is open). There is no
  hint / label / easymotion mode.
- **Per-folder unread badge** in the sidebar, relative timestamps on message
  rows, and `<Space>R` to mark every message in the current folder read
- **Auth** — app password via OS keyring, OAuth2 (Gmail + Microsoft) with PKCE +
  auth-code loopback flow, refresh tokens stored in the keyring
- **Render** — HTML sanitized via ammonia, remote content blocked by default,
  tracker pixels surfaced, SPF/DKIM/DMARC verdicts read from the receiving MTA's
  `Authentication-Results` header (RFC 8601 — inbx runs no DKIM crypto itself),
  phishing heuristics, PGP / S/MIME presence detection
- **PGP** — sign/encrypt via rpgp; gnupg keyring or inbx-managed key source; WKD
  public key lookup; Autocrypt 1.1 header harvest; `prefer-encrypt=mutual`
  auto-encrypt on replies
- **Read receipts** — RFC 8098 MDN (Message Disposition Notification) support
- **Search + threading** — SQLite FTS5 over subject / from / to / body, JWZ §2
  threading with mailing-list bracket-tag stripping
- **Import/export** — mbox and single `.eml` with flag round-trip
- **Sync** — IMAP IDLE watch loop, offline outbox queue with exponential
  backoff, Microsoft Graph delta sync
- **Server-side filters** — ManageSieve client (RFC 5804) + vacation responder
  wizard
- **Calendar** — `.ics` invite parsing + METHOD:REPLY for accept/decline; CalDAV
  pull / put / delete / discover (RFC 6764 PROPFIND walk)
- **Mailbox ops** — UID STORE flags (`mark read/unread/star/trash`), UID
  MOVE/COPY (RFC 6851), EXPUNGE, mailbox CRUD, SUBSCRIBE
- **Attachments** — `<Space>a` picker writes the selected part to
  `~/Downloads/`; inbx never opens or executes an attachment
- **Address book** — frecency-ranked, harvest-on-send, CardDAV pull / push /
  discover
- **Templates** — RFC 5322 files under `$XDG_DATA_HOME/inbx/<acct>/templates/`
- **List-Unsubscribe** — RFC 8058 one-click

## CLI

```
inbx config                               # resolved config path + account count

inbx accounts add [--oauth gmail|microsoft]
inbx accounts list
inbx accounts test
inbx accounts folders
inbx accounts edit --imap-port 143 ...
inbx accounts remove [--purge]

inbx fetch [--folder INBOX] [--all] [--bodies] [--notify]
inbx watch [--folder INBOX] [--bodies]    # IDLE loop
inbx list  [--folder INBOX] [--limit 50]
inbx show <uid>
inbx headers <uid>
inbx body <uid>
inbx search <query>
inbx thread <thread-id>

inbx mark {read|unread|star|unstar|trash} --uid 42 43 44
inbx flag --uid 42 --add "\\Seen"
inbx mv --from INBOX --to Archive --uid 42
inbx cp --from INBOX --to Backup --uid 42
inbx expunge

inbx folder create|delete|rename|subscribe NAME

inbx draft new|reply|forward|save
inbx send [--attach PATH]...

inbx template list|save|show|use|remove
inbx contacts list|search|add|harvest|remove
inbx contacts carddav pull|push|discover --url ...
inbx ical show|reply
inbx cal caldav pull|discover --url ...
inbx cal rsvp|put|delete
inbx unsubscribe <uid>
inbx outbox list|drain|remove
inbx export [--output PATH] [--eml --uid N] [--since TS] [--limit N]
inbx import [--folder NAME] [--input PATH] [--eml]

inbx oauth login|set-client|logout
inbx graph folders|fetch|send       # Microsoft 365 — see [accounts.transport]
inbx jmap folders|fetch|send|watch|push  # Fastmail / Stalwart — same
inbx sieve list|get|put|activate|delete|vacation

inbx pgp keygen|list|export|sign|verify|encrypt|decrypt|lookup-wkd

inbx                                  # ratatui TUI (default — no subcommand)
inbx tui                              # equivalent alias
inbx sync [--account NAME] [--bodies] [--notify]  # alias for inbx-sync daemon
inbx completion fish > ~/.config/fish/completions/inbx.fish
```

Per account, `[accounts.transport]` picks which protocol the top-level `fetch` /
`send` / `watch` commands use — `kind = "imap"` (the default), `"graph"`, or
`"jmap"` with a `session_url`. The `graph` and `jmap` subcommands drive those
backends directly regardless of the setting.

## Install

**macOS (Homebrew)**

```bash
brew install kryptic-sh/tap/inbx
```

**Arch Linux (AUR)**

```bash
yay -S inbx-bin
```

**Alpine Linux**

```bash
apk add --allow-untrusted inbx-*.apk   # download .apk from releases page
```

**Pre-built binaries**

Grab the tarball for your platform from the
[releases page](https://github.com/kryptic-sh/inbx/releases).

## Features

| Feature       | Crates                | Description                                                                                                            |
| ------------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `tree-sitter` | `inbx`, `inbx-render` | Syntax highlighting for `text/x-patch` / `text/x-diff` bodies via `hjkl-bonsai`. Grammars loaded on demand at runtime. |

Enable with:

```bash
cargo build --features tree-sitter
```

## Build

```
cargo build --workspace
cargo test --workspace --all-features
```

## Theme

`$XDG_CONFIG_HOME/inbx/theme.toml` — RGB triples for focused border, unfocused
border, status bg/fg, unread accent, highlight. Partial overrides fall back to a
built-in dark palette.

## hjkl tracking

The composer is built on [hjkl-editor](https://github.com/kryptic-sh/hjkl)
`runtime::*`.

See [PLAN.md](PLAN.md) for full design.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) or open an issue / PR.

## License

MIT
