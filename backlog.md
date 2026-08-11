# Backlog

## Quality sweep — 2026-08-11

### Cross-cutting resolution summary

The review, audit, tidy, and performance passes converged on the same protocol
and snapshot boundaries. All actionable findings from the four passes were fixed
in this sweep:

- Sync now dispatches through the configured IMAP, JMAP, or Graph provider.
- Authoritative snapshots fail on partial protocol results and are applied in
  one SQLite transaction. Database-issued generations prevent stale, unreserved,
  or replayed snapshots from mutating newer state.
- Opaque JMAP and Graph identities retain their full canonical values. Legacy
  rows are reconciled only when identity is unambiguous, and reconciled mail is
  not reported as new.
- Graph and JMAP pagination validates continuation state, progress, duplicates,
  response coverage, and provider errors before a snapshot is considered
  complete.
- JMAP downloads raw messages only through the session's lossless `downloadUrl`,
  rejects per-object mutation failures, escapes hierarchical mailbox paths
  losslessly, and expunges every deleted-message batch.
- Graph delta processing requires a terminal checkpoint, honors the final
  occurrence of repeated IDs, and removes tombstones without creating ghost
  messages or fetching nonexistent bodies.
- IMAP flags use canonical protocol tokens; local expunge also recognizes legacy
  debug-form `Deleted` tokens without matching keyword substrings.
- JMAP EventSource and ManageSieve parsing now have explicit record, line,
  literal, aggregate-byte, and aggregate-record limits.
- Message deletion removes corresponding FTS rows, including transactional
  snapshot pruning and UIDVALIDITY resets.
- Thread placeholder insertion is batched below SQLite's bind limit, while root
  and cycle checks use bounded recursive SQL behavior with explicit errors.
- TLS client configuration is shared, and duplicate Graph/JMAP HTTP error
  handling is consolidated.
- Body indexing completes before a row receives its Maildir path. Graph delta
  state is persisted only after all associated message processing succeeds.

No actionable sweep finding remains open.

### Design decisions

- A complete provider snapshot is authoritative only after the complete fetch
  and validation succeed. Filtered or delta results are explicitly incomplete
  and never prune absent local rows.
- Each folder fetch reserves a database generation before network I/O. Only the
  latest issued, not-yet-applied positive generation may apply. Failed fetches
  leave a harmless reserved generation; later reservations supersede it.
- Ambiguous legacy rows with no provider ID are preserved rather than guessed.
  Message-ID reconciliation requires uniqueness in both the incoming snapshot
  and legacy store; canonical and compatibility UID matches must also resolve to
  one row.
- JMAP EventSource records are limited to 1 MiB.
- Graph pagination is limited to 10,000 pages, and Graph `$top` is clamped to
  the service range `1..=1000`.
- ManageSieve limits are 64 KiB per response line, 16 MiB per literal, 32 MiB
  per aggregate response, and 10,000 response records. Post-literal delimiters
  count toward these limits and must be empty.
- Thread placeholder batches remain below SQLite's 32,766-bind ceiling.

### Verification

The workspace CI gate passed after the final fixes:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo build --workspace --all-features`
- `cargo nextest run --workspace --all-features --no-fail-fast`

New mechanism tests were demonstrated red before restoration for stream error
propagation, protocol bounds, pagination, batching, UIDVALIDITY resets, snapshot
atomicity and ordering, generation reservation/replay, opaque identity and
newness, stale-result propagation, lossless JMAP downloads, mutation item
failures, mailbox-path collisions, Graph delta tombstones and checkpoints, IMAP
flag normalization, local expunge matching, and TUI copy validation.

## Review pass

### Findings

1. **High — Graph and JMAP accounts synchronized through IMAP.** The shared sync
   cycle ignored the configured provider transport. Resolved by routing folder
   and message operations through `MailProvider`.
2. **High — partial IMAP fetches could prune healthy cached messages.** Stream
   item errors were converted to omissions before destructive reconciliation.
   Resolved by propagating authoritative stream failures and applying only
   validated snapshots.
3. **Hardening — opaque provider IDs were narrowed.** JMAP and Graph stable IDs
   lost identity bits in local UIDs. Resolved by widening provider-facing UIDs
   and retaining full canonical hashes and provider IDs.

### Cleared

- IMAP STARTTLS rejects tagged `NO` and `BAD` before TLS upgrade; no plaintext
  fallback was found.
- Unix IPC sockets are owner-only after bind.

### Coverage

The pass traced transport dispatch, provider identity, snapshot persistence,
cache pruning, IPC lifecycle, configuration/proxy paths, mbox helpers, and GnuPG
process invocation. The corrected audit below covered the remaining codebase.

## Audit pass

### Findings

1. **High — configured Graph and JMAP accounts synchronized through IMAP.**
   Resolved with provider-aware sync dispatch.
2. **High — partial IMAP results could trigger destructive cache pruning.**
   Resolved with error propagation and atomic, completeness-aware snapshot
   application.
3. **Medium — ManageSieve accepted unbounded server literals and responses.**
   Resolved with line, literal, aggregate-byte, aggregate-record, and delimiter
   validation.
4. **Medium — JMAP EventSource records could grow without bound.** Resolved with
   a bounded incremental parser supporting fragmented CRLF, LF, lone-CR, and
   mixed terminators.

### Cleared

- OAuth callback handling binds to loopback and validates random PKCE and CSRF
  state before code exchange.
- Attachment output reduces message-supplied names to one final path component.
- GnuPG uses argument vectors rather than shell interpolation.
- No cross-user IPC exploit was established for stale-socket recovery in the
  sticky system temporary directory.

### Coverage

The corrected whole-codebase audit covered CLI/config entry points, IMAP, SMTP,
ManageSieve, JMAP, Graph, OAuth, IPC, PGP, attachment writes, rendering,
migrations, package metadata, workflows, tests, TUI, contacts, DAV, calendar,
composer, and store code. It checked injection, resource exhaustion, TLS/auth,
token flow, panic/error handling, paths, and async/IPC behavior.

## Tidy pass

### Findings

1. **Shared TLS setup was duplicated across IMAP and ManageSieve.** Resolved
   with one cached immutable rustls client configuration.
2. **Graph JSON mutation error mapping was duplicated.** Resolved with shared
   non-success response handling.
3. **JMAP non-success response mapping was duplicated.** Resolved with shared
   HTTP validation while retaining request-specific parsing.

### Coverage

The whole-codebase pass reviewed protocol setup, HTTP response handling,
provider wrappers, aliases, allocation/clone sites, and dead-code candidates
across applications, workspace crates, tooling, tests, migrations, and package
files. No safe public-item deletion was identified.

## Performance pass

### Findings

1. **JMAP EventSource delimiter detection was quadratic.** Resolved with
   incremental linear scanning plus the record limit recorded above.
2. **Thread ingestion performed serial SQL round trips for placeholders and
   ancestry walks.** Resolved with placeholder batches and recursive CTE root
   and cycle checks.
3. **TLS root-store construction repeated per connection.** Resolved with the
   shared cached client configuration.

### Coverage

The whole-codebase pass traced sync scheduling and body batches, all mail
protocol paths, store/FTS/threading persistence, rendering, TUI task handling,
and the existing store benchmarks. No benchmark was run; the findings were
structural costs in per-folder, per-body, and per-chunk loops rather than claims
about measured speedups.
