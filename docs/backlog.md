# Backlog

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
