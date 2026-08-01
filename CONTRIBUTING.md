# Contributing to inbx

Thanks for considering a contribution. inbx is pre-1.0 and the trait surface is
still in motion — please open an issue before starting any non-trivial PR so the
design can be sanity-checked early.

## Development setup

```bash
git clone git@github.com:kryptic-sh/inbx.git
cd inbx
cargo test --workspace    # rust-toolchain.toml pins the toolchain for you
```

`rust-toolchain.toml` pins an exact channel (with `rustfmt` + `clippy`), so
rustup installs the right compiler on the first cargo invocation. Linux builds
need `libdbus-1-dev` + `pkg-config` for the keyring backend.

## Workspace layout

- `crates/inbx-net` — IMAP / SMTP / JMAP / MS-Graph / OAuth2 / ManageSieve
- `crates/inbx-store` — Maildir + SQLite index, FTS5 search, outbox, threading
- `crates/inbx-config` — config loading (no auto-write defaults), theme, keyring
- `crates/inbx-pgp` — gnupg shell-out + rpgp key sources, WKD, PGP/MIME
- `crates/inbx-render` — HTML sanitisation, text rendering, auth + phishing
- `crates/inbx-dav` — shared CalDAV/CardDAV PROPFIND + XML scrape helpers
- `crates/inbx-contacts` — address book
- `crates/inbx-ical` — calendar attachment handling
- `crates/inbx-composer` — compose and send pipeline
- `crates/inbx-ipc` — unix-socket event channel between the daemon and the TUI
- `crates/inbx-sync` — sync engine library (IDLE loop, outbox drain)
- `apps/inbx` — CLI + TUI binary
- `apps/inbx-sync` — background sync daemon
- `xtask` — cargo-xtask stub; no tasks are defined yet

There is no `inbx-core` crate. It was planned, ended up empty, and was dropped
at v0.1.3 — see the note in `PLAN.md`.

## MSRV policy

`rust-version` in `Cargo.toml` tracks current stable Rust. Floor, not ceiling —
bumps land freely when new features are useful. Any bump must be logged in
`CHANGELOG.md` under the version that introduces it.

CI (`.github/workflows/ci.yml`) runs `rustfmt`, `clippy`, `cargo deny check`,
and the test suite on every PR and every push to `main`. Tests run on
ubuntu-latest, macos-latest, and windows-latest, on stable only — there is no
beta or nightly job. Release artifacts (binaries, `.apk`, AUR, Homebrew) build
only on `v*` tags.

## Pull requests

- Branch from `main`. One logical change per PR.
- Commits: [Conventional Commits](https://www.conventionalcommits.org/) format.
  `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`, `build`.
  Scope optional.
- Run before pushing:
  - `cargo fmt --all`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --workspace --all-features`

  CI runs the same three, except that the test job uses `cargo nextest run` plus
  a separate `cargo test --doc` pass.

- New public API needs rustdoc and (where applicable) a `///` example. No crate
  enables `#![deny(missing_docs)]` today, so this is convention, not a lint.

## Benchmarks

`crates/inbx-store` has a criterion bench (`benches/store.rs`) covering cold
start, folder switch, FTS5 search, and JWZ threader ingest:

```bash
cargo bench -p inbx-store
```

Budgets and the last measured numbers are in
[`docs/perf-budgets.md`](docs/perf-budgets.md); the targets they track are in
`PLAN.md` "Performance Budgets". CI does **not** run benches — regressions are
caught by running them locally.

There are currently no `insta` snapshot tests, no proptest suites, and no
`cargo fuzz` harnesses in this repo. `PLAN.md` "Testing" lists those as intent,
not as something you can run today.

## Releases

Everything lives in one workspace — a single version in the root `Cargo.toml`
covers every crate, and nothing is published to crates.io.

Cutting a release is the **BCTP** flow: bump the version in `Cargo.toml`,
regenerate `Cargo.lock`, commit `chore: bump version`, tag `vX.Y.Z`, push commit
plus tag. The tag triggers the release jobs in `.github/workflows/ci.yml`, which
verify the tag matches the manifest version, build binaries for
`x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`,
`x86_64-pc-windows-msvc` and `aarch64-apple-darwin`, upload archives plus sha256
sidecars to the GitHub release, then publish the AUR `inbx-bin` package, an
Alpine `.apk`, and the Homebrew tap formula.

Patch for bug fixes / docs; minor for additive public API; major for breaking
changes.

Since nothing ships to crates.io there is no `cargo yank` path. To withdraw a
broken release, delete or mark the GitHub release and ship a fixed tag; note the
reason in `CHANGELOG.md` under the affected version.

## Reporting bugs / requesting features

Open a GitHub issue — there are no issue templates in this repo. For security
issues, see `SECURITY.md` — do not file public issues.

## Code of Conduct

This project follows the
[kryptic-sh organisation Code of Conduct](https://github.com/kryptic-sh/.github).
