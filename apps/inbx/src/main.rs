mod tui;

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use inbx_config::{Account, Config, TlsMode};

#[derive(Parser)]
#[command(name = "inbx", version, about = "modal-vim email client")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print resolved config path and account count.
    Config,
    /// Manage accounts.
    Accounts {
        #[command(subcommand)]
        action: AccountCmd,
    },
    /// Fetch INBOX headers + discover folders for an account.
    Fetch {
        #[arg(long)]
        account: Option<String>,
        /// Also download message bodies for the most recent messages.
        #[arg(long)]
        bodies: bool,
        /// Cap on bodies to download per fetch when `--bodies` is set.
        #[arg(long, default_value_t = 200)]
        body_limit: u32,
    },
    /// List recent messages from local index.
    List {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Read RFC 5322 from stdin, send via SMTP, append to Sent.
    Send {
        #[arg(long)]
        account: Option<String>,
        /// Skip APPEND to Sent folder.
        #[arg(long)]
        no_save: bool,
    },
    /// Launch the read-only TUI.
    Tui {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Interactive add. Stores password in OS keyring.
    Add,
    List,
    /// Show folders cached locally for an account.
    Folders {
        #[arg(long)]
        account: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Cmd::Config => cmd_config(),
        Cmd::Accounts { action } => match action {
            AccountCmd::Add => cmd_accounts_add(),
            AccountCmd::List => cmd_accounts_list(),
            AccountCmd::Folders { account } => cmd_accounts_folders(account).await,
        },
        Cmd::Fetch {
            account,
            bodies,
            body_limit,
        } => cmd_fetch(account, bodies, body_limit).await,
        Cmd::List {
            account,
            folder,
            limit,
        } => cmd_list(account, folder, limit).await,
        Cmd::Send { account, no_save } => cmd_send(account, no_save).await,
        Cmd::Tui { account } => cmd_tui(account).await,
    }
}

async fn cmd_tui(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    tui::run(acct.name.clone()).await
}

fn cmd_config() -> Result<()> {
    let path = inbx_config::config_path()?;
    let cfg = inbx_config::load()?;
    println!("config: {}", path.display());
    println!("accounts: {}", cfg.accounts.len());
    Ok(())
}

fn cmd_accounts_list() -> Result<()> {
    let cfg = inbx_config::load()?;
    if cfg.accounts.is_empty() {
        println!("(no accounts configured)");
    } else {
        for a in cfg.accounts {
            println!(
                "{} <{}>  imap={}:{} ({:?})  smtp={}:{} ({:?})",
                a.name,
                a.email,
                a.imap_host,
                a.imap_port,
                a.imap_security,
                a.smtp_host,
                a.smtp_port,
                a.smtp_security,
            );
        }
    }
    Ok(())
}

fn cmd_accounts_add() -> Result<()> {
    let mut cfg = inbx_config::load()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut lock = stdin.lock();

    let name = prompt(&mut lock, &mut stdout, "account name (e.g. personal): ")?;
    if cfg.accounts.iter().any(|a| a.name == name) {
        bail!("account {name} already exists");
    }
    let email = prompt(&mut lock, &mut stdout, "email: ")?;
    let imap_host = prompt(&mut lock, &mut stdout, "imap host: ")?;
    let imap_security = prompt_tls(&mut lock, &mut stdout, "imap security [tls/starttls]: ")?;
    let imap_port_default = match imap_security {
        TlsMode::Tls => 993,
        TlsMode::Starttls => 143,
    };
    let imap_port = prompt_port(
        &mut lock,
        &mut stdout,
        &format!("imap port [{imap_port_default}]: "),
        imap_port_default,
    )?;
    let smtp_host = prompt(&mut lock, &mut stdout, "smtp host: ")?;
    let smtp_security = prompt_tls(&mut lock, &mut stdout, "smtp security [tls/starttls]: ")?;
    let smtp_port_default = match smtp_security {
        TlsMode::Tls => 465,
        TlsMode::Starttls => 587,
    };
    let smtp_port = prompt_port(
        &mut lock,
        &mut stdout,
        &format!("smtp port [{smtp_port_default}]: "),
        smtp_port_default,
    )?;
    let username = prompt(&mut lock, &mut stdout, "username: ")?;
    let password =
        rpassword::prompt_password("password (app password): ").context("read password")?;

    inbx_config::store_password(&name, &password)?;

    cfg.accounts.push(Account {
        name: name.clone(),
        email,
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
        username,
    });
    inbx_config::save(&cfg)?;
    println!("added account {name}; password stored in keyring");
    Ok(())
}

async fn cmd_accounts_folders(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;
    let folders = store.list_folders().await?;
    if folders.is_empty() {
        println!("(no folders cached — run `inbx fetch` first)");
        return Ok(());
    }
    for f in folders {
        println!(
            "{:<32}  delim={:<3}  special={:<10}  uidvalidity={}",
            f.name,
            f.delim.unwrap_or_else(|| "-".into()),
            f.special_use.unwrap_or_else(|| "-".into()),
            f.uidvalidity
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
    Ok(())
}

async fn cmd_fetch(account: Option<String>, fetch_bodies: bool, body_limit: u32) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let password = inbx_config::load_password(&acct.name)
        .with_context(|| format!("no password in keyring for {}", acct.name))?;

    tracing::info!(account = %acct.name, "connecting");
    let mut session = inbx_net::connect_imap(&acct, &password).await?;
    let store = inbx_store::Store::open(&acct.name).await?;

    tracing::info!("listing folders");
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
            })
            .await?;
    }
    println!("folders: {}", folders.len());

    tracing::info!("fetching INBOX headers");
    let (uidvalidity, rows) = inbx_net::fetch_inbox_headers(&mut session).await?;
    let prev = store.folder_uidvalidity("INBOX").await?;
    if let Some(prev) = prev
        && prev as u32 != uidvalidity
    {
        tracing::warn!(prev, new = uidvalidity, "UIDVALIDITY changed; wiping INBOX");
        store.wipe_folder_messages("INBOX").await?;
    }
    store
        .upsert_folder(&inbx_store::FolderRow {
            name: "INBOX".into(),
            delim: None,
            special_use: None,
            attrs: None,
            uidvalidity: Some(uidvalidity as i64),
            uidnext: None,
        })
        .await?;
    for h in &rows {
        store
            .upsert_message(&inbx_store::MessageRow {
                folder: "INBOX".into(),
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
            })
            .await?;
    }
    println!("INBOX: {} messages indexed", rows.len());

    if fetch_bodies {
        let pending = store.list_unfetched("INBOX", body_limit).await?;
        if !pending.is_empty() {
            tracing::info!(count = pending.len(), "fetching bodies");
            let uids: Vec<u32> = pending.iter().map(|u| *u as u32).collect();
            let bodies = inbx_net::fetch_bodies(&mut session, "INBOX", &uids).await?;
            for (uid, raw) in bodies {
                let path = store.write_maildir("INBOX", &raw, "\\Seen")?;
                store
                    .set_maildir_path(
                        "INBOX",
                        uid as i64,
                        uidvalidity as i64,
                        &path.to_string_lossy(),
                    )
                    .await?;
            }
            println!("INBOX: bodies downloaded");
        }
    }

    let _ = session.logout().await;
    Ok(())
}

async fn cmd_list(account: Option<String>, folder: String, limit: u32) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;
    let rows = store.list_messages(&folder, limit).await?;
    if rows.is_empty() {
        println!("(no messages — run `inbx fetch` first)");
        return Ok(());
    }
    for m in rows {
        let date = m.date_unix.map(format_unix).unwrap_or_else(|| "—".into());
        let from = m.from_addr.unwrap_or_default();
        let subj = m.subject.unwrap_or_default();
        println!("{:>10}  {:<30}  {}", date, truncate(&from, 30), subj);
    }
    Ok(())
}

async fn cmd_send(account: Option<String>, no_save: bool) -> Result<()> {
    use std::io::Read as _;

    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let password = inbx_config::load_password(&acct.name)
        .with_context(|| format!("no password in keyring for {}", acct.name))?;

    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .context("read stdin")?;
    if raw.is_empty() {
        bail!("empty input on stdin");
    }
    // Normalize bare-LF to CRLF for SMTP wire format.
    let raw = normalize_crlf(raw);

    tracing::info!(account = %acct.name, bytes = raw.len(), "sending");
    inbx_net::send_message(&acct, &password, &raw).await?;
    println!("sent");

    if no_save {
        return Ok(());
    }

    tracing::info!("appending to Sent folder");
    let mut session = inbx_net::connect_imap(&acct, &password).await?;
    let folders = inbx_net::list_folders(&mut session).await?;
    let sent = inbx_net::find_sent_folder(&folders);
    match sent {
        Some(name) => {
            inbx_net::append_message(&mut session, &name, &raw).await?;
            println!("appended to {name}");
        }
        None => {
            tracing::warn!("no Sent folder discovered; skipping APPEND");
        }
    }
    let _ = session.logout().await;
    Ok(())
}

fn normalize_crlf(input: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + 32);
    let mut prev_cr = false;
    for b in input {
        if b == b'\n' && !prev_cr {
            out.push(b'\r');
        }
        prev_cr = b == b'\r';
        out.push(b);
    }
    out
}

fn pick_account<'a>(cfg: &'a Config, name: Option<&str>) -> Result<&'a Account> {
    match name {
        Some(n) => cfg
            .accounts
            .iter()
            .find(|a| a.name == n)
            .with_context(|| format!("no account named {n}")),
        None => match cfg.accounts.as_slice() {
            [] => bail!("no accounts configured; run `inbx accounts add`"),
            [only] => Ok(only),
            _ => bail!("multiple accounts configured; pass --account NAME"),
        },
    }
}

fn prompt(stdin: &mut impl BufRead, stdout: &mut impl Write, msg: &str) -> Result<String> {
    stdout.write_all(msg.as_bytes())?;
    stdout.flush()?;
    let mut s = String::new();
    stdin.read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn prompt_tls(stdin: &mut impl BufRead, stdout: &mut impl Write, msg: &str) -> Result<TlsMode> {
    let raw = prompt(stdin, stdout, msg)?;
    match raw.to_ascii_lowercase().as_str() {
        "" | "tls" => Ok(TlsMode::Tls),
        "starttls" => Ok(TlsMode::Starttls),
        other => bail!("invalid tls mode: {other}"),
    }
}

fn prompt_port(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    msg: &str,
    default: u16,
) -> Result<u16> {
    let raw = prompt(stdin, stdout, msg)?;
    if raw.is_empty() {
        return Ok(default);
    }
    Ok(raw.parse()?)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn format_unix(ts: i64) -> String {
    // Cheap ISO-ish format without bringing chrono/jiff in M2.
    let secs = ts.max(0) as u64;
    let days = secs / 86400;
    // 1970-01-01 epoch — civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}
