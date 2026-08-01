# Threat Model — inbx

Terse reference. Covers data inbx owns and the threats the current codebase does
— and does not — mitigate. Read alongside `SECURITY.md`.

---

## Scope

**In scope:** data written or read by any inbx binary on the local machine.

**Out of scope:** server-side storage (IMAP / JMAP host), network-layer attacks
(BGP hijack, DNS poisoning, CA compromise), physical hardware attacks.

---

## Assets

| Asset                     | Location                                                  | Sensitivity                         |
| ------------------------- | --------------------------------------------------------- | ----------------------------------- |
| Maildir messages          | `~/.local/share/inbx/<acct>/<folder>/{cur,new,tmp}/`      | high                                |
| SQLite index + FTS5       | `~/.local/share/inbx/<acct>/index.sqlite`                 | high                                |
| Contacts + pubkeys        | `~/.local/share/inbx/<acct>/contacts.sqlite`              | high                                |
| Cached CalDAV events      | `~/.local/share/inbx/<acct>/calendar/<uid>.ics`           | medium                              |
| Log files                 | `~/.local/share/inbx/log/inbx.YYYY-MM-DD`                 | medium                              |
| inbx-managed PGP keys     | `~/.local/share/inbx/<acct>/pgp/<fpr>.{pub,sec}.asc`      | critical                            |
| gnupg keys                | `~/.gnupg/`                                               | critical (managed by gpg, not inbx) |
| Sync daemon IPC socket    | `$XDG_RUNTIME_DIR/inbx-sync.sock` (`$TMPDIR` on macOS)    | medium                              |
| OAuth2 refresh tokens     | OS keyring only — never on disk                           | critical                            |
| App passwords             | OS keyring only — never on disk                           | critical                            |
| Config (hosts, usernames) | `~/.config/inbx/config.toml`, `~/.config/inbx/theme.toml` | medium                              |

Folder names map to directories with `/` replaced by `.` (Maildir++), so
`INBOX/Work` lands at `<acct>/INBOX.Work/`.

**Mode note.** `inbx-store` creates the data directory with `create_dir_all` but
does not explicitly `chmod` the SQLite files. File mode is set by the process
umask (typically `0600` or `0640` depending on user config). The same applies to
Maildir files, cached `.ics` events, log files, and `inbx-managed` key files —
users who need hard `0600` should set `umask 0077` in their shell profile. The
two exceptions that do set an explicit mode: the sync daemon's IPC socket is
chmod `0600`, and `inbx-pgp::gnupg` chmods the **throw-away** temp homedirs it
builds for verify / import to `0o700`. inbx never chmods the user's real
`~/.gnupg` — that stays gpg's responsibility.

---

## Threats Considered

### 1. Lost / stolen device — disk encryption OFF

**Scenario.** Attacker has physical access; boots from external media.

**Impact.** Maildir and SQLite indexes are plaintext. Attacker reads all mail,
contacts, and any pubkeys stored in the contacts DB. OAuth/app-password tokens
are protected only by the OS keyring daemon (which may be unlocked on a live
session or trivially bypassed at rest if the keyring is backed by a plaintext
file on an unencrypted volume).

**Mitigation.** **None from inbx.** inbx does **not** encrypt anything at rest.
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

- `inbx-net::imap` uses `rustls` with `webpki-roots`; rejects invalid certs.
  There is no plaintext mode. STARTTLS is an explicit per-account setting and
  hard-fails (`Error::StarttlsUnsupported`) when the server does not advertise
  the capability — it never falls through to an unencrypted session.
- ManageSieve client (`inbx-net::sieve`) connects over implicit TLS to
  port 4190. Same `rustls` stack.
- OAuth2 codes and refresh tokens are exchanged only over the provider's HTTPS
  token endpoint (`oauth2.googleapis.com/token`,
  `login.microsoftonline.com/{tenant}/oauth2/v2.0/token`). Stored in the OS
  keyring; never written to disk plaintext.

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

- HTML is sanitized via `ammonia` and converted to text; every render call site
  passes `RemotePolicy::Block`, so no external resource is ever fetched. There
  is no per-sender allow-list — remote content cannot currently be enabled.
- Tracking pixels (1x1 images, known beacon hosts) are detected while remote
  images are blocked, and reported to the user.
- Attachments are written to `~/Downloads/` on an explicit keystroke and never
  opened or executed — inbx spawns no handler for them. Content sniffing is
  **not** performed anywhere; the composer guesses outgoing content types from
  the filename extension.
- Calendar invites require explicit Accept/Tentative/Decline; no auto-response.
- Read receipts require explicit `Y` keystroke; never sent automatically.
- Phishing heuristics (display-name / domain mismatch) flagged on render.

The MIME filename is attacker-chosen, so only its final component is used:
`Path::join` discards its base when handed an absolute path and walks upward
through `..`, and a name with nothing usable left falls back to a fixed one.
Pinned by `attachment_file_name_never_escapes_its_directory`, which asserts the
joined path stays under the base and gains exactly one component.

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

inbx itself does not depend on `zeroize` and does not scrub passphrases, key
bytes, or decrypted message buffers it holds. The pinned `pgp` (rpgp) release
does derive `ZeroizeOnDrop` on its plain secret-key params, so key material
inside rpgp's own types is cleared; anything inbx copies out of them is not.
Tracked as future work.

### Process-level isolation

No seccomp / Landlock / AppArmor profile today. Everything runs as the user
process. Future: apply a restrictive seccomp profile to the rendering crate
(`inbx-render`) which handles the highest-risk untrusted input.

---

## References

- `SECURITY.md` — vulnerability reporting policy, supported versions
- `crates/inbx-net/src/sieve.rs` — ManageSieve TLS connect
- `crates/inbx-pgp/src/gnupg.rs` — throw-away gpg homedir `0o700` setup
- `crates/inbx-ipc/src/server.rs` — IPC socket chmod `0600`
- `apps/inbx/src/tui/app.rs` — `save_attachment` (writes to `~/Downloads/`)
- `crates/inbx-render/src/phishing.rs` — phishing heuristics
- `crates/inbx-render/src/auth.rs` — DKIM/SPF/DMARC badge

[RFC 5804]: https://www.rfc-editor.org/rfc/rfc5804
[RFC 8098]: https://www.rfc-editor.org/rfc/rfc8098
[Autocrypt 1.1]: https://autocrypt.org/level1.html
