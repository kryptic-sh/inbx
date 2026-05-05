//! inbx-sync — headless multi-account sync daemon.
//!
//! Thin CLI wrapper around the `inbx_sync` library crate. Parses args,
//! inits logging, binds the IPC server, then hands off to `inbx_sync::run`.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
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
    /// Folder watched via push / IDLE (defaults to INBOX). Push signals on this
    /// folder trigger an immediate re-sync of ALL folders.
    #[arg(long, default_value = "INBOX")]
    idle_folder: String,
    /// Restrict sync to these folders. Default: empty = discover all from server.
    #[arg(long, num_args = 0..)]
    folders: Vec<String>,
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
    let accounts: Vec<_> = names
        .iter()
        .filter_map(|name| {
            let acct = cfg.accounts.iter().find(|a| &a.name == name);
            if acct.is_none() {
                tracing::warn!(%name, "skipping; no such account");
            }
            acct.cloned()
        })
        .collect();

    // Bind the IPC server so connected TUI instances receive sync events.
    // On non-unix platforms this logs a warning and continues without IPC.
    #[cfg(unix)]
    let ipc_server: Option<Arc<inbx_ipc::Server>> = match inbx_ipc::Server::bind().await {
        Ok(srv) => {
            tracing::info!(socket = %inbx_ipc::socket_path().display(), "ipc: listening");
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
    let ipc_server: Option<Arc<inbx_ipc::Server>> = {
        tracing::warn!("ipc: unix sockets not supported on this platform; running without IPC");
        None
    };

    inbx_sync::run(inbx_sync::Config {
        accounts,
        ipc: ipc_server,
        notifications: cli.notify,
        idle_folder: cli.idle_folder,
        folders: cli.folders,
        fetch_bodies: cli.bodies,
        body_limit: cli.body_limit,
        poll_interval_secs: 300,
    })
    .await
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,html5ever=error,markup5ever=error,ammonia=warn,html2text=warn",
        )
    });
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
