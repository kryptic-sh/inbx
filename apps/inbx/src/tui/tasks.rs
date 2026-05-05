//! Background-task plumbing for the TUI event loop.
//!
//! Long-running ops (IMAP fetch, SMTP send, ManageSieve connect)
//! must not block the event loop's await chain — that would freeze
//! redraws and stop the busy spinner from animating. Each op spawns
//! onto a tokio task and posts its result back via a shared mpsc
//! channel; the event loop's `tokio::select!` picks results up
//! between (or alongside) key events.

#[cfg(feature = "tree-sitter")]
use std::sync::Arc;

use inbx_net::sieve::SieveScript;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Result of a background op, posted from the spawned task back to App.
pub(super) enum TaskResult {
    /// `manual_sync` finished. Carries the new last_sync_unix and an
    /// optional error string for the status line.
    SyncDone {
        last_sync_unix: Option<i64>,
        error: Option<String>,
        new_messages: usize,
        folder_name: String,
        total_messages: usize,
    },
    /// `fetch_current_body` finished — body is on disk, refresh the preview.
    BodyFetched { uid: i64, error: Option<String> },
    /// `drain_outbox` finished — some N sent, some failed.
    OutboxDrained { sent: usize, failed: usize },
    /// Sieve LISTSCRIPTS returned (or failed).
    SieveScripts(std::result::Result<Vec<SieveScript>, String>),
    /// Sieve GETSCRIPT returned (or failed).
    SieveBody {
        name: String,
        body: std::result::Result<String, String>,
    },
    /// Sieve PUTSCRIPT returned (or failed). On failure we restore the
    /// wizard so the user can retry — name + body travel back too.
    SieveSaved {
        name: String,
        body: String,
        result: std::result::Result<(), String>,
    },
    /// Background watch loop (IMAP IDLE or JMAP EventSource) detected new
    /// data. The event loop should trigger a `manual_sync` on the current
    /// folder. No pending-op counter is decremented — the watch loop is
    /// long-lived and does not participate in the busy/spinner cycle.
    WatchSignal,
    /// An event arrived from the inbx-sync daemon over the IPC socket.
    SyncIpcEvent(inbx_ipc::Event),
    /// A folder CRUD operation (create / rename / delete) completed.
    /// Carries the success message or an error string.
    FolderOp(Result<String, String>),
    /// A tree-sitter grammar finished loading (or failed). The `lang` key
    /// matches the string used for the cache lookup in `App`.
    #[cfg(feature = "tree-sitter")]
    GrammarReady {
        lang: &'static str,
        result: Result<Arc<hjkl_bonsai::runtime::Grammar>, String>,
    },
    /// `oauth_login` finished. `result` carries an error string on failure.
    OAuthLoginDone {
        result: std::result::Result<(), String>,
    },
    /// `send_composer` finished. On success carries the byte count; on failure
    /// carries the error (message was queued in the outbox by the task).
    ComposerSent {
        result: std::result::Result<usize, String>,
    },
    /// `expunge` finished. On success carries (server_count, local_count, folder_name).
    ExpungeDone {
        result: std::result::Result<(usize, u64, String), String>,
    },
    /// `mark_folder_read` finished. Carries `(count, folder_name)` on success.
    MarkFolderReadDone {
        result: std::result::Result<(usize, String), String>,
    },
    /// `unsubscribe_current` finished. Carries a human-readable status string.
    UnsubscribeDone {
        result: std::result::Result<String, String>,
    },
    /// `send_read_receipt` finished. `result` carries the status string.
    ReadReceiptDone {
        result: std::result::Result<String, String>,
    },
}

#[derive(Clone)]
pub(super) struct TaskTx(pub(super) UnboundedSender<TaskResult>);

pub(super) struct TaskRx(pub(super) UnboundedReceiver<TaskResult>);

pub(super) fn channel() -> (TaskTx, TaskRx) {
    let (tx, rx) = unbounded_channel();
    (TaskTx(tx), TaskRx(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_round_trip() {
        let (tx, mut rx) = channel();
        tx.0.send(TaskResult::SyncDone {
            last_sync_unix: Some(1234567890),
            error: None,
            new_messages: 3,
            folder_name: "INBOX".into(),
            total_messages: 42,
        })
        .unwrap();
        let result = rx.0.recv().await.unwrap();
        match result {
            TaskResult::SyncDone {
                last_sync_unix,
                error,
                new_messages,
                folder_name,
                total_messages,
            } => {
                assert_eq!(last_sync_unix, Some(1234567890));
                assert!(error.is_none());
                assert_eq!(new_messages, 3);
                assert_eq!(folder_name, "INBOX");
                assert_eq!(total_messages, 42);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[tokio::test]
    async fn channel_handles_multiple_results() {
        let (tx, mut rx) = channel();
        tx.0.send(TaskResult::OutboxDrained { sent: 1, failed: 0 })
            .unwrap();
        tx.0.send(TaskResult::BodyFetched {
            uid: 42,
            error: None,
        })
        .unwrap();
        tx.0.send(TaskResult::SieveScripts(Err("connect refused".into())))
            .unwrap();

        let r1 = rx.0.recv().await.unwrap();
        let r2 = rx.0.recv().await.unwrap();
        let r3 = rx.0.recv().await.unwrap();

        assert!(matches!(
            r1,
            TaskResult::OutboxDrained { sent: 1, failed: 0 }
        ));
        assert!(matches!(r2, TaskResult::BodyFetched { uid: 42, .. }));
        assert!(matches!(r3, TaskResult::SieveScripts(Err(_))));
    }
}
