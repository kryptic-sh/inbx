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
        /// Fire a desktop notification for new mail.
        #[arg(long)]
        notify: bool,
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
        /// Attach a file. Repeatable.
        #[arg(long = "attach")]
        attachments: Vec<std::path::PathBuf>,
    },
    /// Launch the read-only TUI.
    Tui {
        #[arg(long)]
        account: Option<String>,
    },
    /// Address book operations.
    Contacts {
        #[command(subcommand)]
        action: ContactsCmd,
    },
    /// OAuth2 token management.
    OAuth {
        #[command(subcommand)]
        action: OAuthCmd,
    },
    /// Microsoft Graph (Outlook / M365) operations.
    Graph {
        #[command(subcommand)]
        action: GraphCmd,
    },
    /// JMAP (Fastmail / Stalwart) operations.
    Jmap {
        #[command(subcommand)]
        action: JmapCmd,
    },
    /// Full-text search across the local index.
    Search {
        #[arg(long)]
        account: Option<String>,
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Print all messages in a thread, oldest first.
    Thread {
        #[arg(long)]
        account: Option<String>,
        thread_id: String,
    },
    /// Calendar invite operations (.ics).
    Ical {
        #[command(subcommand)]
        action: IcalCmd,
    },
    /// Export messages from local index to mbox.
    Export {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Output path; `-` for stdout.
        #[arg(long, default_value = "-")]
        output: String,
    },
    /// Import an mbox or .eml file into a folder. The folder is created
    /// in the local index but not on the server.
    Import {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "Imported")]
        folder: String,
        /// Input path; `-` for stdin.
        #[arg(long, default_value = "-")]
        input: String,
        /// Treat input as a single .eml message instead of an mbox.
        #[arg(long)]
        eml: bool,
    },
    /// Emit an RFC 5322 draft (blank, reply, or forward) to stdout.
    Draft {
        #[command(subcommand)]
        action: DraftCmd,
    },
    /// Per-account canned templates.
    Template {
        #[command(subcommand)]
        action: TemplateCmd,
    },
    /// Loop forever: fetch + notify, then IDLE for new mail.
    Watch {
        #[arg(long)]
        account: Option<String>,
        /// Also download bodies for each new batch.
        #[arg(long)]
        bodies: bool,
    },
    /// Outbound queue for offline / failed sends.
    Outbox {
        #[command(subcommand)]
        action: OutboxCmd,
    },
    /// Add or remove flags on stored messages (UID STORE).
    Flag {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        #[arg(long, num_args = 1.., required = true)]
        uid: Vec<u32>,
        /// Flags to add (repeatable, e.g. `--add "\\Seen"`).
        #[arg(long = "add")]
        add: Vec<String>,
        /// Flags to remove (repeatable).
        #[arg(long = "del")]
        del: Vec<String>,
    },
    /// Mailbox CRUD on the server.
    Folder {
        #[command(subcommand)]
        action: FolderCmd,
    },
    /// ManageSieve (RFC 5804) — server-side filter scripts.
    Sieve {
        #[command(subcommand)]
        action: SieveCmd,
    },
    /// One-click List-Unsubscribe (RFC 8058) for a stored message.
    Unsubscribe {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
        /// Skip HTTPS one-click and use the mailto: target.
        #[arg(long)]
        mailto: bool,
        /// Print targets and exit without sending.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum TemplateCmd {
    /// Show saved template names.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Save stdin (or `--file PATH`) as a template under NAME.
    Save {
        #[arg(long)]
        account: Option<String>,
        name: String,
        #[arg(long, default_value = "-")]
        file: String,
    },
    /// Print a template's RFC 5322 source.
    Show {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// Emit a draft scaffolded from NAME (use the template, fill in fields).
    Use {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// Remove the template by name.
    Remove {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
}

#[derive(Subcommand)]
enum DraftCmd {
    /// Empty draft scaffolded with the account's identity.
    New {
        #[arg(long)]
        account: Option<String>,
    },
    /// Reply to the message at `uid` in `folder`.
    Reply {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
        #[arg(long)]
        all: bool,
    },
    /// Forward the message at `uid` in `folder`.
    Forward {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
    },
    /// Read RFC 5322 from stdin and APPEND to the server's Drafts folder.
    Save {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum FolderCmd {
    /// CREATE name on the server.
    Create {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// DELETE name from the server.
    Delete {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// RENAME from → to.
    Rename {
        #[arg(long)]
        account: Option<String>,
        from: String,
        to: String,
    },
    /// SUBSCRIBE / UNSUBSCRIBE.
    Subscribe {
        #[arg(long)]
        account: Option<String>,
        name: String,
        /// Set to false to UNSUBSCRIBE.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        on: bool,
    },
}

#[derive(Subcommand)]
enum OutboxCmd {
    /// Show queued messages.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Try to send everything that's due now.
    Drain {
        #[arg(long)]
        account: Option<String>,
    },
    /// Drop a queued message by id.
    Remove {
        #[arg(long)]
        account: Option<String>,
        id: i64,
    },
}

#[derive(Subcommand)]
enum SieveCmd {
    /// List scripts on the server. Active script marked with *.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Print one script's source.
    Get {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// Upload a script from a file (or stdin with `-`).
    Put {
        #[arg(long)]
        account: Option<String>,
        name: String,
        #[arg(long, default_value = "-")]
        file: String,
    },
    /// Mark a script active.
    Activate {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// Delete a script.
    Delete {
        #[arg(long)]
        account: Option<String>,
        name: String,
    },
    /// Generate, upload, and activate a vacation responder script.
    Vacation {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "vacation")]
        name: String,
        #[arg(long, default_value_t = 7)]
        days: u32,
        #[arg(long)]
        subject: Option<String>,
        /// Vacation message body. Use `-` to read from stdin.
        message: String,
    },
}

#[derive(Subcommand)]
enum IcalCmd {
    /// Display the parsed invite for a stored message.
    Show {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
    },
    /// Generate a METHOD:REPLY .ics for the given UID and print it.
    Reply {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
        /// accept | decline | tentative
        #[arg(long, value_parser = ["accept", "decline", "tentative"])]
        response: String,
    },
}

#[derive(Subcommand)]
enum JmapCmd {
    /// Show primary mailboxes from the JMAP session.
    Folders {
        #[arg(long)]
        account: Option<String>,
        /// Session URL (e.g. https://api.fastmail.com/jmap/session).
        #[arg(long)]
        session: String,
    },
    /// Pull recent Inbox headers via JMAP and merge into the local index.
    Fetch {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// Send a raw RFC 5322 message via Email/import + EmailSubmission/set.
    Send {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand)]
enum GraphCmd {
    /// List Graph mail folders.
    Folders {
        #[arg(long)]
        account: Option<String>,
    },
    /// Fetch headers from the Inbox (well-known "inbox" folder).
    Fetch {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Also download MIME bodies and write to Maildir.
        #[arg(long)]
        bodies: bool,
    },
    /// Read RFC 5322 from stdin and send via Graph.
    Send {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        no_save: bool,
    },
}

#[derive(Subcommand)]
enum OAuthCmd {
    /// Run the auth-code flow and save a refresh token to the keyring.
    Login {
        #[arg(long)]
        account: Option<String>,
    },
    /// Persist OAuth client credentials onto an account.
    SetClient {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// Forget the saved refresh token.
    Logout {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Subcommand)]
enum ContactsCmd {
    /// Add or update a contact.
    Add {
        #[arg(long)]
        account: Option<String>,
        email: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List contacts, frecency-ranked.
    List {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Substring match on email or name.
    Search {
        #[arg(long)]
        account: Option<String>,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Harvest contacts from all locally-stored messages.
    Harvest {
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove a contact.
    Remove {
        #[arg(long)]
        account: Option<String>,
        email: String,
    },
    /// Sync contacts from a CardDAV addressbook URL via REPORT.
    CardDav {
        #[arg(long)]
        account: Option<String>,
        /// Full addressbook URL (e.g. https://host/remote.php/dav/addressbooks/users/alice/contacts/).
        #[arg(long)]
        url: String,
        /// Username for HTTP basic auth (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Interactive add. Stores password in OS keyring.
    Add {
        /// Configure account for OAuth2 instead of an app password.
        #[arg(long, value_parser = ["gmail", "microsoft"])]
        oauth: Option<String>,
    },
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
            AccountCmd::Add { oauth } => cmd_accounts_add(oauth),
            AccountCmd::List => cmd_accounts_list(),
            AccountCmd::Folders { account } => cmd_accounts_folders(account).await,
        },
        Cmd::Fetch {
            account,
            bodies,
            body_limit,
            notify,
        } => cmd_fetch(account, bodies, body_limit, notify).await,
        Cmd::List {
            account,
            folder,
            limit,
        } => cmd_list(account, folder, limit).await,
        Cmd::Send {
            account,
            no_save,
            attachments,
        } => cmd_send(account, no_save, attachments).await,
        Cmd::Tui { account } => cmd_tui(account).await,
        Cmd::Contacts { action } => cmd_contacts(action).await,
        Cmd::OAuth { action } => cmd_oauth(action).await,
        Cmd::Graph { action } => cmd_graph(action).await,
        Cmd::Jmap { action } => cmd_jmap(action).await,
        Cmd::Search {
            account,
            query,
            limit,
        } => cmd_search(account, query, limit).await,
        Cmd::Thread { account, thread_id } => cmd_thread(account, thread_id).await,
        Cmd::Ical { action } => cmd_ical(action).await,
        Cmd::Draft { action } => cmd_draft(action).await,
        Cmd::Template { action } => cmd_template(action).await,
        Cmd::Watch { account, bodies } => cmd_watch(account, bodies).await,
        Cmd::Outbox { action } => cmd_outbox(action).await,
        Cmd::Flag {
            account,
            folder,
            uid,
            add,
            del,
        } => cmd_flag(account, folder, uid, add, del).await,
        Cmd::Folder { action } => cmd_folder(action).await,
        Cmd::Sieve { action } => cmd_sieve(action).await,
        Cmd::Unsubscribe {
            account,
            folder,
            uid,
            mailto,
            dry_run,
        } => cmd_unsubscribe(account, folder, uid, mailto, dry_run).await,
        Cmd::Export {
            account,
            folder,
            output,
        } => cmd_export(account, folder, output).await,
        Cmd::Import {
            account,
            folder,
            input,
            eml,
        } => cmd_import(account, folder, input, eml).await,
    }
}

async fn cmd_draft(action: DraftCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    let composer = match action {
        DraftCmd::New { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            inbx_composer::Composer::new_blank(inbx_composer::Identity::from_account(acct))
        }
        DraftCmd::Reply {
            account,
            folder,
            uid,
            all,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = read_message_raw(&acct.name, &folder, uid).await?;
            inbx_composer::Composer::new_reply(
                inbx_composer::Identity::from_account(acct),
                &raw,
                all,
            )?
        }
        DraftCmd::Forward {
            account,
            folder,
            uid,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = read_message_raw(&acct.name, &folder, uid).await?;
            inbx_composer::Composer::new_forward(inbx_composer::Identity::from_account(acct), &raw)?
        }
        DraftCmd::Save { account } => {
            return cmd_draft_save(account).await;
        }
    };
    let draft = composer.to_draft();
    use std::io::Write as _;
    std::io::stdout().write_all(draft.as_bytes())?;
    Ok(())
}

async fn cmd_template(action: TemplateCmd) -> Result<()> {
    use std::io::Read as _;
    let cfg = inbx_config::load()?;
    match action {
        TemplateCmd::List { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            for name in inbx_composer::templates::list(&acct.name)? {
                println!("{name}");
            }
        }
        TemplateCmd::Save {
            account,
            name,
            file,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = if file == "-" {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                buf
            } else {
                std::fs::read(&file)?
            };
            let path = inbx_composer::templates::save(&acct.name, &name, &raw)?;
            println!("saved {}", path.display());
        }
        TemplateCmd::Show { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = inbx_composer::templates::load_raw(&acct.name, &name)?;
            use std::io::Write as _;
            std::io::stdout().write_all(&raw)?;
        }
        TemplateCmd::Use { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let composer = inbx_composer::templates::from_template(
                inbx_composer::Identity::from_account(acct),
                &acct.name,
                &name,
            )?;
            use std::io::Write as _;
            std::io::stdout().write_all(composer.to_draft().as_bytes())?;
        }
        TemplateCmd::Remove { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            inbx_composer::templates::delete(&acct.name, &name)?;
            println!("removed {name}");
        }
    }
    Ok(())
}

/// Parse an existing RFC 5322 draft, hand it to a fresh Composer, attach the
/// requested files, and re-emit. Lets `inbx send --attach` work without the
/// caller knowing how to assemble multipart/mixed by hand.
fn rebuild_with_attachments(
    account: &Account,
    raw: &[u8],
    paths: &[std::path::PathBuf],
) -> Result<Vec<u8>> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .with_context(|| "parse draft for re-attach")?;
    let mut composer =
        inbx_composer::Composer::new_blank(inbx_composer::Identity::from_account(account));
    if let Some(s) = parsed.subject() {
        composer.subject.set_content(s);
    }
    if let Some(g) = parsed.to() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.to.set_content(&s);
        }
    }
    if let Some(g) = parsed.cc() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.cc.set_content(&s);
        }
    }
    if let Some(g) = parsed.bcc() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.bcc.set_content(&s);
        }
    }
    if let Some(b) = parsed.body_text(0) {
        composer.body.set_content(&b);
    }
    for p in paths {
        composer.attach_path(p)?;
    }
    Ok(composer.to_mime()?)
}

async fn cmd_draft_save(account: Option<String>) -> Result<()> {
    use std::io::Read as _;
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .context("read stdin")?;
    if raw.is_empty() {
        bail!("empty input on stdin");
    }
    let raw = normalize_crlf(raw);
    let mut session = inbx_net::connect_imap(&acct).await?;
    let folders = inbx_net::list_folders(&mut session).await?;
    let drafts = inbx_net::find_drafts_folder(&folders)
        .with_context(|| "no Drafts folder discovered on server")?;
    inbx_net::append_draft(&mut session, &drafts, &raw).await?;
    let _ = session.logout().await;
    println!("saved to {drafts}");
    Ok(())
}

async fn cmd_watch(account: Option<String>, bodies: bool) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct_name = pick_account(&cfg, account.as_deref())?.name.clone();
    loop {
        if let Err(e) = cmd_fetch(Some(acct_name.clone()), bodies, 200, true).await {
            tracing::warn!(%e, "fetch failed; backing off 30s");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        // Refresh acct from disk in case config changed.
        let cfg = inbx_config::load()?;
        let acct = pick_account(&cfg, Some(&acct_name))?.clone();
        match inbx_net::idle::wait_for_new(&acct).await {
            Ok(inbx_net::idle::IdleEvent::NewData) => {
                tracing::info!("new data signal");
            }
            Ok(inbx_net::idle::IdleEvent::Timeout) => {
                tracing::info!("idle keepalive cycle");
            }
            Err(e) => {
                tracing::warn!(%e, "idle error; backing off 30s");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    }
}

async fn cmd_outbox(action: OutboxCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        OutboxCmd::List { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_store::Store::open(&acct.name).await?;
            for r in store.outbox_list().await? {
                println!(
                    "{:>4}  attempts={}  next_retry={}  bytes={}  err={}",
                    r.id,
                    r.attempts,
                    format_unix(r.next_retry_unix),
                    r.raw.len(),
                    r.last_error.unwrap_or_default(),
                );
            }
        }
        OutboxCmd::Drain { account } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let store = inbx_store::Store::open(&acct.name).await?;
            let due = store.outbox_due().await?;
            let mut sent = 0usize;
            let mut failed = 0usize;
            for r in due {
                match inbx_net::send_message(&acct, &r.raw).await {
                    Ok(()) => {
                        store.outbox_delete(r.id).await?;
                        sent += 1;
                    }
                    Err(e) => {
                        store.outbox_record_failure(r.id, &e.to_string()).await?;
                        failed += 1;
                    }
                }
            }
            println!("drained: {sent} sent, {failed} failed");
        }
        OutboxCmd::Remove { account, id } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_store::Store::open(&acct.name).await?;
            store.outbox_delete(id).await?;
            println!("removed {id}");
        }
    }
    Ok(())
}

async fn cmd_flag(
    account: Option<String>,
    folder: String,
    uids: Vec<u32>,
    add: Vec<String>,
    del: Vec<String>,
) -> Result<()> {
    if add.is_empty() && del.is_empty() {
        bail!("at least one --add or --del flag required");
    }
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let mut session = inbx_net::connect_imap(&acct).await?;
    if !add.is_empty() {
        inbx_net::store_flags(&mut session, &folder, &uids, "+FLAGS", &add.join(" ")).await?;
    }
    if !del.is_empty() {
        inbx_net::store_flags(&mut session, &folder, &uids, "-FLAGS", &del.join(" ")).await?;
    }
    let _ = session.logout().await;
    println!("flags updated on {} message(s) in {folder}", uids.len());
    Ok(())
}

async fn cmd_folder(action: FolderCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        FolderCmd::Create { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let mut session = inbx_net::connect_imap(&acct).await?;
            inbx_net::create_folder(&mut session, &name).await?;
            let _ = session.logout().await;
            println!("created {name}");
        }
        FolderCmd::Delete { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let mut session = inbx_net::connect_imap(&acct).await?;
            inbx_net::delete_folder(&mut session, &name).await?;
            let _ = session.logout().await;
            println!("deleted {name}");
        }
        FolderCmd::Rename { account, from, to } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let mut session = inbx_net::connect_imap(&acct).await?;
            inbx_net::rename_folder(&mut session, &from, &to).await?;
            let _ = session.logout().await;
            println!("renamed {from} → {to}");
        }
        FolderCmd::Subscribe { account, name, on } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let mut session = inbx_net::connect_imap(&acct).await?;
            inbx_net::subscribe_folder(&mut session, &name, on).await?;
            let _ = session.logout().await;
            println!("{} {name}", if on { "subscribed" } else { "unsubscribed" });
        }
    }
    Ok(())
}

async fn cmd_sieve(action: SieveCmd) -> Result<()> {
    use std::io::Read as _;
    let cfg = inbx_config::load()?;
    match action {
        SieveCmd::List { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            let scripts = sv.list_scripts().await?;
            for s in scripts {
                println!("{} {}", if s.active { "*" } else { " " }, s.name);
            }
            sv.logout().await?;
        }
        SieveCmd::Get { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            let body = sv.get_script(&name).await?;
            println!("{body}");
            sv.logout().await?;
        }
        SieveCmd::Put {
            account,
            name,
            file,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mut body = String::new();
            if file == "-" {
                std::io::stdin().read_to_string(&mut body)?;
            } else {
                body = std::fs::read_to_string(&file)?;
            }
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            sv.put_script(&name, &body).await?;
            sv.logout().await?;
            println!("uploaded {name}");
        }
        SieveCmd::Activate { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            sv.set_active(&name).await?;
            sv.logout().await?;
            println!("activated {name}");
        }
        SieveCmd::Delete { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            sv.delete_script(&name).await?;
            sv.logout().await?;
            println!("deleted {name}");
        }
        SieveCmd::Vacation {
            account,
            name,
            days,
            subject,
            message,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let body = if message == "-" {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                message
            };
            let script = inbx_net::sieve::vacation_script(body.trim(), days, subject.as_deref());
            let mut sv = inbx_net::sieve::SieveClient::connect(acct).await?;
            sv.put_script(&name, &script).await?;
            sv.set_active(&name).await?;
            sv.logout().await?;
            println!("vacation script `{name}` activated for {days} days");
        }
    }
    Ok(())
}

async fn cmd_export(account: Option<String>, folder: String, output: String) -> Result<()> {
    use std::io::Write as _;
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;
    let rows = store.list_messages(&folder, u32::MAX).await?;

    let mut out: Box<dyn Write> = if output == "-" {
        Box::new(std::io::stdout().lock())
    } else {
        Box::new(std::fs::File::create(&output)?)
    };

    let mut count = 0usize;
    for m in rows {
        let Some(path) = m.maildir_path else { continue };
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(%path, %e, "skip unreadable");
                continue;
            }
        };
        let from = m.from_addr.as_deref().unwrap_or("MAILER-DAEMON");
        let date = m
            .date_unix
            .map(format_unix_rfc822)
            .unwrap_or_else(|| "Thu Jan  1 00:00:00 1970".into());
        writeln!(out, "From {from} {date}")?;
        // mbox From-quoting: lines starting with "From " get a > prepended.
        for line in raw.split(|b| *b == b'\n') {
            if line.starts_with(b"From ") {
                out.write_all(b">")?;
            }
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
        out.write_all(b"\n")?;
        count += 1;
    }
    eprintln!("exported {count} messages from {folder}");
    Ok(())
}

async fn cmd_import(
    account: Option<String>,
    folder: String,
    input: String,
    eml: bool,
) -> Result<()> {
    use std::io::Read as _;
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;

    let mut buf = Vec::new();
    if input == "-" {
        std::io::stdin().read_to_end(&mut buf)?;
    } else {
        buf = std::fs::read(&input)?;
    }

    store
        .upsert_folder(&inbx_store::FolderRow {
            name: folder.clone(),
            delim: None,
            special_use: None,
            attrs: None,
            uidvalidity: Some(0),
            uidnext: None,
            delta_link: None,
        })
        .await?;

    let messages: Vec<Vec<u8>> = if eml { vec![buf] } else { split_mbox(&buf) };

    let mut next_uid = 1i64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut imported = 0usize;
    for raw in messages {
        if raw.is_empty() {
            continue;
        }
        let parsed = mail_parser::MessageParser::default().parse(&raw);
        let subject = parsed
            .as_ref()
            .and_then(|p| p.subject().map(|s| s.to_string()));
        let from = parsed
            .as_ref()
            .and_then(|p| p.from())
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(|s| s.to_string());
        let to = parsed.as_ref().and_then(|p| {
            p.to().map(|g| {
                g.iter()
                    .filter_map(|a| a.address().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        });
        let date_unix = parsed
            .as_ref()
            .and_then(|p| p.date())
            .map(|d| d.to_timestamp());
        let path = store.write_maildir(&folder, &raw, "\\Seen")?;
        let row = inbx_store::MessageRow {
            folder: folder.clone(),
            uid: next_uid,
            uidvalidity: 0,
            message_id: parsed
                .as_ref()
                .and_then(|p| p.message_id().map(|s| s.to_string())),
            subject,
            from_addr: from,
            to_addrs: to,
            date_unix,
            flags: "\\Seen".into(),
            maildir_path: Some(path.to_string_lossy().into_owned()),
            headers_only: 0,
            fetched_at_unix: now,
            in_reply_to: None,
            refs: None,
            thread_id: None,
        };
        store.upsert_message(&row).await?;
        index_message(&store, &folder, next_uid, 0, &raw).await?;
        next_uid += 1;
        imported += 1;
    }
    println!("imported {imported} messages into {folder}");
    Ok(())
}

fn split_mbox(buf: &[u8]) -> Vec<Vec<u8>> {
    // Split at lines starting with "From " (the envelope separator).
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut at_line_start = true;
    let mut i = 0;
    while i < buf.len() {
        if at_line_start && buf[i..].starts_with(b"From ") {
            // Flush previous
            if !current.is_empty() {
                out.push(strip_from_quoting(std::mem::take(&mut current)));
            }
            // Skip the "From " line.
            while i < buf.len() && buf[i] != b'\n' {
                i += 1;
            }
            if i < buf.len() {
                i += 1;
            }
            continue;
        }
        let b = buf[i];
        current.push(b);
        at_line_start = b == b'\n';
        i += 1;
    }
    if !current.is_empty() {
        out.push(strip_from_quoting(current));
    }
    out
}

fn strip_from_quoting(buf: Vec<u8>) -> Vec<u8> {
    // Reverse mbox quoting: ">From " → "From ".
    let mut out = Vec::with_capacity(buf.len());
    let mut at_line_start = true;
    let mut i = 0;
    while i < buf.len() {
        if at_line_start && buf[i] == b'>' && buf[i + 1..].starts_with(b"From ") {
            i += 1;
        }
        out.push(buf[i]);
        at_line_start = buf[i] == b'\n';
        i += 1;
    }
    out
}

fn format_unix_rfc822(ts: i64) -> String {
    // Rough day-of-week + ASCII timestamp; precision not critical for mbox.
    let secs = ts.max(0);
    let days = secs / 86400;
    let dow = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    format!("{dow} Jan  1 00:00:{:02} 1970", secs % 60)
}

async fn cmd_unsubscribe(
    account: Option<String>,
    folder: String,
    uid: i64,
    use_mailto: bool,
    dry_run: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let raw = read_message_raw(&acct.name, &folder, uid).await?;
    let targets = inbx_net::unsubscribe::extract_targets(&raw)?;

    println!(
        "https:     {}\nmailto:    {}\none-click: {}",
        targets.https.as_deref().unwrap_or("—"),
        targets.mailto.as_deref().unwrap_or("—"),
        targets.one_click,
    );
    if dry_run {
        return Ok(());
    }

    if !use_mailto
        && targets.one_click
        && let Some(url) = targets.https.as_deref()
    {
        inbx_net::unsubscribe::one_click(url).await?;
        println!("one-click POST OK");
        return Ok(());
    }
    if let Some(m) = targets.mailto.as_deref() {
        inbx_net::unsubscribe::via_mailto(&acct, m).await?;
        println!("unsubscribe email sent to {m}");
        return Ok(());
    }
    bail!("no usable List-Unsubscribe target");
}

async fn cmd_ical(action: IcalCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        IcalCmd::Show {
            account,
            folder,
            uid,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = read_message_raw(&acct.name, &folder, uid).await?;
            let invite = inbx_ical::parse_message(&raw)?;
            print_invite(&invite);
        }
        IcalCmd::Reply {
            account,
            folder,
            uid,
            response,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let raw = read_message_raw(&acct.name, &folder, uid).await?;
            let invite = inbx_ical::parse_message(&raw)?;
            let r = match response.as_str() {
                "accept" => inbx_ical::RsvpResponse::Accept,
                "decline" => inbx_ical::RsvpResponse::Decline,
                "tentative" => inbx_ical::RsvpResponse::Tentative,
                _ => unreachable!(),
            };
            let attendee = format!("mailto:{}", acct.email);
            let ics = inbx_ical::build_reply(&invite, r, &attendee)?;
            println!("{ics}");
        }
    }
    Ok(())
}

async fn read_message_raw(account: &str, folder: &str, uid: i64) -> Result<Vec<u8>> {
    let store = inbx_store::Store::open(account).await?;
    let rows = store.list_messages(folder, u32::MAX).await?;
    let row = rows
        .into_iter()
        .find(|m| m.uid == uid)
        .with_context(|| format!("uid {uid} not in folder {folder}"))?;
    let path = row
        .maildir_path
        .with_context(|| "message body not yet fetched")?;
    Ok(std::fs::read(&path)?)
}

fn print_invite(inv: &inbx_ical::Invite) {
    println!("UID:      {}", inv.uid);
    if let Some(s) = &inv.summary {
        println!("Summary:  {s}");
    }
    if let Some(s) = &inv.organizer {
        println!("Organizer:{s}");
    }
    if let Some(s) = &inv.start {
        println!("Start:    {s}");
    }
    if let Some(s) = &inv.end {
        println!("End:      {s}");
    }
    if let Some(s) = &inv.location {
        println!("Location: {s}");
    }
    if !inv.attendees.is_empty() {
        println!("Attendees:");
        for a in &inv.attendees {
            println!("  {a}");
        }
    }
    if let Some(m) = &inv.method {
        println!("Method:   {m}");
    }
    if let Some(d) = &inv.description {
        println!("\nDescription:\n{d}");
    }
}

async fn cmd_search(account: Option<String>, query: String, limit: u32) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;
    for m in store.search(&query, limit).await? {
        let date = m.date_unix.map(format_unix).unwrap_or_else(|| "—".into());
        println!(
            "{:>10}  {:<30}  [{}]  {}",
            date,
            truncate(m.from_addr.as_deref().unwrap_or(""), 30),
            m.folder,
            m.subject.unwrap_or_default()
        );
    }
    Ok(())
}

async fn cmd_thread(account: Option<String>, thread_id: String) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;
    let rows = store.list_thread(&thread_id).await?;
    if rows.is_empty() {
        println!("(empty thread)");
        return Ok(());
    }
    for m in rows {
        let date = m.date_unix.map(format_unix).unwrap_or_else(|| "—".into());
        println!(
            "{:>10}  {}  {}",
            date,
            truncate(m.from_addr.as_deref().unwrap_or(""), 30),
            m.subject.unwrap_or_default()
        );
    }
    Ok(())
}

/// Parse the message, extract threading + indexable text, then update the
/// store's threading columns and FTS index for one row.
async fn index_message(
    store: &inbx_store::Store,
    folder: &str,
    uid: i64,
    uidvalidity: i64,
    raw: &[u8],
) -> Result<()> {
    let Some(parsed) = mail_parser::MessageParser::default().parse(raw) else {
        return Ok(());
    };
    let message_id = parsed.message_id().map(|s| s.to_string());
    let in_reply_to_first = parsed
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
            in_reply_to_first.as_deref(),
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

async fn cmd_jmap(action: JmapCmd) -> Result<()> {
    use std::io::Read as _;
    let cfg = inbx_config::load()?;
    match action {
        JmapCmd::Folders { account, session } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let client = inbx_net::jmap::JmapClient::connect(acct, &session).await?;
            for m in client.list_mailboxes().await? {
                println!(
                    "{:>5}u/{:<5}t  {:<10}  {}",
                    m.unread,
                    m.total,
                    m.role.unwrap_or_else(|| "-".into()),
                    m.name
                );
            }
        }
        JmapCmd::Fetch {
            account,
            session,
            limit,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let client = inbx_net::jmap::JmapClient::connect(acct, &session).await?;
            let store = inbx_store::Store::open(&acct.name).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            store
                .upsert_folder(&inbx_store::FolderRow {
                    name: "Inbox".into(),
                    delim: None,
                    special_use: Some("\\Inbox".into()),
                    attrs: None,
                    uidvalidity: Some(0),
                    uidnext: None,
                    delta_link: None,
                })
                .await?;
            let emails = client.fetch_inbox_headers(limit).await?;
            for (i, e) in emails.iter().enumerate() {
                let from = e
                    .from
                    .as_ref()
                    .and_then(|v| v.first())
                    .map(|a| a.formatted());
                let to = e.to.as_ref().map(|v| {
                    v.iter()
                        .map(|a| a.formatted())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                let date_unix = e.received_at.as_deref().and_then(parse_iso8601);
                store
                    .upsert_message(&inbx_store::MessageRow {
                        folder: "Inbox".into(),
                        uid: jmap_uid(&e.id, i),
                        uidvalidity: 0,
                        message_id: e.message_id.as_ref().and_then(|v| v.first()).cloned(),
                        subject: e.subject.clone(),
                        from_addr: from,
                        to_addrs: to,
                        date_unix,
                        flags: if e.is_seen() {
                            "\\Seen".into()
                        } else {
                            String::new()
                        },
                        maildir_path: None,
                        headers_only: 1,
                        fetched_at_unix: now,
                        in_reply_to: None,
                        refs: None,
                        thread_id: None,
                    })
                    .await?;
            }
            println!("Inbox: {} JMAP emails indexed", emails.len());
        }
        JmapCmd::Send { account, session } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let client = inbx_net::jmap::JmapClient::connect(acct, &session).await?;
            let mut raw = Vec::new();
            std::io::stdin()
                .read_to_end(&mut raw)
                .context("read stdin")?;
            if raw.is_empty() {
                bail!("empty input on stdin");
            }
            let raw = normalize_crlf(raw);
            client.send_mime(&raw).await?;
            println!("sent via JMAP");
        }
    }
    Ok(())
}

fn jmap_uid(id: &str, idx: usize) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let h = (h ^ (idx as u64)) & 0x7fff_ffff_ffff_ffff;
    h as i64
}

async fn cmd_graph(action: GraphCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        GraphCmd::Folders { account } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let client = inbx_net::graph::GraphClient::connect(&acct).await?;
            for f in client.list_folders().await? {
                println!(
                    "{:>5}u/{:<5}t  {}  {}",
                    f.unread,
                    f.total,
                    f.well_known.unwrap_or_else(|| "-".into()),
                    f.display_name
                );
            }
        }
        GraphCmd::Fetch {
            account,
            limit,
            bodies,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let client = inbx_net::graph::GraphClient::connect(&acct).await?;
            let store = inbx_store::Store::open(&acct.name).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            store
                .upsert_folder(&inbx_store::FolderRow {
                    name: "Inbox".into(),
                    delim: None,
                    special_use: None,
                    attrs: None,
                    uidvalidity: Some(0),
                    uidnext: None,
                    delta_link: None,
                })
                .await?;
            // Use the persisted deltaLink when present so we only pull
            // changes; first run still returns full state plus the new link.
            let prev_delta = store.get_delta_link("Inbox").await?;
            let _ = limit; // Graph delta is exhaustive — limit ignored here.
            let (messages, new_delta) = client
                .delta_messages("inbox", prev_delta.as_deref())
                .await?;
            if let Some(link) = new_delta.as_deref() {
                store.set_delta_link("Inbox", Some(link)).await?;
            }
            for (i, m) in messages.iter().enumerate() {
                let date_unix = m.received.as_deref().and_then(parse_iso8601);
                let from = m.from.as_ref().map(|r| r.formatted());
                let to = if m.to.is_empty() {
                    None
                } else {
                    Some(
                        m.to.iter()
                            .map(|r| r.formatted())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                };
                let row = inbx_store::MessageRow {
                    folder: "Inbox".into(),
                    // Synthesize a stable integer id from the Graph string id.
                    uid: graph_uid(&m.id, i),
                    uidvalidity: 0,
                    message_id: m.message_id.clone(),
                    subject: m.subject.clone(),
                    from_addr: from,
                    to_addrs: to,
                    date_unix,
                    flags: if m.is_read {
                        "\\Seen".into()
                    } else {
                        String::new()
                    },
                    maildir_path: None,
                    headers_only: 1,
                    fetched_at_unix: now,
                    in_reply_to: None,
                    refs: None,
                    thread_id: None,
                };
                store.upsert_message(&row).await?;
                if bodies {
                    let raw = client.fetch_mime(&m.id).await?;
                    let path = store.write_maildir(
                        "Inbox",
                        &raw,
                        if m.is_read { "\\Seen" } else { "" },
                    )?;
                    store
                        .set_maildir_path("Inbox", row.uid, 0, &path.to_string_lossy())
                        .await?;
                    index_message(&store, "Inbox", row.uid, 0, &raw).await?;
                }
            }
            println!("Inbox: {} messages indexed", messages.len());
        }
        GraphCmd::Send { account, no_save } => {
            use std::io::Read as _;
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let client = inbx_net::graph::GraphClient::connect(&acct).await?;
            let mut raw = Vec::new();
            std::io::stdin()
                .read_to_end(&mut raw)
                .context("read stdin")?;
            if raw.is_empty() {
                bail!("empty input on stdin");
            }
            let raw = normalize_crlf(raw);
            client.send_mime(&raw, !no_save).await?;
            println!("sent via Graph");
            if let Ok(contacts) = inbx_contacts::ContactsStore::open(&acct.name).await {
                let _ = contacts.harvest(&raw).await;
            }
        }
    }
    Ok(())
}

fn graph_uid(id: &str, idx: usize) -> i64 {
    // FNV-1a 64-bit, then sign-bit-clear so it fits comfortably in INTEGER.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let h = (h ^ (idx as u64)) & 0x7fff_ffff_ffff_ffff;
    h as i64
}

fn parse_iso8601(s: &str) -> Option<i64> {
    // Simple Z-suffixed parse: YYYY-MM-DDTHH:MM:SS(.ffff)?Z
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    Some(civil_to_unix(year, month, day, hour, minute, second))
}

fn civil_to_unix(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    // Howard Hinnant civil_to_days.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + (h as i64) * 3600 + (mi as i64) * 60 + (s as i64)
}

async fn cmd_oauth(action: OAuthCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        OAuthCmd::Login { account } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let provider = match &acct.auth {
                inbx_config::AuthMethod::OAuth2 { provider, .. } => provider.clone(),
                _ => bail!(
                    "{} is not configured for OAuth2; set auth.kind = oauth2 in config.toml",
                    acct.name
                ),
            };
            let token = inbx_net::oauth_login(&acct.auth, &provider).await?;
            inbx_config::store_refresh_token(&acct.name, &token.refresh)?;
            println!(
                "saved refresh token for {} (access token expires in {}s)",
                acct.name, token.expires_in
            );
        }
        OAuthCmd::SetClient {
            account,
            client_id,
            client_secret,
        } => {
            let mut cfg = cfg;
            let name = pick_account(&cfg, account.as_deref())?.name.clone();
            let acct = cfg.accounts.iter_mut().find(|a| a.name == name).unwrap();
            match &mut acct.auth {
                inbx_config::AuthMethod::OAuth2 {
                    client_id: c,
                    client_secret: s,
                    ..
                } => {
                    *c = Some(client_id);
                    *s = client_secret;
                }
                other => {
                    bail!("{} is not OAuth2 (auth = {other:?})", acct.name);
                }
            }
            inbx_config::save(&cfg)?;
            println!("updated OAuth client for {}", name);
        }
        OAuthCmd::Logout { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            inbx_config::delete_refresh_token(&acct.name)?;
            println!("removed refresh token for {}", acct.name);
        }
    }
    Ok(())
}

async fn cmd_contacts(action: ContactsCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        ContactsCmd::Add {
            account,
            email,
            name,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            store.upsert(&email, name.as_deref()).await?;
            println!("upserted {email}");
        }
        ContactsCmd::List { account, limit } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            let rows = store.list(limit).await?;
            for c in rows {
                let n = c.name.unwrap_or_default();
                println!(
                    "{:>4}  {}  <{}>",
                    c.frecency_count,
                    if n.is_empty() { "—" } else { &n },
                    c.email
                );
            }
        }
        ContactsCmd::Search {
            account,
            query,
            limit,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            for c in store.search(&query, limit).await? {
                let n = c.name.unwrap_or_default();
                println!(
                    "{:>4}  {}  <{}>",
                    c.frecency_count,
                    if n.is_empty() { "—" } else { &n },
                    c.email
                );
            }
        }
        ContactsCmd::Harvest { account } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let mail_store = inbx_store::Store::open(&acct.name).await?;
            let contacts = inbx_contacts::ContactsStore::open(&acct.name).await?;
            let folders = mail_store.list_folders().await?;
            let mut total = 0usize;
            for f in folders {
                for m in mail_store.list_messages(&f.name, u32::MAX).await? {
                    let Some(path) = m.maildir_path else { continue };
                    let raw = match std::fs::read(&path) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(%path, %e, "skip unreadable");
                            continue;
                        }
                    };
                    total += contacts.harvest(&raw).await?;
                }
            }
            println!("harvested {total} address occurrences");
        }
        ContactsCmd::CardDav { account, url, user } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let username = user.unwrap_or_else(|| acct.username.clone());
            let password = inbx_config::load_password(&acct.name)
                .with_context(|| format!("no password in keyring for {}", acct.name))?;
            let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            let report = inbx_contacts::carddav::sync(&url, &username, &password, &store).await?;
            println!(
                "carddav: {} vcards, {} addresses imported",
                report.vcards_seen, report.addresses_imported
            );
        }
        ContactsCmd::Remove { account, email } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            let removed = store.delete(&email).await?;
            println!(
                "{}",
                if removed {
                    format!("removed {email}")
                } else {
                    format!("(no such contact: {email})")
                }
            );
        }
    }
    Ok(())
}

async fn cmd_tui(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    tui::run(acct).await
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

fn cmd_accounts_add(oauth: Option<String>) -> Result<()> {
    let mut cfg = inbx_config::load()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut lock = stdin.lock();

    let name = prompt(&mut lock, &mut stdout, "account name (e.g. personal): ")?;
    if cfg.accounts.iter().any(|a| a.name == name) {
        bail!("account {name} already exists");
    }
    let email = prompt(&mut lock, &mut stdout, "email: ")?;

    // Provider-aware host defaults when OAuth is selected.
    let (default_imap_host, default_smtp_host) = match oauth.as_deref() {
        Some("gmail") => ("imap.gmail.com", "smtp.gmail.com"),
        Some("microsoft") => ("outlook.office365.com", "smtp.office365.com"),
        _ => ("", ""),
    };
    let imap_host_msg = if default_imap_host.is_empty() {
        "imap host: ".to_string()
    } else {
        format!("imap host [{default_imap_host}]: ")
    };
    let mut imap_host = prompt(&mut lock, &mut stdout, &imap_host_msg)?;
    if imap_host.is_empty() {
        imap_host = default_imap_host.to_string();
    }
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
    let smtp_host_msg = if default_smtp_host.is_empty() {
        "smtp host: ".to_string()
    } else {
        format!("smtp host [{default_smtp_host}]: ")
    };
    let mut smtp_host = prompt(&mut lock, &mut stdout, &smtp_host_msg)?;
    if smtp_host.is_empty() {
        smtp_host = default_smtp_host.to_string();
    }
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
    let username_default = email.clone();
    let username_msg = format!("username [{username_default}]: ");
    let mut username = prompt(&mut lock, &mut stdout, &username_msg)?;
    if username.is_empty() {
        username = username_default;
    }

    let auth = match oauth.as_deref() {
        Some("gmail") => {
            let client_id = prompt(&mut lock, &mut stdout, "oauth client_id: ")?;
            let client_secret = prompt(
                &mut lock,
                &mut stdout,
                "oauth client_secret (blank for none): ",
            )?;
            inbx_config::AuthMethod::OAuth2 {
                provider: inbx_config::OAuthProvider::Gmail,
                client_id: (!client_id.is_empty()).then_some(client_id),
                client_secret: (!client_secret.is_empty()).then_some(client_secret),
            }
        }
        Some("microsoft") => {
            let tenant = prompt(&mut lock, &mut stdout, "ms tenant [common]: ")?;
            let tenant = if tenant.is_empty() {
                "common".into()
            } else {
                tenant
            };
            let client_id = prompt(&mut lock, &mut stdout, "oauth client_id: ")?;
            let client_secret = prompt(
                &mut lock,
                &mut stdout,
                "oauth client_secret (blank for none): ",
            )?;
            inbx_config::AuthMethod::OAuth2 {
                provider: inbx_config::OAuthProvider::Microsoft { tenant },
                client_id: (!client_id.is_empty()).then_some(client_id),
                client_secret: (!client_secret.is_empty()).then_some(client_secret),
            }
        }
        _ => {
            let password =
                rpassword::prompt_password("password (app password): ").context("read password")?;
            inbx_config::store_password(&name, &password)?;
            inbx_config::AuthMethod::AppPassword
        }
    };

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
        auth,
    });
    inbx_config::save(&cfg)?;
    if oauth.is_some() {
        println!("added OAuth account {name}; run `inbx oauth login --account {name}`");
    } else {
        println!("added account {name}; password stored in keyring");
    }
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

async fn cmd_fetch(
    account: Option<String>,
    fetch_bodies: bool,
    body_limit: u32,
    notify: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();

    tracing::info!(account = %acct.name, "connecting");
    let mut session = inbx_net::connect_imap(&acct).await?;
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
                delta_link: None,
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
    let pre_max = store
        .folder_max_uid("INBOX", uidvalidity as i64)
        .await?
        .unwrap_or(0);
    store
        .upsert_folder(&inbx_store::FolderRow {
            name: "INBOX".into(),
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
                in_reply_to: None,
                refs: None,
                thread_id: None,
            })
            .await?;
    }
    println!("INBOX: {} messages indexed", rows.len());

    let new_count = rows.iter().filter(|h| (h.uid as i64) > pre_max).count();
    if notify && new_count > 0 {
        let summary = format!("{} new in {}", new_count, acct.name);
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
                index_message(&store, "INBOX", uid as i64, uidvalidity as i64, &raw).await?;
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

async fn cmd_send(
    account: Option<String>,
    no_save: bool,
    attachments: Vec<std::path::PathBuf>,
) -> Result<()> {
    use std::io::Read as _;

    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();

    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .context("read stdin")?;
    if raw.is_empty() {
        bail!("empty input on stdin");
    }
    // Normalize bare-LF to CRLF for SMTP wire format.
    let raw = normalize_crlf(raw);

    // Re-emit through the composer when attachments are requested so the
    // outgoing MIME is multipart/mixed with the parts attached.
    let raw = if attachments.is_empty() {
        raw
    } else {
        rebuild_with_attachments(&acct, &raw, &attachments)?
    };

    tracing::info!(account = %acct.name, bytes = raw.len(), "sending");
    if let Err(e) = inbx_net::send_message(&acct, &raw).await {
        tracing::warn!(%e, "send failed; queueing in outbox");
        let store = inbx_store::Store::open(&acct.name).await?;
        let id = store.outbox_enqueue(&raw).await?;
        store.outbox_record_failure(id, &e.to_string()).await?;
        println!("queued in outbox (id={id}); run `inbx outbox drain` to retry");
        return Ok(());
    }
    println!("sent");

    if let Ok(contacts) = inbx_contacts::ContactsStore::open(&acct.name).await {
        let _ = contacts.harvest(&raw).await;
    }

    if no_save {
        return Ok(());
    }

    tracing::info!("appending to Sent folder");
    let mut session = inbx_net::connect_imap(&acct).await?;
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
