# Threat Model — inbx v0.1.x

Terse reference. Covers data inbx owns and the threats the current codebase does
— and does not — mitigate. Read alongside `SECURITY.md`.

---

## Scope

**In scope:** data written or read by any inbx binary on the local machine.

**Out of scope:** server-side storage (IMAP / JMAP host), network-layer attacks
(BGP hijack, DNS poisoning, CA compromise), physical hardware attacks.

---

## Assets

| Asset                     | Location                                     | Sensitivity                         |
| ------------------------- | -------------------------------------------- | ----------------------------------- |
| Maildir messages          | `~/.local/share/inbx/<acct>/Maildir/`        | high                                |
| SQLite FTS index          | `~/.local/share/inbx/<acct>/messages.sqlite` | high                                |
| Contacts + pubkeys        | `~/.local/share/inbx/<acct>/contacts.sqlite` | high                                |
| inbx-managed PGP keys     | `~/.local/share/inbx/<acct>/pgp/*.asc`       | critical                            |
| gnupg keys                | `~/.gnupg/`                                  | critical (managed by gpg, not inbx) |
| OAuth2 refresh tokens     | OS keyring only — never on disk              | critical                            |
| App passwords             | OS keyring only — never on disk              | critical                            |
| Config (hosts, usernames) | `~/.config/inbx/config.toml`                 | medium                              |

**Mode note.** `inbx-store` creates the data directory with `create_dir_all` but
does not explicitly `chmod` the SQLite files. File mode is set by the process
umask (typically `0600` or `0640` depending on user config). `inbx-pgp::gnupg`
sets gpg homedir to `0o700` on creation. `inbx-managed` key files inherit umask;
users who need hard `0600` should set `umask 0077` in their shell profile.

---

## Threats Considered

### 1. Lost / stolen device — disk encryption OFF

**Scenario.** Attacker has physical access; boots from external media.

**Impact.** Maildir and SQLite indexes are plaintext. Attacker reads all mail,
contacts, and any pubkeys stored in the contacts DB. OAuth/app-password tokens
are protected only by the OS keyring daemon (which may be unlocked on a live
session or trivially bypassed at rest if the keyring is backed by a plaintext
file on an unencrypted volume).

**Mitigation.** **None from inbx.** inbx does **not** encrypt at rest in v0.1.x.
Rely on full-disk encryption (LUKS on Linux, FileVault on macOS, BitLocker on
Windows). This is the primary residual risk acknowledged by this model.

---

### 2. Lost / stolen device — disk encryption ON, device locked

**Scenario.** Device lost; FDE passphrase not compromised.

**Impact.** Maildir, SQLite, and key files are ciphertext on disk. OS keyring
entries are protected by the login keychain, itself encrypted by the FDE layer.

**Mitigation.** FDE handles this. inbx data is protected to the same level as
any other user file. No additional action required.

---

### 3. Multi-user system — shared OS account

**Scenario.** Two users share one Unix login (rare but real in embedded / lab
setups).

**Impact.** Both users can read Maildir and SQLite without any privilege
escalation.

**Mitigation.** Don't share OS accounts. inbx makes no special attempt to
restrict intra-user access beyond what the filesystem provides. File modes
follow umask; see Mode note above.

---

### 4. Multi-user system — separate OS accounts

**Scenario.** Normal multi-user Linux. Attacker is a different `uid`.

**Impact.** Maildir and SQLite are readable only by the owning uid (standard
filesystem DAC). OS keyring is per-user (`libsecret` / `keychain` / `kwallet`).

**Mitigation.** Standard Unix DAC. inbx does not require any special privilege
beyond the user's own files.

---

### 5. Network attacker

**Scenario.** MITM on the wire path to IMAP / SMTP / JMAP / ManageSieve.

**Impact.** Could read or inject email if TLS is stripped or downgraded.

**Mitigation.**

- `inbx-net::imap` uses `rustls` with `webpki-roots`; rejects invalid certs. No
  plaintext fallback, no STARTTLS-downgrade path in the current handshake.
- ManageSieve client (`inbx-net::sieve`) connects over implicit TLS to
  port 4190. Same `rustls` stack.
- OAuth2 refresh tokens are exchanged only over HTTPS with the provider's JWKS
  endpoint. Stored in the OS keyring; never written to disk plaintext.

---

### 6. Malware running as the user

**Scenario.** Arbitrary code running with the user's `uid`.

**Impact.** Full read access to Maildir, SQLite, key files, and keyring. This is
the same as threat 3 above — no intra-user boundary exists.

**Mitigation.** Out of scope. No user-space application can defend against
attacker code running with the same uid without kernel-enforced isolation.
Future: seccomp / Landlock profiles for the rendering crate may limit blast
radius.

---

### 7. Malicious email content

**Scenario.** Attacker sends crafted HTML, attachments, or calendar invites.

**Impact.** Potential for phishing, tracking, or unsafe attachment execution.

**Mitigation.**

- HTML is sanitized; no external resources loaded by default.
- Tracking pixels detected and stripped; user notified.
- Attachments opened only via explicit user action (`xdg-open`). MIME type
  sniffed; extension never trusted.
- Calendar invites require explicit Accept/Tentative/Decline; no auto-response.
- Read receipts require explicit `Y` keystroke; never sent automatically.
- Phishing heuristics (display-name / domain mismatch) flagged on render.

---

## Deferred / Not Implemented

### Per-account at-rest encryption of Maildir and SQLite

Would require a key-derivation layer (passphrase → Argon2 → AES-GCM-SIV) applied
per-page (SQLite) or per-message (Maildir). Cost: query latency on every fetch,
passphrase prompt at startup or on each sync, key-management UX surface that
inbx does not yet have. Decision: defer until a user reports a concrete threat
that FDE does not address. This is the primary known gap.

### Plausible deniability

Hidden volumes, dummy traffic, decoy key material. Out of scope.

### Memory scrubbing

Zeroing PGP secret-key material after use. `pgp` (rpgp) does not expose a
`Zeroize`-impl wrapper for secret key bytes in v0.14.x. Tracked as future work;
not blocking v0.1.x.

### Process-level isolation

No seccomp / Landlock / AppArmor profile today. Everything runs as the user
process. Future: apply a restrictive seccomp profile to the rendering crate
(`inbx-render`) which handles the highest-risk untrusted input.

---

## References

- `SECURITY.md` — vulnerability reporting policy, supported versions
- `crates/inbx-net/src/sieve.rs` — ManageSieve TLS connect
- `crates/inbx-pgp/src/gnupg.rs` — gpg homedir `0o700` setup
- `crates/inbx-render/src/phishing.rs` — phishing heuristics
- `crates/inbx-render/src/auth.rs` — DKIM/SPF/DMARC badge

[RFC 5804]: https://www.rfc-editor.org/rfc/rfc5804
[RFC 8098]: https://www.rfc-editor.org/rfc/rfc8098
[Autocrypt 1.1]: https://autocrypt.org/level1.html
