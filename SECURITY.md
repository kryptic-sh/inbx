# Security Policy

## Supported versions

inbx is pre-1.0. Only the latest 0.1.x patch release receives security fixes.
Older 0.x minors are best-effort once 0.2.0 ships.

| Version | Supported |
| ------- | --------- |
| 0.1.x   | yes       |

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

- **Default-deny remote content** — HTML mail loads no external resources unless
  the user explicitly allows a sender.
- **Tracking pixel strip** — remote 1x1 images are detected and removed; a
  report is surfaced to the user.
- **TLS hard-fail** — rustls + webpki roots; no plaintext IMAP/SMTP fallback.
- **Keyring-only tokens** — OAuth2 / app-password tokens stored in the OS
  keyring, never written to disk in plaintext, redacted from logs.
- **DKIM / SPF / DMARC display** — verification results shown as a badge; failed
  checks are prominently flagged.
- **Phishing heuristics** — display-name / domain mismatch warnings on render.
- **No auto-execute attachments** — MIME type is sniffed; file extension is
  never trusted. Attachments open only via explicit user action through
  `xdg-open`.
- **S/MIME + PGP** — sign and encrypt via sequoia-openpgp; keys managed through
  the standard keyring.
- **Read receipts** — never sent automatically; user is prompted each time.
- **Sandbox HTML** — GUI renders sanitised blobs in a webview with scripting
  disabled; TUI is text-only.
- **Encryption at rest** — deferred; threat model will be documented before
  implementation.

## Dependencies

`cargo deny` runs in cron CI checking RUSTSEC advisories. Vulnerable transitive
dependencies trigger an issue automatically.
