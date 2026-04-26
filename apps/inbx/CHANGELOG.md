# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1](https://github.com/kryptic-sh/inbx/releases/tag/inbx-v0.0.1) - 2026-04-26

### Added

- CardDAV PROPFIND auto-discovery (RFC 6764)
- --since DAYS for fetch (UID SEARCH SINCE windowing)
- file logging + autoconfig integration in accounts add
- sieve XOAUTH2, autoconfig, TUI body scroll + move picker, release-plz
- TUI ? help overlay
- local flag mirror + TUI message-pane mutation keys
- TUI composer cursor + README refresh
- accounts edit + fetch --all
- pretty dates + show/headers/body subcommands
- shell completion + per-folder watch
- mark sugar + accounts remove + watch auto-drain
- EXPUNGE / UID MOVE+COPY, accounts test, per-folder fetch
- IMAP flag mutations + folder CRUD
- composer attachments, JMAP OAuth, Graph delta, theme config
- M24 per-account templates + crates.io hjkl pin
- M7 drafts sync (server APPEND with \Draft)
- M6b TUI composer overlay
- M6 inbx-composer on hjkl-editor runtime + draft CLI
- M21 minimal JMAP client (Fastmail / Stalwart)
- M23 CardDAV addressbook sync
- M13 IMAP IDLE watch loop + offline outbox queue
- M22 PGP/S/MIME signature + encryption detection
- M17 ManageSieve client + vacation responder
- M15 Authentication-Results badge + phishing heuristics
- M18 desktop notifications on new mail
- M19 mbox + .eml import / export
- M16 List-Unsubscribe + RFC 8058 one-click
- M14 calendar invite parse + RSVP
- M12 SQLite FTS5 search + thread resolution
- M11 Microsoft Graph backend
- M9 Gmail OAuth2 (XOAUTH2) — also lays MS scaffolding
- M8 contacts crate + autocomplete + harvest
- M5 HTML render with sanitize + remote-content gate
- M4 TUI read-only panes + body fetch
- M3 SMTP send + Sent folder append
- M2 IMAP fetch, folder discovery, local SQLite index

### Other

- rename OAuth identifiers to Oauth (CLI now `inbx oauth`)
- scaffold inbx workspace (M1)
