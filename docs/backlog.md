# Backlog

## Dependencies

- **`deny.toml` ignores RUSTSEC-2026-0370, -0371 and -0372 for now.** They flag
  `zvariant`, `zbus_names` and `zvariant_derive` as unmaintained, because zbus 6
  merged them in; none is a vulnerability. inbx gets them only through
  `notify-rust` (used by `inbx` and `inbx-sync` for desktop notifications),
  whose latest release, 4.18.2, still requires `zbus ^5`. Once a `notify-rust`
  release moves to zbus 6, update it and drop the three ignores.
