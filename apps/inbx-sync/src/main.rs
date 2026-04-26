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
        tasks.spawn(async move {
            loop {
                if let Err(e) = sync_once(&acct, &folder, bodies, body_limit, notify).await {
                    tracing::warn!(account = %acct.name, %e, "cycle failed; sleeping 30s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                match inbx_net::idle::wait_for_new_in(&acct, &folder).await {
                    Ok(_) => tracing::info!(account = %acct.name, "idle signal"),
                    Err(e) => {
                        tracing::warn!(account = %acct.name, %e, "idle error; sleeping 30s");
                        tokio::time::sleep(Duration::from_secs(30)).await;
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
    Ok(())
}

async fn sync_once(
    account: &Account,
    folder: &str,
    fetch_bodies: bool,
    body_limit: u32,
    notify: bool,
) -> Result<()> {
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
            }
        }
    }
    let _ = session.logout().await;
    Ok(())
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
