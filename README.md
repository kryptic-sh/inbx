# inbx

[![CI](https://github.com/kryptic-sh/inbx/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/inbx/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Website](https://img.shields.io/badge/website-inbx.kryptic.sh-7ee787)](https://inbx.kryptic.sh)

Modal-vim email client. Rust workspace.

Sibling to [sqeel](https://github.com/kryptic-sh/sqeel),
[buffr](https://github.com/kryptic-sh/buffr),
[hjkl](https://github.com/kryptic-sh/hjkl).

## Status

Working CLI + TUI + GUI. Real-account dogfood pending.

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
  inbx-core       state, models, sync engine
  inbx-net        IMAP / SMTP / JMAP / Graph / OAuth2 / Sieve / IDLE
  inbx-store      Maildir + SQLite + FTS5 + outbox
  inbx-config     TOML config + XDG + keyring + theme
  inbx-render     HTML sanitize + remote-content gate + auth + PGP
  inbx-contacts   address book + autocomplete + CardDAV
  inbx-ical       calendar invite display + RSVP
  inbx-composer   hjkl-editor wrapper, MIME builder, templates
apps/
  inbx            CLI + TUI binary (ratatui)
  inbx-gui        GUI binary (egui)
```

## Highlights

- **TUI** with vim navigation (j/k, h/l, gg/G, Tab) and a modal composer overlay
  (`c`/`r`/`R`/`f`, Ctrl-S send, Ctrl-D save draft)
- **GUI** (eframe + egui) — read-only three-pane: folders / messages / preview
- **Auth** — app password via OS keyring, OAuth2 (Gmail + Microsoft) with PKCE +
  auth-code loopback flow, refresh tokens stored in the keyring
- **Render** — HTML sanitized via ammonia, remote content blocked by default,
  tracker pixels surfaced, SPF/DKIM/DMARC + phishing heuristics, PGP / S/MIME
  presence detection
- **Search + threading** — SQLite FTS5 over subject / from / to / body,
  In-Reply-To walk for thread grouping
- **Sync** — IMAP IDLE watch loop, offline outbox queue with exponential
  backoff, Microsoft Graph delta sync
- **Server-side filters** — ManageSieve client (RFC 5804) + vacation responder
  generator
- **Calendar** — `.ics` invite parsing + METHOD:REPLY for accept/decline
- **Mailbox ops** — UID STORE flags (`mark read/unread/star/trash`), UID
  MOVE/COPY (RFC 6851), EXPUNGE, mailbox CRUD, SUBSCRIBE
- **Address book** — frecency-ranked, harvest-on-send, CardDAV pull
- **Templates** — RFC 5322 files under `$XDG_DATA_HOME/inbx/<acct>/templates/`
- **List-Unsubscribe** — RFC 8058 one-click

## CLI

```
inbx accounts add [--oauth gmail|microsoft]
inbx accounts test
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
inbx contacts list|search|add|harvest|card-dav --url ...
inbx ical show|reply
inbx unsubscribe <uid>
inbx outbox list|drain|remove

inbx oauth login|set-client|logout
inbx graph folders|fetch|send       # Microsoft 365
inbx jmap folders|fetch|send         # Fastmail / Stalwart
inbx sieve list|get|put|activate|delete|vacation

inbx tui                              # ratatui TUI
inbx-gui                              # egui GUI
inbx completion fish > ~/.config/fish/completions/inbx.fish
```

## Build

```
cargo build --workspace
cargo test --workspace
```

## Theme

`$XDG_CONFIG_HOME/inbx/theme.toml` — RGB triples for focused border, unfocused
border, status bg/fg, unread accent, highlight. Partial overrides fall back to a
built-in dark palette.

## hjkl tracking

The composer is built on [hjkl-editor](https://github.com/kryptic-sh/hjkl)
`runtime::*`. A Claude routine polls hourly for new hjkl releases and opens an
integration PR on this repo when one lands; if 0.1.0 ships its `spec::*` trait
surface, the routine performs the migration in the PR.

See [PLAN.md](PLAN.md) for full design.

## License

MIT
