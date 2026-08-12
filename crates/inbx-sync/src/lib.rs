//! inbx-sync library — reusable sync engine for the inbx workspace.
//!
//! Provides the multi-account IMAP IDLE loop, outbox drain, autocrypt harvest,
//! and (optionally) IPC broadcast. Call [`run`] from the standalone
//! `inbx-sync` binary, the `inbx sync` subcommand, or the TUI's in-process
//! fallback. Logging must be initialised by the caller — this crate never
//! calls any `tracing_subscriber::*::init()`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use inbx_config::Account;
use mail_parser::MessageParser;
use tokio::task::JoinSet;

/// Result of applying one provider folder snapshot to the local store.
#[derive(Debug, Clone)]
pub struct SnapshotApply {
    pub applied: bool,
    pub new_rows: Vec<inbx_net::HeaderRow>,
}

impl SnapshotApply {
    pub fn new_count(&self) -> usize {
        self.new_rows.len()
    }
}

/// Convert the network-facing snapshot into the store-owned protocol-neutral input.
pub async fn apply_snapshot(
    store: &inbx_store::Store,
    folder: &str,
    generation: i64,
    snapshot: inbx_net::provider::HeaderSnapshot,
) -> Result<SnapshotApply> {
    let transport = match snapshot.transport {
        inbx_net::provider::SnapshotTransport::Imap { uidvalidity } => {
            inbx_store::SnapshotTransport::Imap { uidvalidity }
        }
        inbx_net::provider::SnapshotTransport::Opaque => inbx_store::SnapshotTransport::Opaque,
    };
    let rows = snapshot
        .rows
        .into_iter()
        .map(|header| inbx_store::SnapshotHeader {
            uid: header.uid,
            uidvalidity: i64::from(header.uidvalidity),
            message_id: header.message_id,
            subject: header.subject,
            from_addr: header.from_addr,
            to_addrs: header.to_addrs,
            date_unix: header.date_unix,
            flags: header.flags,
            fetched_at_unix: header.fetched_at_unix,
            provider_id: header.provider_id,
        })
        .collect();
    let output = store
        .apply_snapshot(inbx_store::SnapshotInput {
            folder: folder.to_owned(),
            generation,
            complete: snapshot.complete,
            transport,
            rows,
        })
        .await?;
    Ok(SnapshotApply {
        applied: output.applied,
        new_rows: output
            .new_rows
            .into_iter()
            .map(|header| {
                Ok(inbx_net::HeaderRow {
                    uid: header.uid,
                    uidvalidity: u32::try_from(header.uidvalidity).map_err(|_| {
                        anyhow::anyhow!(
                            "store snapshot uidvalidity {} cannot fit u32",
                            header.uidvalidity
                        )
                    })?,
                    message_id: header.message_id,
                    subject: header.subject,
                    from_addr: header.from_addr,
                    to_addrs: header.to_addrs,
                    date_unix: header.date_unix,
                    flags: header.flags,
                    fetched_at_unix: header.fetched_at_unix,
                    provider_id: header.provider_id,
                })
            })
            .collect::<Result<_>>()?,
    })
}

/// Configuration for a sync run.
///
/// All fields are `pub`; no builder needed — just fill in the struct.
pub struct Config {
    /// Accounts to sync. Must be non-empty.
    pub accounts: Vec<Account>,
    /// Bound IPC server. `None` when running in-process (TUI fallback) or on
    /// non-unix platforms. When `Some`, `FolderUpdated` events are broadcast
    /// to connected TUI clients after each cycle.
    pub ipc: Option<Arc<inbx_ipc::Server>>,
    /// In-process receiver for `FolderUpdated` events. TUI fallback configures
    /// this instead of IPC so sync completions still refresh its store view.
    pub local_events: Option<tokio::sync::mpsc::UnboundedSender<inbx_ipc::Event>>,
    /// Whether to fire desktop notifications on new mail. Set to `false` when
    /// spawned in-process from the TUI (the status line already shows new mail).
    pub notifications: bool,
    /// Folder watched via push (IMAP IDLE / JMAP EventSource / Graph delta).
    /// Defaults to `"INBOX"` in callers. Push events trigger immediate re-sync
    /// of all folders.
    pub idle_folder: String,
    /// When non-empty, sync only these folders. Empty = discover all from server.
    pub folders: Vec<String>,
    /// Whether to download message bodies on each fetch cycle.
    pub fetch_bodies: bool,
    /// Cap on bodies fetched per cycle when `fetch_bodies` is true.
    pub body_limit: u32,
    /// Seconds between sync cycles. Push signals also trigger a cycle early.
    pub poll_interval_secs: u64,
}

#[cfg(unix)]
fn emit_folder_updated(
    ipc: Option<&Arc<inbx_ipc::Server>>,
    local_events: Option<&tokio::sync::mpsc::UnboundedSender<inbx_ipc::Event>>,
    account: String,
    folder: String,
    new_count: u32,
) {
    let event = inbx_ipc::Event::FolderUpdated {
        account,
        folder,
        new_count,
    };
    if let Some(ipc) = ipc {
        ipc.send(event.clone());
    }
    if let Some(local_events) = local_events
        && local_events.send(event).is_err()
    {
        tracing::debug!("local sync event receiver closed");
    }
}

#[cfg(not(unix))]
fn emit_folder_updated(
    local_events: Option<&tokio::sync::mpsc::UnboundedSender<inbx_ipc::Event>>,
    account: String,
    folder: String,
    new_count: u32,
) {
    let event = inbx_ipc::Event::FolderUpdated {
        account,
        folder,
        new_count,
    };
    if let Some(local_events) = local_events
        && local_events.send(event).is_err()
    {
        tracing::debug!("local sync event receiver closed");
    }
}

/// Run the multi-account sync loop until Ctrl-C or all tasks exit.
///
/// Does **not** initialise logging — the caller must set up a tracing
/// subscriber before calling this function.
pub async fn run(cfg: Config) -> Result<()> {
    if cfg.accounts.is_empty() {
        anyhow::bail!("no accounts configured; run `inbx accounts add`");
    }

    let idle_folder = Arc::new(cfg.idle_folder);
    let static_folders = Arc::new(cfg.folders);
    let poll_interval_secs = cfg.poll_interval_secs;
    tracing::info!(
        accounts = cfg.accounts.len(),
        idle_folder = %idle_folder,
        "inbx-sync starting"
    );

    // Heartbeat task: every 60s broadcast a Heartbeat so TUI clients can
    // detect a stale/dead daemon.
    #[cfg(unix)]
    if let Some(srv) = cfg.ipc.clone() {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let ts_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                srv.send(inbx_ipc::Event::Heartbeat { ts_unix });
            }
        });
    }

    let mut tasks = JoinSet::new();
    for acct in cfg.accounts {
        let idle_folder = idle_folder.clone();
        let static_folders = static_folders.clone();
        let fetch_bodies = cfg.fetch_bodies;
        let body_limit = cfg.body_limit;
        let notify = cfg.notifications;
        let local_events = cfg.local_events.clone();
        #[cfg(unix)]
        let ipc = cfg.ipc.clone();
        tasks.spawn(async move {
            loop {
                // 1. Determine the folder list for this cycle.
                //    If the caller fixed a set, use it; otherwise discover from
                //    the server by running sync_once on the idle folder first
                //    (which calls list_folders internally and upserts to the
                //    store), then reading back what the store knows.
                let folders: Vec<String> = if !static_folders.is_empty() {
                    static_folders.as_ref().clone()
                } else {
                    // Run a discovery sync on the idle folder. This populates
                    // the store's folders table via inbx_net::list_folders.
                    match sync_once(&acct, &idle_folder, fetch_bodies, body_limit, notify).await {
                        Err(e) => {
                            tracing::warn!(account = %acct.name, %e, "discovery cycle failed; sleeping 30s");
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            continue;
                        }
                        Ok(new_count) => {
                            #[cfg(unix)]
                            emit_folder_updated(
                                ipc.as_ref(),
                                local_events.as_ref(),
                                acct.name.clone(),
                                idle_folder.to_string(),
                                new_count,
                            );
                            #[cfg(not(unix))]
                            emit_folder_updated(
                                local_events.as_ref(),
                                acct.name.clone(),
                                idle_folder.to_string(),
                                new_count,
                            );
                        }
                    }
                    // Read back discovered folders from the store, skipping
                    // \Noselect virtual parents (e.g. Gmail's "[Gmail]").
                    match inbx_store::Store::open(&acct.name).await {
                        Ok(store) => match store.list_folders().await {
                            Ok(rows) => rows
                                .into_iter()
                                .filter(|r| {
                                    !r.attrs
                                        .as_deref()
                                        .is_some_and(|a| a.contains("\\Noselect"))
                                })
                                .map(|r| r.name)
                                .collect(),
                            Err(e) => {
                                tracing::warn!(account = %acct.name, %e, "list_folders from store failed; using idle_folder only");
                                vec![idle_folder.to_string()]
                            }
                        },
                        Err(e) => {
                            tracing::warn!(account = %acct.name, %e, "store open failed; using idle_folder only");
                            vec![idle_folder.to_string()]
                        }
                    }
                };

                // 2. Sync every folder (skipping idle_folder — already synced
                //    during discovery above when static_folders is empty).
                for folder in &folders {
                    if static_folders.is_empty() && folder.as_str() == idle_folder.as_str() {
                        // Already synced in the discovery step above.
                        continue;
                    }
                    match sync_once(&acct, folder, fetch_bodies, body_limit, notify).await {
                        Err(e) => {
                            tracing::warn!(account = %acct.name, %folder, %e, "folder sync failed; continuing");
                        }
                        Ok(new_count) => {
                            #[cfg(unix)]
                            emit_folder_updated(
                                ipc.as_ref(),
                                local_events.as_ref(),
                                acct.name.clone(),
                                folder.clone(),
                                new_count,
                            );
                            #[cfg(not(unix))]
                            emit_folder_updated(
                                local_events.as_ref(),
                                acct.name.clone(),
                                folder.clone(),
                                new_count,
                            );
                        }
                    }
                }

                // 3. Wait for push signal on idle_folder OR periodic timer —
                //    whichever fires first. Either triggers the next full cycle.
                tokio::select! {
                    _ = wait_for_change(&acct, &idle_folder) => {
                        tracing::debug!(account = %acct.name, "push signal; re-syncing all folders");
                    }
                    _ = tokio::time::sleep(Duration::from_secs(poll_interval_secs)) => {
                        tracing::debug!(account = %acct.name, "poll timer fired; re-syncing all folders");
                    }
                }
            }
        });
    }

    // Wait forever (or until Ctrl-C). JoinSet propagates panics; we just
    // let them surface so the daemon dies loud.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received; shutting down");
        }
        _ = async {
            while tasks.join_next().await.is_some() {}
        } => {}
    }
    // Dropping ipc here closes the listener and all client connections see EOF.
    Ok(())
}

/// Wait for the server to signal new data. Dispatches on `account.transport`:
/// - IMAP → RFC 2177 IDLE (25-min keepalive window).
/// - JMAP → RFC 8620 EventSource; first event or stream close signals a cycle.
/// - Graph → no push path today; sleeps 5 min before the next poll cycle.
///
/// Any error backs off 30 s before returning, matching the outer loop pattern.
pub async fn wait_for_change(account: &Account, folder: &str) {
    use inbx_config::Transport;

    const BACKOFF: Duration = Duration::from_secs(30);

    match &account.transport {
        Transport::Imap => match inbx_net::idle::wait_for_new_in(account, folder).await {
            Ok(_) => tracing::info!(account = %account.name, "idle signal"),
            Err(e) => {
                tracing::warn!(account = %account.name, %e, "idle error; sleeping 30s");
                tokio::time::sleep(BACKOFF).await;
            }
        },
        Transport::Jmap { session_url } => {
            let client = match inbx_net::jmap::JmapClient::connect(account, session_url).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "JMAP connect failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            let mut stream = match client.open_event_source().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "JMAP EventSource open failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            match stream.next_event().await {
                Ok(Some(payload)) => {
                    tracing::info!(account = %account.name, %payload, "JMAP push event");
                }
                Ok(None) => {
                    tracing::debug!(account = %account.name, "JMAP EventSource closed; reconnecting");
                }
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "JMAP EventSource error; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        }
        Transport::Graph => {
            // Delta-link poll: open store, resolve folder id, fetch changes.
            let store = match inbx_store::Store::open(&account.name).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "Graph: store open failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            let client = match inbx_net::graph::GraphClient::connect(account).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "Graph: connect failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            // Resolve folder display name → Graph folder id.
            let folder_id = match client.list_folders().await {
                Ok(folders) => {
                    match folders
                        .iter()
                        .find(|f| f.display_name.eq_ignore_ascii_case(folder))
                        .map(|f| f.id.clone())
                    {
                        Some(id) => id,
                        None => {
                            tracing::warn!(account = %account.name, %folder, "Graph: folder not found; sleeping 30s");
                            tokio::time::sleep(BACKOFF).await;
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "Graph: list_folders failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            // Load stored delta link (None on first run).
            let stored_link = match store.get_delta_link(folder).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "Graph: get_delta_link failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            // Call delta endpoint.
            let (messages, new_link) = match client
                .delta_messages(&folder_id, stored_link.as_deref())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(account = %account.name, %e, "Graph: delta_messages failed; sleeping 30s");
                    tokio::time::sleep(BACKOFF).await;
                    return;
                }
            };
            // Persist new delta link.
            if let Err(e) = store.set_delta_link(folder, new_link.as_deref()).await {
                tracing::warn!(account = %account.name, %e, "Graph: set_delta_link failed (ignored)");
            }
            if messages.is_empty() {
                // No changes — sleep before next poll, don't signal sync.
                tracing::debug!(account = %account.name, "Graph delta: no changes; sleeping 75s");
                tokio::time::sleep(Duration::from_secs(75)).await;
                return;
            }
            tracing::info!(account = %account.name, count = messages.len(), "Graph delta: new messages");
        }
    }
}

/// Return the provider's canonical spelling for a configured folder name.
///
/// IMAP's `INBOX` is case-insensitive while providers often report `Inbox`.
/// Matching case-insensitively prevents discovery from creating a second sync.
fn canonical_folder_name<'a>(folders: &'a [inbx_net::FolderInfo], configured: &'a str) -> &'a str {
    folders
        .iter()
        .find(|candidate| candidate.name.eq_ignore_ascii_case(configured))
        .map(|candidate| candidate.name.as_str())
        .unwrap_or(configured)
}

/// Run one full sync cycle for an account: drain outbox, fetch headers,
/// upsert into the store, optionally download bodies. Returns the count of
/// newly arrived messages (UIDs higher than the previous max).
pub async fn sync_once(
    account: &Account,
    folder: &str,
    fetch_bodies: bool,
    body_limit: u32,
    notify: bool,
) -> Result<u32> {
    // Best-effort outbox drain piggybacks on this connection cycle.
    let store = inbx_store::Store::open(&account.name).await?;
    let due = store.outbox_due().await?;
    for r in due {
        match inbx_net::send_message(account, &r.raw).await {
            Ok(()) => {
                store.outbox_delete(r.id).await?;
                tracing::info!(account = %account.name, id = r.id, "outbox: sent");
            }
            Err(e) => {
                store.outbox_record_failure(r.id, &e.to_string()).await?;
                tracing::warn!(account = %account.name, id = r.id, %e, "outbox: still failing");
            }
        }
    }

    let mut provider = inbx_net::connect_provider(account, Some(&store)).await?;
    let folders = provider.list_folders().await?;
    let folder = canonical_folder_name(&folders, folder);
    let folder_rows = folders
        .iter()
        .map(|f| inbx_store::FolderRow {
            name: f.name.clone(),
            delim: f.delim.clone(),
            special_use: f.special_use.clone(),
            attrs: if f.attrs.is_empty() {
                None
            } else {
                Some(f.attrs.join(","))
            },
            uidvalidity: None,
            uidnext: None,
            delta_link: None,
            last_sync_unix: None,
        })
        .collect::<Vec<_>>();
    // `list_folders` is the provider's complete authoritative listing. Do not
    // reconcile after failed discovery or a partial message snapshot.
    store.reconcile_folders(&folder_rows).await?;
    // Always request a complete snapshot: pruning is only sound after a
    // successful full fetch, never after a provider delta or failed stream.
    let generation = store.reserve_snapshot_generation(folder).await?;
    let snapshot = provider.fetch_headers(folder, None, 1000).await?;
    let uidvalidity = snapshot.transport.uidvalidity();
    let applied = apply_snapshot(&store, folder, generation, snapshot).await?;
    if !applied.applied {
        let _ = provider.logout().await;
        return Ok(0);
    }
    let new_count = applied.new_count();
    let new_rows = applied.new_rows;
    if notify && new_count > 0 {
        let summary = format!("{new_count} new in {}", account.name);
        let body = new_rows
            .iter()
            .take(5)
            .map(|h| {
                format!(
                    "{} — {}",
                    h.from_addr.as_deref().unwrap_or(""),
                    h.subject.as_deref().unwrap_or(""),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .appname("inbx")
            .show()
        {
            tracing::warn!(%e, "notify failed");
        }
    }
    if fetch_bodies {
        let pending = store.list_unfetched(folder, body_limit).await?;
        if !pending.is_empty() {
            let uids: Vec<i64> = pending;
            let bodies = provider.fetch_bodies(folder, &uids).await?;
            // Open contacts store once for the entire body batch (best-effort).
            let contacts = inbx_contacts::ContactsStore::open(&account.name).await.ok();
            for (uid, raw) in bodies {
                let path = store.write_maildir(folder, &raw, "\\Seen")?;
                index_in_store(&store, folder, uid, uidvalidity, &raw).await?;
                store
                    .set_maildir_path(folder, uid, uidvalidity, &path.to_string_lossy())
                    .await?;
                // Harvest Autocrypt header from each incoming body (best-effort).
                if let Some(cs) = &contacts {
                    harvest_autocrypt(cs, &raw).await;
                }
            }
        }
    }
    let _ = provider.logout().await;
    Ok(new_count as u32)
}

/// Parse and store any Autocrypt: header from a raw message into the contacts
/// store. Logs on error but never propagates — sync must not fail over a
/// contacts update.
pub async fn harvest_autocrypt(contacts: &inbx_contacts::ContactsStore, raw: &[u8]) {
    use inbx_render::AutocryptHeader;
    match inbx_render::render_message_with_pgp(raw, inbx_render::RemotePolicy::Block, None, None)
        .await
    {
        Ok(rendered) => {
            if let Some(AutocryptHeader {
                addr,
                keydata_armored,
                fingerprint,
                ..
            }) = rendered.autocrypt
            {
                if let Err(e) = contacts
                    .store_autocrypt(&addr, &keydata_armored, &fingerprint)
                    .await
                {
                    tracing::debug!(%addr, %e, "autocrypt harvest: store_autocrypt failed (ignored)");
                } else {
                    tracing::debug!(%addr, %fingerprint, "autocrypt harvest: stored pubkey");
                }
            }
        }
        Err(e) => {
            tracing::debug!(%e, "autocrypt harvest: render failed (ignored)");
        }
    }
}

/// Index a raw message body into the FTS store for search and threading.
pub async fn index_in_store(
    store: &inbx_store::Store,
    folder: &str,
    uid: i64,
    uidvalidity: i64,
    raw: &[u8],
) -> Result<()> {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Ok(());
    };
    let message_id = parsed.message_id().map(|s| s.to_string());
    let in_reply_to = parsed
        .in_reply_to()
        .as_text_list()
        .and_then(|v| v.first().map(|s| s.to_string()));
    let refs: Vec<String> = parsed
        .references()
        .as_text_list()
        .map(|v| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    store
        .set_threading(
            folder,
            uid,
            uidvalidity,
            message_id.as_deref(),
            in_reply_to.as_deref(),
            &refs,
        )
        .await?;
    let subject = parsed.subject().unwrap_or_default();
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .unwrap_or("")
        .to_string();
    let to = parsed
        .to()
        .map(|g| {
            g.iter()
                .filter_map(|a| a.address().map(|s| s.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let body = parsed
        .body_text(0)
        .map(|s| s.to_string())
        .unwrap_or_default();
    store
        .index_for_search(folder, uid, uidvalidity, subject, &from, &to, &body)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use inbx_net::{HeaderRow, provider::HeaderSnapshot, provider::SnapshotTransport};
    use inbx_store::{MessageRow, Store};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::{apply_snapshot, canonical_folder_name, emit_folder_updated};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    #[cfg(unix)]
    static SOCKET_COUNTER: AtomicUsize = AtomicUsize::new(0);
    use tokio::sync::mpsc::unbounded_channel;

    #[cfg(unix)]
    #[tokio::test]
    async fn folder_updated_reaches_ipc_and_local_sinks() {
        let path = std::env::temp_dir().join(format!(
            "inbx-sync-test-{}-{}.sock",
            std::process::id(),
            SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let ipc = inbx_ipc::Server::bind_at(&path).await.unwrap();
        let mut client = inbx_ipc::Client::connect_to(&path).await.unwrap();
        let mut ipc_events = client.receiver();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), ipc_events.recv())
                .await
                .unwrap(),
            Some(inbx_ipc::Event::Hello { .. })
        ));
        let (local_events, mut local_rx) = unbounded_channel();

        emit_folder_updated(
            Some(&ipc),
            Some(&local_events),
            "work".into(),
            "INBOX".into(),
            3,
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), local_rx.recv())
                .await
                .unwrap(),
            Some(inbx_ipc::Event::FolderUpdated { account, folder, new_count })
                if account == "work" && folder == "INBOX" && new_count == 3
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), ipc_events.recv())
                .await
                .unwrap(),
            Some(inbx_ipc::Event::FolderUpdated { account, folder, new_count })
                if account == "work" && folder == "INBOX" && new_count == 3
        ));
    }

    #[cfg(unix)]
    #[test]
    fn closed_local_event_sink_does_not_fail_sync() {
        let (local_events, local_rx) = unbounded_channel();
        drop(local_rx);
        emit_folder_updated(None, Some(&local_events), "work".into(), "INBOX".into(), 0);
    }

    fn header(uid: i64, provider_id: Option<&str>) -> HeaderRow {
        HeaderRow {
            uid,
            uidvalidity: 0,
            message_id: None,
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            fetched_at_unix: 0,
            provider_id: provider_id.map(str::to_owned),
        }
    }

    async fn store() -> Store {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap();
        sqlx::migrate!("../inbx-store/migrations")
            .run(&pool)
            .await
            .unwrap();
        Store::from_pool(pool)
    }

    fn row(uid: i64, provider_id: Option<&str>) -> MessageRow {
        MessageRow {
            folder: "Inbox".to_owned(),
            uid,
            uidvalidity: 0,
            message_id: None,
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            maildir_path: None,
            headers_only: 1,
            fetched_at_unix: 0,
            in_reply_to: None,
            refs: None,
            thread_id: None,
            provider_id: provider_id.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn stale_snapshot_apply_preserves_applied_flag_and_has_no_new_rows() {
        let store = store().await;
        let stale = store.reserve_snapshot_generation("Inbox").await.unwrap();
        store.reserve_snapshot_generation("Inbox").await.unwrap();
        let result = apply_snapshot(
            &store,
            "Inbox",
            stale,
            HeaderSnapshot {
                rows: vec![header(1, None)],
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 0 },
            },
        )
        .await
        .unwrap();
        assert!(!result.applied);
        assert!(result.new_rows.is_empty());
    }

    #[tokio::test]
    async fn complete_and_partial_snapshots_prune_by_their_identity() {
        let store = store().await;
        store.upsert_message(&row(1, None)).await.unwrap();
        apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![],
                complete: false,
                transport: SnapshotTransport::Imap { uidvalidity: 0 },
            },
        )
        .await
        .unwrap();
        assert_eq!(store.list_messages("Inbox", 10).await.unwrap().len(), 1);
        apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![],
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 0 },
            },
        )
        .await
        .unwrap();
        assert!(store.list_messages("Inbox", 10).await.unwrap().is_empty());

        store.upsert_message(&row(1, Some("old"))).await.unwrap();
        apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![],
                complete: false,
                transport: SnapshotTransport::Opaque,
            },
        )
        .await
        .unwrap();
        assert_eq!(store.list_messages("Inbox", 10).await.unwrap().len(), 1);
        apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![],
                complete: true,
                transport: SnapshotTransport::Opaque,
            },
        )
        .await
        .unwrap();
        assert!(store.list_messages("Inbox", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn uidvalidity_reset_replaces_old_rows_and_legacy_ids_are_not_new() {
        let store = store().await;
        store
            .upsert_folder(&inbx_store::FolderRow {
                name: "Inbox".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: Some(1),
                uidnext: None,
                delta_link: None,
                last_sync_unix: None,
            })
            .await
            .unwrap();
        store.upsert_message(&row(1, None)).await.unwrap();
        let mut incoming = header(2, None);
        incoming.uidvalidity = 2;
        let applied = apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![incoming],
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 2 },
            },
        )
        .await
        .unwrap();
        assert_eq!(applied.new_count(), 1);
        assert_eq!(store.list_messages("Inbox", 10).await.unwrap()[0].uid, 2);

        let mut legacy = row(427_567_909, Some("test"));
        legacy.maildir_path = Some("cur/body".into());
        legacy.headers_only = 0;
        store.upsert_message(&legacy).await.unwrap();
        let applied = apply_snapshot(
            &store,
            "Inbox",
            store.reserve_snapshot_generation("Inbox").await.unwrap(),
            HeaderSnapshot {
                rows: vec![header(8_783_962_037_831_871_269, Some("test"))],
                complete: false,
                transport: SnapshotTransport::Opaque,
            },
        )
        .await
        .unwrap();
        assert_eq!(applied.new_count(), 0);
        let migrated = store.list_messages("Inbox", 10).await.unwrap();
        assert!(
            migrated
                .iter()
                .any(|message| message.uid == 8_783_962_037_831_871_269
                    && message.maildir_path.as_deref() == Some("cur/body"))
        );
    }

    #[test]
    fn canonical_folder_name_uses_discovered_inbox_spelling() {
        let folders = vec![inbx_net::FolderInfo {
            name: "Inbox".to_owned(),
            delim: Some("/".to_owned()),
            special_use: Some("\\Inbox".to_owned()),
            attrs: Vec::new(),
            selectable: true,
        }];

        assert_eq!(canonical_folder_name(&folders, "INBOX"), "Inbox");
        assert_eq!(canonical_folder_name(&folders, "Archive"), "Archive");
    }
}
