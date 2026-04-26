# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1](https://github.com/kryptic-sh/inbx/releases/tag/inbx-net-v0.0.1) - 2026-04-26

### Added

- --since DAYS for fetch (UID SEARCH SINCE windowing)
- sieve XOAUTH2, autoconfig, TUI body scroll + move picker, release-plz
- shell completion + per-folder watch
- EXPUNGE / UID MOVE+COPY, accounts test, per-folder fetch
- IMAP flag mutations + folder CRUD
- composer attachments, JMAP OAuth, Graph delta, theme config
- M7 drafts sync (server APPEND with \Draft)
- M21 minimal JMAP client (Fastmail / Stalwart)
- M13 IMAP IDLE watch loop + offline outbox queue
- M17 ManageSieve client + vacation responder
- M16 List-Unsubscribe + RFC 8058 one-click
- M11 Microsoft Graph backend
- M9 Gmail OAuth2 (XOAUTH2) — also lays MS scaffolding
- M4 TUI read-only panes + body fetch
- M3 SMTP send + Sent folder append
- M2 IMAP fetch, folder discovery, local SQLite index

### Other

- rename OAuth identifiers to Oauth (CLI now `inbx oauth`)
- scaffold inbx workspace (M1)
