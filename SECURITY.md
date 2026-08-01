# Security Policy

## Supported versions

inbx is pre-1.0. Only the latest release receives security fixes — see the
[releases page](https://github.com/kryptic-sh/inbx/releases). Older tags are
best-effort. There is no long-term support branch.

## Reporting a vulnerability

**Do not open a public GitHub issue for security reports.**

Email `mxaddict@kryptic.sh` with:

- Affected crate(s) and version(s)
- Description of the issue and impact
- Reproduction steps or proof-of-concept
- Disclosure timeline preference

Acknowledgment within 72 hours. Coordinated disclosure window is typically 30
days from acknowledgment, extendable for complex issues.

## Threat model highlights

inbx handles untrusted remote content (email) and long-lived credentials. Key
design decisions:

- **Deny remote content** — every call site passes `RemotePolicy::Block`, so
  HTML mail loads no external resources: remote `<img src>` is rewritten to a
  `data:` placeholder. `RemotePolicy::Allow` exists in the library but no binary
  ever selects it — there is no per-sender allow-list today.
- **Tracking pixel report** — 1x1 images and known beacon hosts are collected
  while remote images are blocked, and surfaced to the user alongside the count
  of blocked images.
- **TLS hard-fail** — rustls + webpki roots. `TlsMode` has exactly two variants,
  `tls` and `starttls`; there is no plaintext mode and STARTTLS aborts rather
  than falling back when the server does not advertise the capability.
- **Keyring-only tokens** — OAuth2 / app-password tokens stored in the OS
  keyring, never written to disk in plaintext. No call site logs a token; note
  that this is a convention, not an enforced redaction layer.
- **DKIM / SPF / DMARC display** — inbx runs no DKIM/SPF crypto itself. It reads
  the verdicts the receiving MTA stamped into `Authentication-Results`
  (RFC 8601) and surfaces them as a badge; failed checks are flagged.
- **Phishing heuristics** — display-name / domain mismatch warnings on render.
- **Attachments are never opened or executed** — the TUI attachment picker
  writes the selected part to `~/Downloads/` on an explicit keystroke and stops
  there. inbx spawns no handler for it. The only process inbx ever spawns are
  `xdg-open` / `open` for a List-Unsubscribe `https:` URL, and `gpg` when the
  account's PGP key source is `gnupg`. Note: outgoing attachment content types
  are guessed from the filename extension — inbx does no content sniffing.
- **PGP** — sign and encrypt via `pgp` (rpgp, pure Rust); no `sequoia-openpgp`
  and no C crypto dependency. Two key sources per account: shell-out to the
  system `gpg` keyring, or an inbx-managed armored keypair with its passphrase
  in the OS keyring. **S/MIME is detection only** — inbx labels an S/MIME signed
  or encrypted part and cannot verify, decrypt, or produce one.
- **Read receipts** — never sent automatically; the user must press `Y` in the
  preview pane for each message.
- **HTML is never rendered as HTML** — the TUI converts to text via `html2text`.
  `inbx-render` also returns a sanitised HTML string for a future GUI shell, but
  no shipped inbx binary renders it in a webview.
- **Encryption at rest** — not implemented. See
  [`docs/threat-model.md`](docs/threat-model.md).

## Dependencies

`cargo deny check` runs in CI on every pull request and every push to `main`
(the `deny` job in `.github/workflows/ci.yml`), covering RUSTSEC advisories and
the license allow-list in `deny.toml`. There is no cron schedule and no
automatic issue filing — a vulnerable dependency fails the build.
