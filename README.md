# inbx

Modal-vim email client. Rust workspace.

Sibling to [sqeel](https://github.com/kryptic-sh/sqeel),
[buffr](https://github.com/kryptic-sh/buffr),
[hjkl](https://github.com/kryptic-sh/hjkl).

**Status:** pre-MVP. M1 scaffold landed.

## Targets

- Generic IMAP + SMTP (Fastmail, Proton Bridge, dovecot, iCloud, etc.)
- Gmail (OAuth2 XOAUTH2)
- Microsoft Outlook / M365 (OAuth2 + IMAP/SMTP, then MS Graph API)
- JMAP (Fastmail, Stalwart)

## Workspace

```
crates/
  inbx-core       state, models, sync engine
  inbx-net        IMAP / SMTP / JMAP / Graph / OAuth2
  inbx-store      Maildir + SQLite + tantivy
  inbx-config     TOML config + XDG + keyring
  inbx-render     HTML sanitize + remote-content gate
  inbx-contacts   address book + autocomplete
  inbx-ical       calendar invite display + RSVP
  inbx-composer   hjkl-editor wrapper, MIME builder
apps/
  inbx            TUI binary (ratatui)
  inbx-gui        GUI binary (egui) — later
```

See [PLAN.md](PLAN.md) for full design.

## Build

```
cargo build --workspace
cargo test --workspace
```

## License

MIT
