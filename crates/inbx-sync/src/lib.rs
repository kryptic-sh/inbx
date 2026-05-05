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
                            if let Some(ref srv) = ipc {
                                srv.send(inbx_ipc::Event::FolderUpdated {
                                    account: acct.name.clone(),
                                    folder: idle_folder.to_string(),
                                    new_count,
                                });
                            }
                            #[cfg(not(unix))]
                            let _ = new_count;
                        }
                    }
                    // Read back discovered folders from the store.
                    match inbx_store::Store::open(&acct.name).await {
                        Ok(store) => match store.list_folders().await {
                            Ok(rows) => rows.into_iter().map(|r| r.name).collect(),
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
                            if let Some(ref srv) = ipc {
                                srv.send(inbx_ipc::Event::FolderUpdated {
                                    account: acct.name.clone(),
                                    folder: folder.clone(),
                                    new_count,
                                });
                            }
                            #[cfg(not(unix))]
                            let _ = new_count;
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

    let mut session = inbx_net::connect_imap(account).await?;
    let folders = inbx_net::list_folders(&mut session).await?;
    for f in &folders {
        store
            .upsert_folder(&inbx_store::FolderRow {
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
            })
            .await?;
    }
    let (uidvalidity, rows) = inbx_net::fetch_headers(&mut session, folder).await?;
    let prev = store.folder_uidvalidity(folder).await?;
    if let Some(prev) = prev
        && prev as u32 != uidvalidity
    {
        tracing::warn!(prev, new = uidvalidity, %folder, "UIDVALIDITY changed; wiping");
        store.wipe_folder_messages(folder).await?;
    }
    let pre_max = store
        .folder_max_uid(folder, uidvalidity as i64)
        .await?
        .unwrap_or(0);
    store
        .upsert_folder(&inbx_store::FolderRow {
            name: folder.to_string(),
            delim: None,
            special_use: None,
            attrs: None,
            uidvalidity: Some(uidvalidity as i64),
            uidnext: None,
            delta_link: None,
        })
        .await?;
    for h in &rows {
        store
            .upsert_message(&inbx_store::MessageRow {
                folder: folder.to_string(),
                uid: h.uid as i64,
                uidvalidity: h.uidvalidity as i64,
                message_id: h.message_id.clone(),
                subject: h.subject.clone(),
                from_addr: h.from_addr.clone(),
                to_addrs: h.to_addrs.clone(),
                date_unix: h.date_unix,
                flags: h.flags.clone(),
                maildir_path: None,
                headers_only: 1,
                fetched_at_unix: h.fetched_at_unix,
                in_reply_to: None,
                refs: None,
                thread_id: None,
                provider_id: h.provider_id.clone(),
            })
            .await?;
    }
    let new_count = rows.iter().filter(|h| (h.uid as i64) > pre_max).count();
    if notify && new_count > 0 {
        let summary = format!("{new_count} new in {}", account.name);
        let body = rows
            .iter()
            .filter(|h| (h.uid as i64) > pre_max)
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
            let uids: Vec<u32> = pending.iter().map(|u| *u as u32).collect();
            let bodies = inbx_net::fetch_bodies(&mut session, folder, &uids).await?;
            // Open contacts store once for the entire body batch (best-effort).
            let contacts = inbx_contacts::ContactsStore::open(&account.name).await.ok();
            for (uid, raw) in bodies {
                let path = store.write_maildir(folder, &raw, "\\Seen")?;
                store
                    .set_maildir_path(
                        folder,
                        uid as i64,
                        uidvalidity as i64,
                        &path.to_string_lossy(),
                    )
                    .await?;
                index_in_store(&store, folder, uid as i64, uidvalidity as i64, &raw).await?;
                // Harvest Autocrypt header from each incoming body (best-effort).
                if let Some(cs) = &contacts {
                    harvest_autocrypt(cs, &raw).await;
                }
            }
        }
    }
    let _ = session.logout().await;
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
