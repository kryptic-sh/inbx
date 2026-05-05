//! inbx-sync — headless multi-account sync daemon.
//!
//! For each configured account, a task loops:
//!   1. Drain outbox (best effort).
//!   2. Connect IMAP, fetch + index INBOX headers, optionally bodies.
//!   3. Open IDLE on INBOX, wait for the keepalive window or a server
//!      EXISTS notification.
//!   4. Repeat. On error, back off 30s and reconnect.
//!
//! The daemon writes structured logs to stderr and to a daily-rotated
//! file under `$XDG_DATA_HOME/inbx/log/inbx-sync.YYYY-MM-DD`. Quit with
//! Ctrl-C; tasks shut down cleanly.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use inbx_config::Account;
use mail_parser::MessageParser;
use tokio::task::JoinSet;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Parser)]
#[command(name = "inbx-sync", version, about = "headless inbx sync daemon")]
struct Cli {
    /// Sync only these accounts. Default: every configured account.
    #[arg(long, num_args = 0..)]
    account: Vec<String>,
    /// Also download bodies on each fetch cycle.
    #[arg(long)]
    bodies: bool,
    /// Cap on bodies per cycle when --bodies is set.
    #[arg(long, default_value_t = 200)]
    body_limit: u32,
    /// Folder to watch per account (defaults to INBOX).
    #[arg(long, default_value = "INBOX")]
    folder: String,
    /// Fire desktop notifications on new mail.
    #[arg(long)]
    notify: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = init_logging();
    let cli = Cli::parse();
    let cfg = inbx_config::load()?;

    let names: Vec<String> = if cli.account.is_empty() {
        cfg.accounts.iter().map(|a| a.name.clone()).collect()
    } else {
        cli.account.clone()
    };
    if names.is_empty() {
        anyhow::bail!("no accounts configured; run `inbx accounts add`");
    }

    let folder = Arc::new(cli.folder);
    tracing::info!(accounts = names.len(), folder = %folder, "inbx-sync starting");

    // Bind the IPC server so connected TUI instances receive sync events.
    // On non-unix platforms this logs a warning and continues without IPC.
    #[cfg(unix)]
    let ipc_server: Option<Arc<inbx_ipc::Server>> = match inbx_ipc::Server::bind().await {
        Ok(srv) => {
            tracing::info!(socket = %inbx_ipc::socket_path().display(), "ipc: listening");
            // Broadcast Hello so any TUI that connected before the first cycle
            // knows the daemon version.
            srv.send(inbx_ipc::Event::Hello {
                version: env!("CARGO_PKG_VERSION").to_string(),
            });
            Some(srv)
        }
        Err(e) => {
            tracing::error!(%e, "ipc: bind failed; exiting");
            std::process::exit(1);
        }
    };
    #[cfg(not(unix))]
    {
        tracing::warn!("ipc: unix sockets not supported on this platform; running without IPC");
    }

    // Heartbeat task: every 60s broadcast a Heartbeat so TUI clients can
    // detect a stale/dead daemon.
    #[cfg(unix)]
    if let Some(srv) = ipc_server.clone() {
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
    for name in names {
        let acct = match cfg.accounts.iter().find(|a| a.name == name) {
            Some(a) => a.clone(),
            None => {
                tracing::warn!(%name, "skipping; no such account");
                continue;
            }
        };
        let folder = folder.clone();
        let bodies = cli.bodies;
        let body_limit = cli.body_limit;
        let notify = cli.notify;
        #[cfg(unix)]
        let ipc = ipc_server.clone();
        tasks.spawn(async move {
            loop {
                match sync_once(&acct, &folder, bodies, body_limit, notify).await {
                    Err(e) => {
                        tracing::warn!(account = %acct.name, %e, "cycle failed; sleeping 30s");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                    Ok(new_count) => {
                        #[cfg(unix)]
                        if let Some(ref srv) = ipc {
                            srv.send(inbx_ipc::Event::FolderUpdated {
                                account: acct.name.clone(),
                                folder: folder.to_string(),
                                new_count,
                            });
                        }
                        #[cfg(not(unix))]
                        let _ = new_count;
                    }
                }
                wait_for_change(&acct, &folder).await;
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
    // Dropping ipc_server here closes the listener and all client connections
    // see EOF — no explicit shutdown needed.
    Ok(())
}

/// Wait for the server to signal new data.  Dispatches on `account.transport`:
/// - IMAP → RFC 2177 IDLE (25-min keepalive window).
/// - JMAP → RFC 8620 EventSource; first event or stream close signals a cycle.
/// - Graph → no push path today; sleeps 5 min before the next poll cycle.
///
/// Any error backs off 30 s before returning, matching the outer loop pattern.
async fn wait_for_change(account: &Account, folder: &str) {
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

async fn sync_once(
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

/// Parse and store any Autocrypt: header from a raw message into the contacts store.
/// Logs on error but never propagates — sync must not fail over a contacts update.
async fn harvest_autocrypt(contacts: &inbx_contacts::ContactsStore, raw: &[u8]) {
    use inbx_render::AutocryptHeader;
    // Use a minimal render pass (no PGP, no key lookup) just to get autocrypt field.
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

async fn index_in_store(
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

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let dirs = directories::ProjectDirs::from("sh", "kryptic", "inbx");
    let state_dir = dirs.as_ref().map(|d| d.data_local_dir().join("log"));
    let (file_layer, guard) = match state_dir {
        Some(path) => {
            if std::fs::create_dir_all(&path).is_ok() {
                let appender = tracing_appender::rolling::daily(&path, "inbx-sync");
                let (nb, guard) = tracing_appender::non_blocking(appender);
                (
                    Some(
                        tracing_subscriber::fmt::layer()
                            .with_writer(nb)
                            .with_ansi(false),
                    ),
                    Some(guard),
                )
            } else {
                (None, None)
            }
        }
        None => (None, None),
    };
    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer);
    if let Some(file_layer) = file_layer {
        subscriber.with(file_layer).init();
    } else {
        subscriber.init();
    }
    guard
}
