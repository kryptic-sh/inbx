//! Socket path resolution for the inbx-sync IPC channel.

use std::path::PathBuf;

/// Returns the path to the unix socket used by `inbx-sync`.
///
/// - Linux: `$XDG_RUNTIME_DIR/inbx-sync.sock` (falls back to `/tmp` if unset).
/// - macOS: `$TMPDIR/inbx-sync.sock` (`$TMPDIR` is always set by launchd).
/// - Other unix: `/tmp/inbx-sync.sock`.
/// - Non-unix: returns an arbitrary path (never actually used; `Server`/`Client`
///   fail immediately on non-unix targets).
pub fn socket_path() -> PathBuf {
    let dir = runtime_dir();
    dir.join("inbx-sync.sock")
}

fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Some(v) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(v);
        }
        PathBuf::from("/tmp")
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(v) = std::env::var_os("TMPDIR") {
            return PathBuf::from(v);
        }
        PathBuf::from("/tmp")
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        PathBuf::from("/tmp")
    }
}
