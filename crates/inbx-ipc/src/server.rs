//! Unix-socket broadcast server for the inbx-sync IPC channel.

use std::sync::Arc;

use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::{Event, IpcError};

/// Broadcast server bound to `socket_path()`.
///
/// Each connected TUI client receives every `Event` broadcast via `sender()`.
/// Drop `Server` to close the listener; connected clients see EOF and exit
/// their pump loop gracefully.
pub struct Server {
    /// Kept alive to hold the bound socket open.
    _listener_task: tokio::task::JoinHandle<()>,
    tx: broadcast::Sender<Event>,
}

impl Server {
    /// Bind the IPC socket and start the accept loop.
    ///
    /// Before binding, attempts a quick connect to the same path:
    /// - Connect succeeds → a real daemon is already running; returns `Err`.
    /// - Connect fails and the socket file exists → stale file from a prior
    ///   crash; unlink it before binding so `UnixListener::bind` succeeds.
    pub async fn bind() -> Result<Arc<Self>, IpcError> {
        let path = crate::socket_path();

        // Stale-socket detection: try to connect with a 250 ms timeout.
        // If the connect succeeds, a live daemon is already running — bail out.
        // If it fails but the file exists, it's a stale leftover from a crash;
        // unlink it so we can bind cleanly.
        let connect_result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::net::UnixStream::connect(&path),
        )
        .await;

        match connect_result {
            // Got a connection: real daemon is alive.
            Ok(Ok(_)) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another inbx-sync daemon is already running",
                )));
            }
            // Connect failed (timeout or error) but the file exists → stale socket.
            _ if path.exists() => {
                std::fs::remove_file(&path)?;
            }
            _ => {}
        }

        let listener = UnixListener::bind(&path)?;

        // Set permissions to 0600 so only the owning user can connect.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        let (tx, _) = broadcast::channel::<Event>(64);
        let tx_clone = tx.clone();

        let listener_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let mut rx = tx_clone.subscribe();
                        tokio::spawn(async move {
                            let (_, mut writer) = tokio::io::split(stream);
                            loop {
                                match rx.recv().await {
                                    Ok(event) => {
                                        let mut line = match serde_json::to_string(&event) {
                                            Ok(s) => s,
                                            Err(e) => {
                                                tracing::warn!(%e, "ipc: serialize error");
                                                break;
                                            }
                                        };
                                        line.push('\n');
                                        if writer.write_all(line.as_bytes()).await.is_err() {
                                            // Client disconnected — drop receiver and exit.
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(n, "ipc: client lagged; skipping events");
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::debug!(%e, "ipc: accept error (listener closed?)");
                        break;
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            _listener_task: listener_task,
            tx,
        }))
    }

    /// Broadcast an event to all connected clients.
    /// Silently drops the event when no clients are connected.
    pub fn send(&self, event: Event) {
        // `send` returns `Err` only when there are no receivers; that's fine.
        let _ = self.tx.send(event);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Abort the accept loop; connected client tasks exit on their next recv.
        self._listener_task.abort();
        // Best-effort cleanup of the socket file.
        let _ = std::fs::remove_file(crate::socket_path());
    }
}
