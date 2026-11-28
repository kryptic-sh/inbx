# Backlog

## CI

- **Add a cargo-audit job to CI.** (2026-11-28.) The owner wants every
  kryptic-sh repo we own to run both cargo-deny and cargo-audit in CI. This one
  runs cargo-deny (`.github/workflows/ci.yml`) but has no cargo-audit job. lyr's
  `.github/workflows/ci.yml` is the model: an `audit` job on `ubuntu-latest`
  that installs `cargo-audit` with `taiki-e/install-action@v2` and runs plain
  `cargo audit`, which fails only on vulnerabilities (unmaintained and yanked
  crates are warnings).

## Dependencies

- **`deny.toml` ignores RUSTSEC-2026-0387 for now.** `crypto-bigint` 0.5 could
  branch on secret values in subtraction, division and Montgomery
  multiplication, and only 0.7.6+ is fixed. `pgp` 0.21.0 (the latest, checked
  2026-10-27) still reaches 0.5 through `elliptic-curve` 0.13 and `cx448`.
  rpgp/rpgp#573 ("Adopt RustCrypto new releases") moves it to `crypto-bigint`
  0.7 and replaces `cx448` with `x448` 0.14, but is blocked on RustCrypto
  releases that are still pre-releases (`x448` 0.14.0-pre, `rsa` 0.10.0-rc from
  git, `ed448-goldilocks`, `eax`, `ocb3`). Declined meanwhile: patching `pgp` to
  that branch, which builds on pre-release crypto and git dependencies and
  cannot be published to crates.io. Once a `pgp` release includes it, bump
  `pgp`, fix its API changes and drop the ignore.

- **`rand` stays on 0.8 until `pgp` moves.** `inbx-pgp` hands a
  `rand::thread_rng()` to `pgp`'s key builder (`inbx_managed.rs`), which wants a
  `rand_core` 0.6 `CryptoRng`. Dependabot's rand 0.9 bump (inbx#44) fails clippy
  and tests on exactly that (checked 2026-10-27), and both the resolved `pgp`
  0.19.0 and the latest 0.21.1 (checked 2026-11-17) still depend on `rand` 0.8.
  Bump `rand` together with the `pgp` release that leaves 0.8, likely the one
  carrying rpgp/rpgp#573.

- **The `hjkl-*` crates stay on 0.43 until inbx decides how to read keys.** From
  0.46.0 hjkl's TUI crates take ttyr's `KeyEvent` instead of crossterm's:
  `hjkl_engine_tui::crossterm_to_input` became `ttyr_to_input(&KeyEvent)`, and
  `hjkl_vim_tui::handle_key` and `hjkl_picker_tui::handle_key` take a ttyr
  `&KeyEvent`. inbx reads every terminal event through crossterm's `EventStream`
  (`apps/inbx/src/tui/mod.rs`, `splash.rs`), so with only the 0.46 bump
  `apps/inbx/src/tui/keys.rs` fails to build: the header-field and wizard
  `crossterm_to_input` calls, the composer body's `hjkl_vim_tui::handle_key` and
  the six `hjkl_picker_tui::handle_key` arms in `handle_active_picker_key`
  (checked 2026-11-17). hjkl re-exports only `ttyr::event::KeyEvent` (from
  `hjkl-engine-tui`), not `KeyCode` or `KeyModifiers`, and ttyr's `KeyEvent` is
  `#[non_exhaustive]` and built with `KeyEvent::new(KeyCode, KeyModifiers)`, so
  the re-export alone cannot turn a crossterm key into a ttyr one. Two ways
  forward, the owner's call:
  - **Add `ttyr` 0.3 as a direct dependency of `apps/inbx`.** Either convert at
    the boundary (a crossterm-to-ttyr `KeyEvent` function in `keys.rs` feeding
    the three hjkl entry points), or move the TUI's input and drawing to ttyr as
    hjkl and tmxr (commit 00294bf) did, dropping `crossterm` and enabling
    ratatui's backend some other way. The second replaces `EventStream`, raw
    mode and mouse capture in `mod.rs` and `splash.rs`, and `binds.rs`'s key
    types.
  - **No new dependency: skip hjkl's `-tui` key adapters.** Each is a thin
    wrapper over a toolkit-agnostic API that inbx already reaches: convert
    crossterm keys to `hjkl_engine::Input` in inbx (0.43's `crossterm_to_input`,
    about twenty lines), call `hjkl_vim::dispatch_input` plus
    `Editor::emit_cursor_shape_if_changed` for the body, and route picker keys
    to `Picker::cancel`, `accept`, `select_next`, `select_prev`,
    `handle_source_key` and `handle_query_input` as
    `hjkl_picker_tui::handle_key` does. `apps/inbx` then lists `hjkl-vim` and
    `hjkl-engine` (already workspace dependencies) and may drop `hjkl-vim-tui`,
    `hjkl-picker-tui` and `hjkl-engine-tui`. The cost is a copy of hjkl's picker
    key routing that can drift from upstream. Dependabot's per-crate 0.44 PRs
    (inbx#82, #83, #84, #86) cannot go green on their own either: the hjkl
    crates move in lockstep, so each leaves two hjkl versions in the graph. Move
    every `hjkl-*` entry in one change
    (`cargo upgrade --incompatible -p hjkl-<crate>@0.46` for each), then close
    those PRs.

- **`deny.toml` ignores RUSTSEC-2026-0459 and RUSTSEC-2026-0441 for now.** -0459
  is the SQLite page-cache bug (CVE-2026-119375) in the SQLite that
  `libsqlite3-sys` bundles; only `libsqlite3-sys` 0.39 fixes it, and both the
  resolved `sqlx` 0.8.6 and the latest `sqlx` 0.9.0 require `libsqlite3-sys`
  below 0.38 (checked 2026-11-17). It needs a crafted database plus `-wal` pair,
  and inbx opens only its own files under its data directory. Once an `sqlx`
  release allows 0.39, update it (0.9 is a breaking upgrade) and drop the
  ignore. -0441 marks `pin-utils` unmaintained, no vulnerability; it comes only
  through `async-imap`, whose latest release, 0.12.0, still depends on it.

- **`deny.toml` ignores RUSTSEC-2026-0370, -0371 and -0372 for now.** They flag
  `zvariant`, `zbus_names` and `zvariant_derive` as unmaintained, because zbus 6
  merged them in; none is a vulnerability. inbx gets them only through
  `notify-rust` (used by `inbx` and `inbx-sync` for desktop notifications),
  whose latest release, 4.18.2, still requires `zbus ^5`. Once a `notify-rust`
  release moves to zbus 6, update it and drop the three ignores.

## Coverage gaps

- **No test types into the composer through the TUI.** `handle_composer_key`
  (`apps/inbx/src/tui/keys.rs`) passes body keys to `hjkl_vim_tui::handle_key`,
  but nothing builds an `App` in tests (`App::new` needs an account and a
  store), so replacing that call with a no-op left every test green (checked
  2026-10-27). `body_editor_runs_vim_keys` in `inbx-composer` covers the editor
  itself: building it without the vim discipline makes
  `hjkl_vim::dispatch_input` panic.
