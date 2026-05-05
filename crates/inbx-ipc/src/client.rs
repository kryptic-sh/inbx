//! Unix-socket client for the inbx-sync IPC channel.

use tokio::io::AsyncBufReadExt as _;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::{Event, IpcError};

/// Connected IPC client.
///
/// Call `connect()` to establish a connection to the sync daemon. Then call
/// `receiver()` to get a channel on which incoming `Event`s arrive.
/// The channel closes (returns `None`) when the daemon drops the connection.
pub struct Client {
    rx: mpsc::Receiver<Event>,
}

impl Client {
    /// Attempt to connect to the sync daemon with a 500 ms timeout.
    pub async fn connect() -> Result<Self, IpcError> {
        let path = crate::socket_path();
        let stream = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            UnixStream::connect(&path),
        )
        .await
        .map_err(|_| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timeout connecting to inbx-sync socket",
            ))
        })??;

        let (tx, rx) = mpsc::channel::<Event>(64);

        tokio::spawn(async move {
            let reader = tokio::io::BufReader::new(stream);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match serde_json::from_str::<Event>(&line) {
                        Ok(event) => {
                            if tx.send(event).await.is_err() {
                                // TUI dropped the receiver — stop the pump.
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(%e, "ipc client: parse error on line");
                        }
                    },
                    Ok(None) => {
                        tracing::debug!("ipc client: daemon closed connection");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(%e, "ipc client: read error");
                        break;
                    }
                }
            }
        });

        Ok(Self { rx })
    }

    /// Take the event receiver.
    ///
    /// May only be called once; subsequent calls return a closed channel.
    pub fn receiver(&mut self) -> mpsc::Receiver<Event> {
        let (_, empty_rx) = mpsc::channel(1);
        std::mem::replace(&mut self.rx, empty_rx)
    }
}
