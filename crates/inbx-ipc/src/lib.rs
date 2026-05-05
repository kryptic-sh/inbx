//! inbx-ipc — unix-socket IPC between `inbx-sync` daemon and `inbx` TUI.
//!
//! # Wire format
//! JSON-lines: one `Event` value per line, newline-terminated.
//!
//! # Platform gating
//! The `Server` and `Client` types are only present on unix platforms
//! (`#[cfg(unix)]`). Non-unix builds compile the `Event` enum and
//! `socket_path()` but get no-op stubs so call sites compile without
//! `#[cfg]` noise at each use.

mod event;
mod path;

pub use event::Event;
pub use path::socket_path;

#[cfg(unix)]
mod client;
#[cfg(unix)]
mod server;

#[cfg(unix)]
pub use client::Client;
#[cfg(unix)]
pub use server::Server;

/// Stub used on non-unix targets so call sites compile without `#[cfg]` guards.
#[cfg(not(unix))]
pub struct Server;

#[cfg(not(unix))]
impl Server {
    /// Always returns an error on non-unix targets.
    pub async fn bind() -> Result<Self, crate::IpcError> {
        Err(IpcError::NotSupported)
    }

    /// No-op on non-unix; the broadcast sender is never created.
    pub fn sender(&self) -> NullSender {
        NullSender
    }
}

/// Stub sender for non-unix builds.
#[cfg(not(unix))]
pub struct NullSender;

#[cfg(not(unix))]
impl NullSender {
    pub fn send(&self, _: Event) {}
}

/// Stub used on non-unix targets so call sites compile without `#[cfg]` guards.
#[cfg(not(unix))]
pub struct Client;

#[cfg(not(unix))]
impl Client {
    /// Always returns an error on non-unix targets.
    pub async fn connect() -> Result<Self, crate::IpcError> {
        Err(IpcError::NotSupported)
    }

    /// Returns a receiver that is immediately closed on non-unix.
    pub fn receiver(&mut self) -> tokio::sync::mpsc::Receiver<Event> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        rx
    }
}

/// Errors from the IPC layer.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("IPC not supported on this platform")]
    NotSupported,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
