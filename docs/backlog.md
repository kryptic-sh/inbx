# Backlog

## Dependencies

- **`deny.toml` ignores RUSTSEC-2026-0370, -0371 and -0372 for now.** They flag
  `zvariant`, `zbus_names` and `zvariant_derive` as unmaintained, because zbus 6
  merged them in; none is a vulnerability. inbx gets them only through
  `notify-rust` (used by `inbx` and `inbx-sync` for desktop notifications),
  whose latest release, 4.18.2, still requires `zbus ^5`. Once a `notify-rust`
  release moves to zbus 6, update it and drop the three ignores.

## CI

- **The `clippy` job is red, unrelated to the dependency updates.** Not fixed
  yet:
  - `clippy::double_must_use` (CI's toolchain): the `#[async_trait]` expansion
    on the `PubkeyLookup` trait in `inbx-pgp`.
  - `clippy::unused_io_amount` (local `cargo clippy --workspace --all-targets`
    on the pinned 1.95.0): the bare `stream.read(&mut request)` in the
    `raw_blob_download_rejects_non_success_status` test in `inbx-net`'s
    `jmap` module.
  - `clippy::assertions_on_constants`: `assert!(MAX_RESPONSE_LINE_BYTES > 0)` in
    the `response_limits_are_nonzero_and_literal_bound_is_parseable` test in
    `inbx-net`'s `sieve` module.
