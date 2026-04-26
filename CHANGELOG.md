# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Changed

- bump hjkl =0.0.28 — adopts canonical Buffer impl + sticky_col/iskeyword hoist
  (M6 composer migration to spec::* still pending Editor<B,H> generic at 0.1.0).

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
