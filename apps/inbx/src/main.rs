mod mbox;
mod tui;

use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Tee tracing output to stderr (pretty) and a daily-rotated file under
/// `$XDG_STATE_HOME/inbx/log/inbx.YYYY-MM-DD`. Returns the worker guard
/// that must outlive the process so buffered records are flushed at exit.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let dirs = directories::ProjectDirs::from("sh", "kryptic", "inbx");
    let state_dir = dirs.as_ref().map(|d| d.data_local_dir().join("log"));

    let (file_layer, guard) = match state_dir {
        Some(path) => {
            if std::fs::create_dir_all(&path).is_ok() {
                let appender = tracing_appender::rolling::daily(&path, "inbx");
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

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use inbx_config::{Account, Config, TlsMode};

/// ASCII-art banner. Regenerate with:
///
/// ```sh
/// figlet -f "ANSI Regular" inbx > apps/inbx/src/art.txt
/// ```
const LONG_ABOUT: &str = concat!(
    "\n",
    include_str!("art.txt"),
    "\nmodal-vim email client · v",
    env!("CARGO_PKG_VERSION"),
);

#[derive(Parser)]
#[command(
    name = "inbx",
    version,
    about = "modal-vim email client",
    long_about = LONG_ABOUT,
)]
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
    /// Fetch headers + discover folders for an account.
    Fetch {
        #[arg(long)]
        account: Option<String>,
        /// Folder to sync. Defaults to INBOX.
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Sync every selectable folder instead of just one.
        #[arg(long)]
        all: bool,
        /// Only sync messages newer than this many days (0 = all).
        #[arg(long, default_value_t = 0)]
        since: u32,
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
    /// Print a stored message: rendered body + auth banner + header summary.
    Show {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
    },
    /// Print all RFC 5322 headers of a stored message.
    Headers {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
    },
    /// Print the raw body of a stored message (no rendering).
    Body {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        uid: i64,
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
    #[command(name = "oauth")]
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
    /// CalDAV calendar sync (pull / discover).
    Cal {
        #[command(subcommand)]
        action: CalCmd,
    },
    /// Export messages from local index to mbox or single .eml.
    Export {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Output path; `-` for stdout.
        #[arg(long, default_value = "-")]
        output: String,
        /// Export a single message as raw RFC 5322 (.eml), no mbox envelope.
        #[arg(long)]
        eml: bool,
        /// UID of the single message to export (required with --eml).
        #[arg(long)]
        uid: Option<i64>,
        /// Skip messages older than this Unix timestamp.
        #[arg(long)]
        since: Option<i64>,
        /// Maximum number of messages to export.
        #[arg(long)]
        limit: Option<usize>,
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
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Also download bodies for each new batch.
        #[arg(long)]
        bodies: bool,
    },
    /// Print shell completion script (bash | zsh | fish | elvish | powershell).
    Completion {
        #[arg(value_parser = clap::value_parser!(clap_complete::Shell))]
        shell: clap_complete::Shell,
    },
    /// Outbound queue for offline / failed sends.
    Outbox {
        #[command(subcommand)]
        action: OutboxCmd,
    },
    /// Sugar over `flag` for common verbs.
    Mark {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// One of: read, unread, star, unstar, trash.
        #[arg(value_parser = ["read", "unread", "star", "unstar", "trash"])]
        verb: String,
        #[arg(num_args = 1.., required = true)]
        uid: Vec<u32>,
    },
    /// EXPUNGE messages flagged \Deleted in a folder.
    Expunge {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        folder: String,
    },
    /// UID MOVE messages between folders (RFC 6851).
    Mv {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, num_args = 1.., required = true)]
        uid: Vec<u32>,
    },
    /// UID COPY messages between folders.
    Cp {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value = "INBOX")]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, num_args = 1.., required = true)]
        uid: Vec<u32>,
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
    /// PGP key management and file crypto operations.
    Pgp {
        #[command(subcommand)]
        cmd: PgpCmd,
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
    /// Polling watch via Email/changes. Logs newly created/updated/destroyed ids.
    Watch {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        session: String,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 30)]
        interval: u64,
    },
    /// Real-time push via the JMAP EventSource (RFC 8620 §7.3).
    Push {
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
enum PgpCmd {
    /// Generate an inbx-managed Ed25519 keypair for an account.
    Keygen {
        #[arg(long)]
        account: Option<String>,
        /// Override identity name (defaults to account email's local part).
        #[arg(long)]
        name: Option<String>,
        /// Passphrase. If omitted, prompts on stdin.
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// List keys available via the account's key source.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Export a public key to stdout as ASCII armor.
    Export {
        #[arg(long)]
        account: Option<String>,
        /// Hex fingerprint. Defaults to the account's configured key.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Sign a file (detached, ASCII-armor signature on stdout).
    Sign {
        #[arg(long)]
        account: Option<String>,
        path: std::path::PathBuf,
    },
    /// Verify a detached signature against a file.
    Verify {
        #[arg(long)]
        account: Option<String>,
        /// Path to the signed file.
        path: std::path::PathBuf,
        /// Path to the detached signature (ASCII armor).
        #[arg(long)]
        sig: std::path::PathBuf,
        /// Path to the signer's pubkey ASCII armor (optional; defaults to
        /// the account's configured key for self-verify smoke tests).
        #[arg(long)]
        pubkey: Option<std::path::PathBuf>,
    },
    /// Encrypt a file to one or more pubkey files. Output written to
    /// `<path>.pgp` unless `--out` is given.
    Encrypt {
        #[arg(long)]
        account: Option<String>,
        path: std::path::PathBuf,
        #[arg(long)]
        recipient: Vec<std::path::PathBuf>,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Decrypt a file. Output to stdout.
    Decrypt {
        #[arg(long)]
        account: Option<String>,
        path: std::path::PathBuf,
    },
    /// Look up a recipient's public key via WKD and write it to disk.
    LookupWkd {
        /// Email to look up.
        email: String,
        /// Output path. Defaults to <email>.pub.asc in the current account's
        /// inbx-managed dir (so it's auto-discovered as an encrypt recipient).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
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
    /// CardDAV addressbook operations (pull / push / discover).
    Carddav {
        #[command(subcommand)]
        action: CarddavCmd,
    },
}

#[derive(Subcommand)]
enum CarddavCmd {
    /// Sync VCARDs from an addressbook URL via REPORT.
    Pull {
        #[arg(long)]
        account: Option<String>,
        /// Addressbook URL. With `--discover`, treat this as a server
        /// base URL and walk the RFC 6764 PROPFIND chain.
        #[arg(long)]
        url: String,
        /// HTTP basic-auth username (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
        /// PROPFIND-walk current-user-principal → addressbook-home →
        /// resourcetype to find the addressbook URL automatically.
        #[arg(long)]
        discover: bool,
    },
    /// PUT one local contact into the addressbook as a VCARD.
    Push {
        #[arg(long)]
        account: Option<String>,
        /// Addressbook base URL (without trailing filename).
        #[arg(long)]
        url: String,
        /// HTTP basic-auth username (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
        /// Email of the local contact to push.
        email: String,
    },
    /// PROPFIND-walk to enumerate addressbooks at a server base URL.
    Discover {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        url: String,
        #[arg(long)]
        user: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RsvpArg {
    Accept,
    Decline,
    Tentative,
}

impl From<RsvpArg> for inbx_ical::RsvpResponse {
    fn from(a: RsvpArg) -> Self {
        match a {
            RsvpArg::Accept => Self::Accept,
            RsvpArg::Decline => Self::Decline,
            RsvpArg::Tentative => Self::Tentative,
        }
    }
}

#[derive(Subcommand)]
enum CalCmd {
    /// CalDAV calendar operations (pull / discover).
    Caldav {
        #[command(subcommand)]
        action: CaldavCmd,
    },
    /// Send an RSVP reply to a stored calendar event.
    Rsvp {
        /// Event UID as written by `inbx cal caldav pull` (the iCalendar UID,
        /// not the sanitized filename — sanitization is applied for you).
        uid: String,

        /// Response status.
        #[arg(value_enum)]
        response: RsvpArg,

        /// Account name to send from.
        #[arg(long)]
        account: String,
    },
    /// Upload a local .ics to a CalDAV calendar. Updates if the UID already
    /// exists in the local index; creates otherwise.
    Put {
        /// Path to the local .ics file to upload.
        path: std::path::PathBuf,
        /// CalDAV calendar URL the event belongs to (must match the URL used
        /// for the most recent `cal caldav pull`).
        #[arg(long)]
        url: String,
        #[arg(long)]
        account: String,
        /// HTTP basic-auth username (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
    },
    /// Delete a stored event by UID, both from the server and locally.
    Delete {
        uid: String,
        /// CalDAV calendar URL the event belongs to.
        #[arg(long)]
        url: String,
        #[arg(long)]
        account: String,
        /// HTTP basic-auth username (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
        /// Skip the If-Match guard. Use only when the local etag is known stale.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum CaldavCmd {
    /// Sync VEVENTs from a calendar URL via REPORT.
    Pull {
        #[arg(long)]
        account: Option<String>,
        /// Calendar URL. With `--discover`, treat this as a server base URL
        /// and walk the RFC 6764 PROPFIND chain.
        #[arg(long)]
        url: String,
        /// HTTP basic-auth username (defaults to account.username).
        #[arg(long)]
        user: Option<String>,
        /// PROPFIND-walk current-user-principal → calendar-home-set →
        /// resourcetype to find the calendar URL automatically.
        #[arg(long)]
        discover: bool,
    },
    /// PROPFIND-walk to enumerate calendars at a server base URL.
    Discover {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        url: String,
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
    /// Connect to IMAP, list folders, logout. Reports OK or the error.
    Test {
        #[arg(long)]
        account: Option<String>,
    },
    /// Show folders cached locally for an account.
    Folders {
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove the account from config.toml and clear its keyring entries.
    Remove {
        #[arg(long)]
        account: Option<String>,
        /// Also delete the account's data directory (Maildir + SQLite).
        #[arg(long)]
        purge: bool,
    },
    /// Edit one or more fields on an existing account.
    Edit {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        imap_host: Option<String>,
        #[arg(long)]
        imap_port: Option<u16>,
        #[arg(long, value_parser = ["tls", "starttls"])]
        imap_security: Option<String>,
        #[arg(long)]
        smtp_host: Option<String>,
        #[arg(long)]
        smtp_port: Option<u16>,
        #[arg(long, value_parser = ["tls", "starttls"])]
        smtp_security: Option<String>,
        #[arg(long)]
        username: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = init_logging();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Config => cmd_config(),
        Cmd::Accounts { action } => match action {
            AccountCmd::Add { oauth } => cmd_accounts_add(oauth),
            AccountCmd::List => cmd_accounts_list(),
            AccountCmd::Test { account } => cmd_accounts_test(account).await,
            AccountCmd::Folders { account } => cmd_accounts_folders(account).await,
            AccountCmd::Remove { account, purge } => cmd_accounts_remove(account, purge),
            AccountCmd::Edit {
                account,
                email,
                imap_host,
                imap_port,
                imap_security,
                smtp_host,
                smtp_port,
                smtp_security,
                username,
            } => cmd_accounts_edit(
                account,
                email,
                imap_host,
                imap_port,
                imap_security,
                smtp_host,
                smtp_port,
                smtp_security,
                username,
            ),
        },
        Cmd::Fetch {
            account,
            folder,
            all,
            since,
            bodies,
            body_limit,
            notify,
        } => {
            if all {
                cmd_fetch_all(account, since, bodies, body_limit, notify).await
            } else {
                cmd_fetch(account, folder, since, bodies, body_limit, notify).await
            }
        }
        Cmd::List {
            account,
            folder,
            limit,
        } => cmd_list(account, folder, limit).await,
        Cmd::Show {
            account,
            folder,
            uid,
        } => cmd_show(account, folder, uid).await,
        Cmd::Headers {
            account,
            folder,
            uid,
        } => cmd_headers(account, folder, uid).await,
        Cmd::Body {
            account,
            folder,
            uid,
        } => cmd_body(account, folder, uid).await,
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
        Cmd::Cal { action } => cmd_cal(action).await,
        Cmd::Draft { action } => cmd_draft(action).await,
        Cmd::Template { action } => cmd_template(action).await,
        Cmd::Watch {
            account,
            folder,
            bodies,
        } => cmd_watch(account, folder, bodies).await,
        Cmd::Completion { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "inbx", &mut std::io::stdout());
            Ok(())
        }
        Cmd::Outbox { action } => cmd_outbox(action).await,
        Cmd::Mark {
            account,
            folder,
            verb,
            uid,
        } => cmd_mark(account, folder, verb, uid).await,
        Cmd::Expunge { account, folder } => cmd_expunge(account, folder).await,
        Cmd::Mv {
            account,
            from,
            to,
            uid,
        } => cmd_move_or_copy(account, from, to, uid, false).await,
        Cmd::Cp {
            account,
            from,
            to,
            uid,
        } => cmd_move_or_copy(account, from, to, uid, true).await,
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
        Cmd::Pgp { cmd } => cmd_pgp(cmd).await,
        Cmd::Export {
            account,
            folder,
            output,
            eml,
            uid,
            since,
            limit,
        } => cmd_export(account, folder, output, eml, uid, since, limit).await,
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
        composer.set_subject(s);
    }
    if let Some(g) = parsed.to() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.set_to(&s);
        }
    }
    if let Some(g) = parsed.cc() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.set_cc(&s);
        }
    }
    if let Some(g) = parsed.bcc() {
        let s = g
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !s.is_empty() {
            composer.set_bcc(&s);
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
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
    let folders = provider.list_folders().await?;
    let drafts = inbx_net::find_drafts_folder(&folders)
        .with_context(|| "no Drafts folder discovered on server")?;
    provider.append_draft(&drafts, &raw).await?;
    drop(provider);
    println!("saved to {drafts}");
    Ok(())
}

async fn cmd_watch(account: Option<String>, folder: String, bodies: bool) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let acct_name = acct.name.clone();
    // Transport dispatch — JMAP forwards to its push subcommand; Graph runs a
    // delta-link poll loop; IMAP runs the classic IDLE-with-fetch loop.
    if let inbx_config::Transport::Jmap { session_url } = &acct.transport {
        return cmd_jmap(JmapCmd::Push {
            account: Some(acct_name),
            session: session_url.clone(),
        })
        .await;
    }
    loop {
        // Drain outbox before each fetch so retries piggyback on the connect.
        if let Err(e) = drain_outbox_silent(&acct_name).await {
            tracing::warn!(%e, "outbox drain failed; will retry next cycle");
        }
        if let Err(e) = cmd_fetch(
            Some(acct_name.clone()),
            folder.clone(),
            0,
            bodies,
            200,
            true,
        )
        .await
        {
            tracing::warn!(%e, "fetch failed; backing off 30s");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        // Refresh acct from disk in case config changed.
        let cfg = inbx_config::load()?;
        let acct = pick_account(&cfg, Some(&acct_name))?.clone();
        match &acct.transport {
            inbx_config::Transport::Imap => {
                match inbx_net::idle::wait_for_new_in(&acct, &folder).await {
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
            inbx_config::Transport::Graph => {
                // Delta-link poll: 75s between cycles when idle, signal
                // immediately on changes by falling through to the next fetch.
                if let Err(e) = graph_delta_tick(&acct, &folder).await {
                    tracing::warn!(%e, "Graph delta tick failed; backing off 30s");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            }
            inbx_config::Transport::Jmap { .. } => unreachable!("forwarded above"),
        }
    }
}

/// One Graph delta-poll cycle for `inbx watch`. Returns when there are
/// changes (caller fetches next iteration) or after a 75 s sleep on no-change.
async fn graph_delta_tick(acct: &inbx_config::Account, folder: &str) -> Result<()> {
    let store = inbx_store::Store::open(&acct.name).await?;
    let client = inbx_net::graph::GraphClient::connect(acct).await?;
    let folders = client.list_folders().await?;
    let folder_id = folders
        .iter()
        .find(|f| f.display_name.eq_ignore_ascii_case(folder))
        .map(|f| f.id.clone())
        .ok_or_else(|| anyhow::anyhow!("Graph folder {folder} not found"))?;
    let stored = store.get_delta_link(folder).await?;
    let (messages, new_link) = client.delta_messages(&folder_id, stored.as_deref()).await?;
    if let Err(e) = store.set_delta_link(folder, new_link.as_deref()).await {
        tracing::warn!(%e, "Graph: set_delta_link failed (ignored)");
    }
    if messages.is_empty() {
        tracing::debug!("Graph delta: no changes; sleeping 75s");
        tokio::time::sleep(std::time::Duration::from_secs(75)).await;
    } else {
        tracing::info!(count = messages.len(), "Graph delta: new messages");
    }
    Ok(())
}

/// Best-effort drain — only attempts due rows; failures bubble back into the
/// outbox row's exponential backoff. Caller logs at warn level if the whole
/// drain itself errors (DB unreachable, account vanished, etc.).
async fn drain_outbox_silent(acct_name: &str) -> Result<()> {
    let cfg = inbx_config::load()?;
    let Some(acct) = cfg.accounts.iter().find(|a| a.name == acct_name).cloned() else {
        return Ok(());
    };
    let store = inbx_store::Store::open(&acct.name).await?;
    let due = store.outbox_due().await?;
    for r in due {
        match inbx_net::send_message(&acct, &r.raw).await {
            Ok(()) => {
                store.outbox_delete(r.id).await?;
                tracing::info!(id = r.id, "outbox: sent");
            }
            Err(e) => {
                store.outbox_record_failure(r.id, &e.to_string()).await?;
                tracing::warn!(id = r.id, %e, "outbox: still failing");
            }
        }
    }
    Ok(())
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

async fn cmd_mark(
    account: Option<String>,
    folder: String,
    verb: String,
    uids: Vec<u32>,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let (add_flag, remove_flag): (&str, &str) = match verb.as_str() {
        "read" => ("\\Seen", ""),
        "unread" => ("", "\\Seen"),
        "star" => ("\\Flagged", ""),
        "unstar" => ("", "\\Flagged"),
        "trash" => ("\\Deleted", ""),
        _ => unreachable!(),
    };
    let add: Vec<&str> = if add_flag.is_empty() {
        vec![]
    } else {
        vec![add_flag]
    };
    let remove: Vec<&str> = if remove_flag.is_empty() {
        vec![]
    } else {
        vec![remove_flag]
    };
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
    for &uid in &uids {
        provider
            .set_flags(&folder, uid as i64, &add, &remove)
            .await?;
    }
    drop(provider);
    let local_uids: Vec<i64> = uids.iter().map(|u| *u as i64).collect();
    store
        .mutate_flags(&folder, &local_uids, &add, &remove)
        .await?;
    println!("{verb}: {} message(s) in {folder}", uids.len());
    Ok(())
}

async fn cmd_expunge(account: Option<String>, folder: String) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
    let n = provider.expunge_folder(&folder).await?;
    drop(provider);
    let purged = store.purge_deleted(&folder).await?;
    println!("expunged {n} server / {purged} local rows in {folder}");
    Ok(())
}

async fn cmd_move_or_copy(
    account: Option<String>,
    from: String,
    to: String,
    uids: Vec<u32>,
    copy: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    if copy {
        // UID COPY has no cross-protocol equivalent on JMAP/Graph — bail unless IMAP.
        if !matches!(acct.transport, inbx_config::Transport::Imap) {
            bail!(
                "inbx cp not yet supported on JMAP/Graph; use inbx mv or the provider-specific subcommand"
            );
        }
        let mut session = inbx_net::connect_imap(&acct).await?;
        inbx_net::uid_copy(&mut session, &from, &uids, &to).await?;
        let _ = session.logout().await;
        println!("copied {} message(s) {from} → {to}", uids.len());
    } else {
        let store = inbx_store::Store::open(&acct.name).await?;
        let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
        for &uid in &uids {
            provider.move_message(&from, uid as i64, &to).await?;
        }
        drop(provider);
        let local: Vec<i64> = uids.iter().map(|u| *u as i64).collect();
        store.delete_messages(&from, &local).await?;
        println!("moved {} message(s) {from} → {to}", uids.len());
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
    let add_refs: Vec<&str> = add.iter().map(|s| s.as_str()).collect();
    let del_refs: Vec<&str> = del.iter().map(|s| s.as_str()).collect();
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
    for &uid in &uids {
        provider
            .set_flags(&folder, uid as i64, &add_refs, &del_refs)
            .await?;
    }
    drop(provider);
    let local: Vec<i64> = uids.iter().map(|u| *u as i64).collect();
    store
        .mutate_flags(&folder, &local, &add_refs, &del_refs)
        .await?;
    println!("flags updated on {} message(s) in {folder}", uids.len());
    Ok(())
}

async fn cmd_folder(action: FolderCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        FolderCmd::Create { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let store = inbx_store::Store::open(&acct.name).await?;
            let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
            provider.create_folder(&name).await?;
            drop(provider);
            println!("created {name}");
        }
        FolderCmd::Delete { account, name } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let store = inbx_store::Store::open(&acct.name).await?;
            let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
            provider.delete_folder(&name).await?;
            drop(provider);
            println!("deleted {name}");
        }
        FolderCmd::Rename { account, from, to } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let store = inbx_store::Store::open(&acct.name).await?;
            let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
            provider.rename_folder(&from, &to).await?;
            drop(provider);
            println!("renamed {from} → {to}");
        }
        FolderCmd::Subscribe { account, name, on } => {
            let acct = pick_account(&cfg, account.as_deref())?.clone();
            let store = inbx_store::Store::open(&acct.name).await?;
            let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
            provider.subscribe_folder(&name, on).await?;
            drop(provider);
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

async fn cmd_export(
    account: Option<String>,
    folder: String,
    output: String,
    eml: bool,
    uid: Option<i64>,
    since: Option<i64>,
    limit: Option<usize>,
) -> Result<()> {
    use std::io::Write as _;
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let store = inbx_store::Store::open(&acct.name).await?;

    // .eml mode: export exactly one message as raw RFC 5322, no mbox envelope.
    if eml {
        let target_uid = uid.with_context(|| "--uid is required when --eml is set")?;
        let raw = read_message_raw(&acct.name, &folder, target_uid).await?;
        if output == "-" {
            std::io::stdout().write_all(&raw)?;
        } else {
            std::fs::write(&output, &raw)?;
        }
        return Ok(());
    }

    let rows = store.list_messages(&folder, u32::MAX).await?;

    let mut out: Box<dyn Write> = if output == "-" {
        Box::new(std::io::stdout().lock())
    } else {
        Box::new(std::fs::File::create(&output)?)
    };

    let mut count = 0usize;
    for m in rows {
        // --since filter
        if let Some(since_ts) = since
            && m.date_unix.unwrap_or(0) < since_ts
        {
            continue;
        }
        // --limit cap
        if let Some(lim) = limit
            && count >= lim
        {
            break;
        }
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
            .map(mbox::format_unix_from_line)
            .unwrap_or_else(|| "Thu Jan  1 00:00:00 1970".into());
        writeln!(out, "From {from} {date}")?;
        // Inject Status: / X-Status: headers reflecting local flags.
        let enriched = mbox::inject_status_headers(&raw, &m.flags);
        // mbox From-quoting.
        out.write_all(&mbox::apply_from_quoting(&enriched))?;
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

    let messages: Vec<Vec<u8>> = if eml {
        vec![buf]
    } else {
        mbox::split_mbox(&buf)
    };

    // Start UIDs after the highest existing one in this folder so re-imports
    // don't collide with prior rows. uidvalidity = 0 for locally-imported
    // folders (matches the upsert_folder call above).
    let mut next_uid = store.folder_max_uid(&folder, 0).await?.unwrap_or(0) + 1;
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
        // Derive flags from RFC 4155 Status: / X-Status: headers when present;
        // fall back to empty (no flags) rather than defaulting to \Seen.
        let flags = {
            let f = mbox::flags_from_status_headers(&raw);
            if f.is_empty() { String::new() } else { f }
        };
        let path = store.write_maildir(&folder, &raw, &flags)?;
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
            flags,
            maildir_path: Some(path.to_string_lossy().into_owned()),
            headers_only: 0,
            fetched_at_unix: now,
            in_reply_to: None,
            refs: None,
            thread_id: None,
            provider_id: None,
        };
        store.upsert_message(&row).await?;
        index_message(&store, &folder, next_uid, 0, &raw).await?;
        next_uid += 1;
        imported += 1;
    }
    println!("imported {imported} messages into {folder}");
    Ok(())
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
                        provider_id: Some(e.id.clone()),
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
        JmapCmd::Push { account, session } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let client = inbx_net::jmap::JmapClient::connect(acct, &session).await?;
            let mut stream = client.open_event_source().await?;
            tracing::info!("EventSource open");
            while let Some(payload) = stream.next_event().await? {
                println!("{payload}");
            }
            println!("(stream closed)");
        }
        JmapCmd::Watch {
            account,
            session,
            interval,
        } => {
            let acct = pick_account(&cfg, account.as_deref())?;
            let client = inbx_net::jmap::JmapClient::connect(acct, &session).await?;
            let mut state = client.current_state().await?;
            println!("watching from state {state} (poll every {interval}s)");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                match client.changes(&state).await {
                    Ok(ch) => {
                        if !ch.created.is_empty()
                            || !ch.updated.is_empty()
                            || !ch.destroyed.is_empty()
                        {
                            tracing::info!(
                                created = ch.created.len(),
                                updated = ch.updated.len(),
                                destroyed = ch.destroyed.len(),
                                "JMAP changes"
                            );
                            // Hydrate created so the user sees subjects on the fly.
                            for h in client.fetch_by_ids(&ch.created).await.unwrap_or_default() {
                                println!(
                                    "+ {}  {}",
                                    h.from
                                        .as_ref()
                                        .and_then(|v| v.first())
                                        .map(|a| a.formatted())
                                        .unwrap_or_default(),
                                    h.subject.as_deref().unwrap_or(""),
                                );
                            }
                        }
                        state = ch.new_state;
                    }
                    Err(e) => {
                        tracing::warn!(%e, "Email/changes failed; continuing");
                    }
                }
            }
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
                    provider_id: Some(m.id.clone()),
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
            let token = inbx_net::oauth_login(&acct.auth, &provider, acct.proxy.as_ref()).await?;
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
            let mut store = inbx_contacts::ContactsStore::open(&acct.name).await?;
            if let Some(carddav_cfg) = &acct.carddav {
                let pw = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                store = store.with_carddav(carddav_cfg, &acct.username, pw);
            }
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
        ContactsCmd::Carddav { action } => match action {
            CarddavCmd::Push {
                account,
                url,
                email,
                user,
            } => {
                let acct = pick_account(&cfg, account.as_deref())?.clone();
                let username = user.unwrap_or_else(|| acct.username.clone());
                let password = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
                let matches = store.search(&email, 1).await?;
                let contact = matches
                    .into_iter()
                    .find(|c| c.email.eq_ignore_ascii_case(&email))
                    .with_context(|| format!("no local contact for {email}"))?;
                let uid = format!("inbx-{}", email.replace('@', "-at-"));
                let vcard = inbx_contacts::carddav::build_vcard(
                    &contact.email,
                    contact.name.as_deref(),
                    Some(&uid),
                );
                let resource_url = format!("{}/{uid}.vcf", url.trim_end_matches('/'));
                inbx_contacts::carddav::put_vcard(
                    &resource_url,
                    &username,
                    &password,
                    &vcard,
                    inbx_contacts::carddav::PutMode::CreateOnly,
                )
                .await?;
                println!("pushed {email} → {resource_url}");
            }
            CarddavCmd::Pull {
                account,
                url,
                user,
                discover,
            } => {
                let acct = pick_account(&cfg, account.as_deref())?.clone();
                let username = user.unwrap_or_else(|| acct.username.clone());
                let password = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                let store = inbx_contacts::ContactsStore::open(&acct.name).await?;
                let target_url = if discover {
                    let books =
                        inbx_contacts::carddav::discover(&url, &username, &password).await?;
                    if books.is_empty() {
                        bail!("no addressbooks discovered at {url}");
                    }
                    let pick = &books[0];
                    println!(
                        "discovered {} addressbook(s); syncing {} ({})",
                        books.len(),
                        pick.display_name.clone().unwrap_or_else(|| "?".into()),
                        pick.url
                    );
                    pick.url.clone()
                } else {
                    url
                };
                let report =
                    inbx_contacts::carddav::sync(&target_url, &username, &password, &store).await?;
                println!(
                    "carddav: {} vcards, {} addresses imported",
                    report.vcards_seen, report.addresses_imported
                );
            }
            CarddavCmd::Discover { account, url, user } => {
                let acct = pick_account(&cfg, account.as_deref())?.clone();
                let username = user.unwrap_or_else(|| acct.username.clone());
                let password = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                let books = inbx_contacts::carddav::discover(&url, &username, &password).await?;
                if books.is_empty() {
                    println!("(no addressbooks found)");
                }
                for b in books {
                    println!(
                        "{:<40}  {}",
                        b.display_name.unwrap_or_else(|| "?".into()),
                        b.url
                    );
                }
            }
        },
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

async fn cmd_cal(action: CalCmd) -> Result<()> {
    let cfg = inbx_config::load()?;
    match action {
        CalCmd::Caldav { action } => match action {
            CaldavCmd::Pull {
                account,
                url,
                user,
                discover,
            } => {
                let acct = pick_account(&cfg, account.as_deref())?.clone();
                let username = user.unwrap_or_else(|| acct.username.clone());
                let password = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                let target_url = if discover {
                    let cals = inbx_ical::caldav::discover(&url, &username, &password).await?;
                    if cals.is_empty() {
                        bail!("no calendars discovered at {url}");
                    }
                    let pick = &cals[0];
                    println!(
                        "discovered {} calendar(s); syncing {} ({})",
                        cals.len(),
                        pick.display_name.clone().unwrap_or_else(|| "?".into()),
                        pick.url
                    );
                    pick.url.clone()
                } else {
                    url
                };
                let store_dir = inbx_config::data_dir()
                    .map(|d| d.join(&acct.name).join("calendar"))
                    .unwrap_or_else(|_| {
                        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join(".local")
                            .join("share")
                            .join("inbx")
                            .join(&acct.name)
                            .join("calendar")
                    });
                let report =
                    inbx_ical::caldav::sync(&target_url, &username, &password, &store_dir).await?;
                println!(
                    "caldav: {} events, {} stored to {}",
                    report.events_seen,
                    report.events_stored,
                    store_dir.display()
                );
            }
            CaldavCmd::Discover { account, url, user } => {
                let acct = pick_account(&cfg, account.as_deref())?.clone();
                let username = user.unwrap_or_else(|| acct.username.clone());
                let password = inbx_config::load_password(&acct.name)
                    .with_context(|| format!("no password in keyring for {}", acct.name))?;
                let cals = inbx_ical::caldav::discover(&url, &username, &password).await?;
                if cals.is_empty() {
                    println!("(no calendars found)");
                }
                for c in cals {
                    println!(
                        "{:<40}  {}{}",
                        c.display_name.unwrap_or_else(|| "?".into()),
                        c.url,
                        c.color.map(|col| format!("  [{col}]")).unwrap_or_default()
                    );
                }
            }
        },
        CalCmd::Rsvp {
            uid,
            response,
            account,
        } => cmd_cal_rsvp(&uid, response, &account).await?,
        CalCmd::Put {
            path,
            url,
            account,
            user,
        } => cmd_cal_put(&path, &url, &account, user.as_deref()).await?,
        CalCmd::Delete {
            uid,
            url,
            account,
            user,
            force,
        } => cmd_cal_delete(&uid, &url, &account, user.as_deref(), force).await?,
    }
    Ok(())
}

async fn cmd_cal_rsvp(uid: &str, response: RsvpArg, account_name: &str) -> Result<()> {
    let cfg = inbx_config::load()?;
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.name == account_name)
        .ok_or_else(|| anyhow::anyhow!("no account named '{account_name}' in config"))?
        .clone();

    // Locate the stored .ics written by `inbx cal caldav pull`.
    // The path is <data_dir>/<account>/calendar/<sanitized_uid>.ics.
    let store_dir = inbx_config::data_dir()
        .map(|d| d.join(&account.name).join("calendar"))
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local")
                .join("share")
                .join("inbx")
                .join(&account.name)
                .join("calendar")
        });
    let safe_uid = inbx_ical::caldav::sanitize_uid(uid);
    let ics_path = store_dir.join(format!("{safe_uid}.ics"));
    if !ics_path.exists() {
        anyhow::bail!(
            "no stored event for uid '{uid}' under {}",
            store_dir.display()
        );
    }
    let text = std::fs::read_to_string(&ics_path)
        .with_context(|| format!("reading {}", ics_path.display()))?;
    let invite =
        inbx_ical::parse_ics(&text).map_err(|e| anyhow::anyhow!("parse_ics failed: {e}"))?;

    // Find the ATTENDEE line matching our account email.
    // Different servers format ATTENDEE differently — sometimes with
    // parameters before the value, so a case-insensitive substring match
    // is more robust than strict URI equality.
    let our_addr = account.email.to_ascii_lowercase();
    let attendee = invite
        .attendees
        .iter()
        .find(|a| a.to_ascii_lowercase().contains(&our_addr))
        .cloned()
        .unwrap_or_else(|| format!("mailto:{}", account.email));
    // build_reply requires the "mailto:" prefix.
    let attendee = if attendee.to_ascii_lowercase().starts_with("mailto:") {
        attendee
    } else {
        format!("mailto:{attendee}")
    };

    let rsvp_response: inbx_ical::RsvpResponse = response.into();
    let reply_ics = inbx_ical::build_reply(&invite, rsvp_response, &attendee)
        .map_err(|e| anyhow::anyhow!("build_reply failed: {e}"))?;

    // Resolve organizer → To address. invite.organizer is a "mailto:foo@bar"
    // URI (possibly with parameters); strip the scheme for the MIME To header.
    let organizer = invite
        .organizer
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("event has no ORGANIZER; cannot reply"))?;
    let to_addr = organizer.splitn(2, ':').last().unwrap_or(organizer);

    let summary = invite.summary.as_deref().unwrap_or("(no subject)");
    let verb = match rsvp_response {
        inbx_ical::RsvpResponse::Accept => "Accepted",
        inbx_ical::RsvpResponse::Decline => "Declined",
        inbx_ical::RsvpResponse::Tentative => "Tentative",
    };
    let subject = format!("{verb}: {summary}");
    let body_text = format!(
        "{verb} the invitation to \"{summary}\".\n\n\
         (Sent by inbx — RSVP reply, RFC 5546 METHOD:REPLY attached.)\n",
    );

    use mail_builder::MessageBuilder;
    use mail_builder::mime::MimePart;

    let raw = MessageBuilder::new()
        .from((account.name.as_str(), account.email.as_str()))
        .to(to_addr)
        .subject(subject)
        .body(MimePart::new(
            "multipart/alternative",
            vec![
                MimePart::new("text/plain", body_text),
                MimePart::new("text/calendar; method=REPLY", reply_ics),
            ],
        ))
        .write_to_vec()?;

    inbx_net::send_message(&account, &raw).await?;
    println!("rsvp {verb} sent for uid '{uid}' to {to_addr}");
    Ok(())
}

/// Strip scheme + host from a full URL to produce a root-relative path,
/// mirroring what a PROPFIND response typically returns for an href.
fn url_to_relative_href(event_url: &str) -> String {
    if let Some(idx) = event_url.find("://")
        && let Some(slash_idx) = event_url[idx + 3..].find('/')
    {
        return event_url[idx + 3 + slash_idx..].to_string();
    }
    event_url.to_string()
}

async fn cmd_cal_put(
    path: &std::path::Path,
    url: &str,
    account_name: &str,
    user: Option<&str>,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.name == account_name)
        .ok_or_else(|| anyhow::anyhow!("no account named '{account_name}' in config"))?
        .clone();
    let username = user.unwrap_or(account.username.as_str()).to_string();
    let password = inbx_config::load_password(&account.name)
        .with_context(|| format!("no password in keyring for {}", account.name))?;

    let ics =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let invite =
        inbx_ical::parse_ics(&ics).map_err(|e| anyhow::anyhow!("parse_ics failed: {e}"))?;
    let uid = invite.uid;

    let store_dir = inbx_config::data_dir()?
        .join(&account.name)
        .join("calendar");
    std::fs::create_dir_all(&store_dir)?;
    let mut index = inbx_ical::caldav::load_index(&store_dir)?;

    let (event_url, if_match, was_update) = match index.find_by_uid(&uid) {
        Some(entry) => {
            let abs = inbx_dav::absolutize(url, &entry.href);
            (abs, Some(entry.etag.clone()), true)
        }
        None => (inbx_ical::caldav::derive_event_href(url, &uid), None, false),
    };

    let new_etag =
        inbx_ical::caldav::put_event(&event_url, &username, &password, &ics, if_match.as_deref())
            .await?;

    // Write the local .ics to keep the store in sync with the server.
    let safe_uid = inbx_ical::caldav::sanitize_uid(&uid);
    let filename = format!("{safe_uid}.ics");
    std::fs::write(store_dir.join(&filename), ics.as_bytes())?;

    if was_update {
        if let Some(entry) = index.find_by_uid_mut(&uid) {
            entry.etag = new_etag.clone();
        }
    } else {
        let href = url_to_relative_href(&event_url);
        index.entries.push(inbx_ical::caldav::IndexEntry {
            href,
            uid: uid.clone(),
            etag: new_etag.clone(),
            filename,
        });
    }
    inbx_ical::caldav::save_index(&store_dir, &index)?;

    let verb = if was_update { "updated" } else { "created" };
    println!("{verb} event '{uid}' (etag={new_etag})");
    Ok(())
}

async fn cmd_cal_delete(
    uid: &str,
    url: &str,
    account_name: &str,
    user: Option<&str>,
    force: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.name == account_name)
        .ok_or_else(|| anyhow::anyhow!("no account named '{account_name}' in config"))?
        .clone();
    let username = user.unwrap_or(account.username.as_str()).to_string();
    let password = inbx_config::load_password(&account.name)
        .with_context(|| format!("no password in keyring for {}", account.name))?;

    let store_dir = inbx_config::data_dir()?
        .join(&account.name)
        .join("calendar");
    let mut index = inbx_ical::caldav::load_index(&store_dir)?;

    let entry = index
        .find_by_uid(uid)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no stored event with uid '{uid}' in {}",
                store_dir.display()
            )
        })?
        .clone();

    let event_url = inbx_dav::absolutize(url, &entry.href);
    let if_match = if force {
        None
    } else {
        Some(entry.etag.as_str())
    };

    inbx_ical::caldav::delete_event(&event_url, &username, &password, if_match).await?;

    // Remove from index and delete local file.
    index.remove_by_uid(uid);
    let _ = std::fs::remove_file(store_dir.join(&entry.filename));
    inbx_ical::caldav::save_index(&store_dir, &index)?;

    println!("deleted event '{uid}'");
    Ok(())
}

async fn cmd_tui(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    tui::run(acct).await
}

// ── PGP command handlers ──────────────────────────────────────────────────────

/// Resolve the per-account inbx-managed directory.
/// Uses `pgp.managed_dir` if set; otherwise `<data_dir>/<account>/pgp/`.
fn managed_dir_for(acct: &inbx_config::Account) -> std::path::PathBuf {
    if let Some(pgp) = &acct.pgp
        && let Some(dir) = &pgp.managed_dir
    {
        return dir.clone();
    }
    inbx_config::data_dir()
        .map(|d| d.join(&acct.name).join("pgp"))
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local")
                .join("share")
                .join("inbx")
                .join(&acct.name)
                .join("pgp")
        })
}

async fn cmd_pgp(cmd: PgpCmd) -> Result<()> {
    match cmd {
        PgpCmd::Keygen {
            account,
            name,
            passphrase,
        } => cmd_pgp_keygen(account, name, passphrase).await,
        PgpCmd::List { account } => cmd_pgp_list(account).await,
        PgpCmd::Export {
            account,
            fingerprint,
        } => cmd_pgp_export(account, fingerprint).await,
        PgpCmd::Sign { account, path } => cmd_pgp_sign(account, path).await,
        PgpCmd::Verify {
            account,
            path,
            sig,
            pubkey,
        } => cmd_pgp_verify(account, path, sig, pubkey).await,
        PgpCmd::Encrypt {
            account,
            path,
            recipient,
            out,
        } => cmd_pgp_encrypt(account, path, recipient, out).await,
        PgpCmd::Decrypt { account, path } => cmd_pgp_decrypt(account, path).await,
        PgpCmd::LookupWkd {
            email,
            out,
            account,
        } => cmd_pgp_lookup_wkd(account, email, out).await,
    }
}

async fn cmd_pgp_keygen(
    account: Option<String>,
    name_override: Option<String>,
    passphrase_flag: Option<String>,
) -> Result<()> {
    let mut cfg = inbx_config::load()?;
    let acct_name = pick_account(&cfg, account.as_deref())?.name.clone();

    // Check pgp config; refuse if key_source is explicitly GnuPG.
    let needs_write = {
        let acct = cfg.accounts.iter().find(|a| a.name == acct_name).unwrap();
        match &acct.pgp {
            Some(pgp) => {
                if pgp.key_source == inbx_pgp::KeySourceKind::Gnupg {
                    bail!(
                        "this command writes inbx-managed keys; \
                         set `pgp.key_source = \"inbx-managed\"` in your config first"
                    );
                }
                false
            }
            None => true, // write default PgpConfig
        }
    };

    if needs_write {
        let acct = cfg
            .accounts
            .iter_mut()
            .find(|a| a.name == acct_name)
            .unwrap();
        acct.pgp = Some(inbx_pgp::PgpConfig {
            key_source: inbx_pgp::KeySourceKind::InbxManaged,
            key_fingerprint: None,
            managed_dir: None,
            prefer_encrypt_mutual: true,
        });
        inbx_config::save(&cfg)?;
        let cfg_path = inbx_config::config_path()?;
        println!("wrote default pgp config to {}", cfg_path.display());
    }

    let acct = cfg
        .accounts
        .iter()
        .find(|a| a.name == acct_name)
        .unwrap()
        .clone();
    let email = &acct.email;
    let name = name_override.unwrap_or_else(|| {
        // Default: local part of email.
        email.split('@').next().unwrap_or(email).to_string()
    });

    let passphrase = match passphrase_flag {
        Some(p) => p,
        None => rpassword::prompt_password("pgp passphrase: ")?,
    };

    let managed_dir = managed_dir_for(&acct);
    let (key_id, sec_path) =
        inbx_pgp::inbx_managed::keygen(&managed_dir, &name, email, &passphrase).await?;

    println!("fingerprint: {}", key_id.0);
    println!("secret key:  {}", sec_path.display());
    println!(
        "\nShare your public key with:\n  inbx pgp export --account {acct_name} --fingerprint {}",
        key_id.0
    );
    Ok(())
}

async fn cmd_pgp_list(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct.pgp.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "no pgp config for account {}; run `inbx pgp keygen` first",
            acct.name
        )
    })?;
    let src = inbx_pgp::key_source_for(pgp)?;
    let keys = src.list_keys().await?;
    if keys.is_empty() {
        println!("(no keys found)");
    } else {
        for (key_id, uid) in keys {
            println!("{}  {}", key_id.0, uid);
        }
    }
    Ok(())
}

async fn cmd_pgp_export(account: Option<String>, fingerprint: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct
        .pgp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no pgp config for account {}", acct.name))?;
    let src = inbx_pgp::key_source_for(pgp)?;

    let key_id = fingerprint
        .or_else(|| pgp.key_fingerprint.clone())
        .map(inbx_pgp::KeyId)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no fingerprint given and no pgp.key_fingerprint in config; \
                 pass --fingerprint HEXFPR"
            )
        })?;

    let armor = src.export_public(&key_id).await?;
    use std::io::Write as _;
    std::io::stdout().write_all(armor.0.as_bytes())?;
    Ok(())
}

async fn cmd_pgp_sign(account: Option<String>, path: std::path::PathBuf) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct
        .pgp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no pgp config for account {}", acct.name))?;
    let src = inbx_pgp::key_source_for(pgp)?;

    let key_id = pgp
        .key_fingerprint
        .as_deref()
        .map(|s| inbx_pgp::KeyId(s.to_string()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "pgp.key_fingerprint not set; run `inbx pgp keygen` or set it in config"
            )
        })?;

    let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let sig = src.sign_detached(&key_id, &data).await?;
    use std::io::Write as _;
    std::io::stdout().write_all(&sig.0)?;
    Ok(())
}

async fn cmd_pgp_verify(
    account: Option<String>,
    path: std::path::PathBuf,
    sig_path: std::path::PathBuf,
    pubkey_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct
        .pgp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no pgp config for account {}", acct.name))?;
    let src = inbx_pgp::key_source_for(pgp)?;

    let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let sig_bytes =
        std::fs::read(&sig_path).with_context(|| format!("read {}", sig_path.display()))?;
    let sig = inbx_pgp::Signature(sig_bytes);

    // Resolve pubkey: from file arg, or from account's configured key.
    let pubkey = if let Some(pk_path) = pubkey_path {
        let armor = std::fs::read_to_string(&pk_path)
            .with_context(|| format!("read pubkey {}", pk_path.display()))?;
        inbx_pgp::ArmoredKey(armor)
    } else {
        let key_id = pgp
            .key_fingerprint
            .as_deref()
            .map(|s| inbx_pgp::KeyId(s.to_string()))
            .ok_or_else(|| {
                anyhow::anyhow!("no --pubkey given and pgp.key_fingerprint not set in config")
            })?;
        src.export_public(&key_id).await?
    };

    let result = src.verify_detached(&pubkey, &data, &sig).await?;
    if result.valid {
        println!(
            "VALID  signer={}  uid={}",
            result.signer_fingerprint.unwrap_or_default(),
            result.signer_uid.unwrap_or_default()
        );
    } else {
        eprintln!("INVALID signature");
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_pgp_encrypt(
    account: Option<String>,
    path: std::path::PathBuf,
    recipient_paths: Vec<std::path::PathBuf>,
    out_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct
        .pgp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no pgp config for account {}", acct.name))?;
    let src = inbx_pgp::key_source_for(pgp)?;

    if recipient_paths.is_empty() {
        bail!("at least one --recipient pubkey file is required");
    }

    let mut recipients = Vec::new();
    for rp in &recipient_paths {
        let armor = std::fs::read_to_string(rp)
            .with_context(|| format!("read recipient pubkey {}", rp.display()))?;
        recipients.push(inbx_pgp::ArmoredKey(armor));
    }

    let plaintext = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let ciphertext = src.encrypt_to(&recipients, &plaintext).await?;

    let out = out_path.unwrap_or_else(|| {
        let mut p = path.clone();
        let ext = p
            .extension()
            .map(|e| format!("{}.pgp", e.to_string_lossy()))
            .unwrap_or_else(|| "pgp".to_string());
        p.set_extension(ext);
        p
    });
    std::fs::write(&out, &ciphertext.0).with_context(|| format!("write {}", out.display()))?;
    println!("encrypted to {}", out.display());
    Ok(())
}

async fn cmd_pgp_decrypt(account: Option<String>, path: std::path::PathBuf) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let pgp = acct
        .pgp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no pgp config for account {}", acct.name))?;
    let src = inbx_pgp::key_source_for(pgp)?;

    let raw = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let ciphertext = inbx_pgp::Ciphertext(raw);
    let (plain, _verify) = src.decrypt(&ciphertext).await?;
    use std::io::Write as _;
    std::io::stdout().write_all(&plain.0)?;
    Ok(())
}

async fn cmd_pgp_lookup_wkd(
    account: Option<String>,
    email: String,
    out: Option<std::path::PathBuf>,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let client = inbx_net::proxy::build_reqwest_client(acct.proxy.as_ref(), 10)
        .with_context(|| "build wkd http client")?;
    match inbx_pgp::wkd::lookup_with_client(&client, &email).await {
        Ok(Some(key)) => {
            // Resolve output path: explicit --out, or <managed_dir>/<fpr>.pub.asc
            // (matches keygen's filename convention so the encrypt path's
            // "load all *.pub.asc" picks it up consistently).
            let out_path = match out {
                Some(p) => p,
                None => {
                    let dir = managed_dir_for(acct);
                    std::fs::create_dir_all(&dir)
                        .with_context(|| format!("create {}", dir.display()))?;
                    dir.join(format!("{}.pub.asc", key.fingerprint))
                }
            };
            std::fs::write(&out_path, key.armored.0.as_bytes())
                .with_context(|| format!("write {}", out_path.display()))?;
            println!("fingerprint: {}", key.fingerprint);
            println!("email:       {}", email);
            println!("written to:  {}", out_path.display());
            Ok(())
        }
        Ok(None) => {
            eprintln!("no WKD key found for {email}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("WKD parse error for {email}: {e}");
            std::process::exit(1);
        }
    }
}

// ── end PGP handlers ──────────────────────────────────────────────────────────

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

    // Provider lookup via the autoconfig table. Falls back to a generic
    // imap.<domain> / smtp.<domain> guess when the domain isn't known.
    let suggestion = inbx_config::autoconfig::suggest(&email);
    if let Some(s) = &suggestion {
        match &s.source {
            inbx_config::autoconfig::SuggestionSource::BuiltIn { name } => {
                println!("autoconfig: {name} provider detected");
            }
            inbx_config::autoconfig::SuggestionSource::DomainGuess => {
                println!(
                    "autoconfig: unknown provider — guessing imap.{{domain}} / smtp.{{domain}}"
                );
            }
        }
    }
    let default_imap_host = suggestion
        .as_ref()
        .map(|s| s.imap_host.as_str())
        .unwrap_or("");
    let default_smtp_host = suggestion
        .as_ref()
        .map(|s| s.smtp_host.as_str())
        .unwrap_or("");
    let suggested_imap_security = suggestion
        .as_ref()
        .map(|s| s.imap_security)
        .unwrap_or(TlsMode::Tls);
    let suggested_smtp_security = suggestion
        .as_ref()
        .map(|s| s.smtp_security)
        .unwrap_or(TlsMode::Tls);
    let suggested_imap_port = suggestion.as_ref().map(|s| s.imap_port).unwrap_or(993);
    let suggested_smtp_port = suggestion.as_ref().map(|s| s.smtp_port).unwrap_or(465);

    let imap_host_msg = if default_imap_host.is_empty() {
        "imap host: ".to_string()
    } else {
        format!("imap host [{default_imap_host}]: ")
    };
    let mut imap_host = prompt(&mut lock, &mut stdout, &imap_host_msg)?;
    if imap_host.is_empty() {
        imap_host = default_imap_host.to_string();
    }
    let sec_default = match suggested_imap_security {
        TlsMode::Tls => "tls",
        TlsMode::Starttls => "starttls",
    };
    let imap_security = prompt_tls_with_default(
        &mut lock,
        &mut stdout,
        &format!("imap security [{sec_default}]: "),
        suggested_imap_security,
    )?;
    let imap_port = prompt_port(
        &mut lock,
        &mut stdout,
        &format!("imap port [{suggested_imap_port}]: "),
        suggested_imap_port,
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
    let smtp_sec_default = match suggested_smtp_security {
        TlsMode::Tls => "tls",
        TlsMode::Starttls => "starttls",
    };
    let smtp_security = prompt_tls_with_default(
        &mut lock,
        &mut stdout,
        &format!("smtp security [{smtp_sec_default}]: "),
        suggested_smtp_security,
    )?;
    let _ = suggested_smtp_port;
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
        transport: inbx_config::Transport::Imap,
        pgp: None,
        proxy: None,
        carddav: None,
    });
    inbx_config::save(&cfg)?;
    if oauth.is_some() {
        println!("added OAuth account {name}; run `inbx oauth login --account {name}`");
    } else {
        println!("added account {name}; password stored in keyring");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_accounts_edit(
    account: Option<String>,
    email: Option<String>,
    imap_host: Option<String>,
    imap_port: Option<u16>,
    imap_security: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    smtp_security: Option<String>,
    username: Option<String>,
) -> Result<()> {
    let mut cfg = inbx_config::load()?;
    let name = pick_account(&cfg, account.as_deref())?.name.clone();
    let acct = cfg.accounts.iter_mut().find(|a| a.name == name).unwrap();
    if let Some(v) = email {
        acct.email = v;
    }
    if let Some(v) = imap_host {
        acct.imap_host = v;
    }
    if let Some(v) = imap_port {
        acct.imap_port = v;
    }
    if let Some(v) = imap_security {
        acct.imap_security = match v.as_str() {
            "tls" => TlsMode::Tls,
            "starttls" => TlsMode::Starttls,
            _ => unreachable!(),
        };
    }
    if let Some(v) = smtp_host {
        acct.smtp_host = v;
    }
    if let Some(v) = smtp_port {
        acct.smtp_port = v;
    }
    if let Some(v) = smtp_security {
        acct.smtp_security = match v.as_str() {
            "tls" => TlsMode::Tls,
            "starttls" => TlsMode::Starttls,
            _ => unreachable!(),
        };
    }
    if let Some(v) = username {
        acct.username = v;
    }
    inbx_config::save(&cfg)?;
    println!("updated {name}");
    Ok(())
}

fn cmd_accounts_remove(account: Option<String>, purge: bool) -> Result<()> {
    let mut cfg = inbx_config::load()?;
    let name = pick_account(&cfg, account.as_deref())?.name.clone();
    cfg.accounts.retain(|a| a.name != name);
    inbx_config::save(&cfg)?;
    let _ = inbx_config::delete_password(&name);
    let _ = inbx_config::delete_refresh_token(&name);
    if purge
        && let Ok(dir) = inbx_config::data_dir().map(|d| d.join(&name))
        && dir.exists()
    {
        std::fs::remove_dir_all(&dir)?;
        println!("purged {}", dir.display());
    }
    println!("removed {name}");
    Ok(())
}

async fn cmd_accounts_test(account: Option<String>) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let started = std::time::Instant::now();
    match inbx_net::connect_imap(&acct).await {
        Ok(mut session) => {
            let folders = inbx_net::list_folders(&mut session).await?;
            let _ = session.logout().await;
            println!(
                "OK  {}  imap={}:{} ({:?})  {} folders  in {}ms",
                acct.name,
                acct.imap_host,
                acct.imap_port,
                acct.imap_security,
                folders.len(),
                started.elapsed().as_millis()
            );
        }
        Err(e) => {
            println!("FAIL  {}  {}", acct.name, e);
            return Err(e.into());
        }
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

async fn cmd_fetch_all(
    account: Option<String>,
    since: u32,
    bodies: bool,
    body_limit: u32,
    notify: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;
    let folders = provider.list_folders().await?;
    drop(provider);
    let mut total = 0usize;
    for f in folders {
        if !f.selectable {
            continue;
        }
        match cmd_fetch(
            Some(acct.name.clone()),
            f.name.clone(),
            since,
            bodies,
            body_limit,
            notify,
        )
        .await
        {
            Ok(()) => total += 1,
            Err(e) => {
                tracing::warn!(folder = %f.name, %e, "fetch failed; continuing");
            }
        }
    }
    println!("synced {total} folder(s)");
    Ok(())
}

async fn cmd_fetch(
    account: Option<String>,
    folder: String,
    since: u32,
    fetch_bodies: bool,
    body_limit: u32,
    notify: bool,
) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?.clone();

    tracing::info!(account = %acct.name, transport = ?acct.transport, "connecting");
    let store = inbx_store::Store::open(&acct.name).await?;
    let mut provider = inbx_net::connect_provider(&acct, Some(&store)).await?;

    tracing::info!("listing folders");
    let folders = provider.list_folders().await?;
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

    tracing::info!(folder = %folder, since, "fetching headers");
    let (uidvalidity, rows) = if since > 0 && matches!(acct.transport, inbx_config::Transport::Imap)
    {
        // Date-filtered fast path — UID SEARCH SINCE <date> then fetch
        // only those UIDs.  The MailProvider trait has no days-ago filter,
        // so we drop to a raw IMAP session just for this step.
        let mut session = inbx_net::connect_imap(&acct).await?;
        let uids = inbx_net::search_since(&mut session, &folder, since).await?;
        let (uv, fetched) =
            inbx_net::fetch_headers_uids(&mut session, &folder, Some(&uids)).await?;
        let _ = session.logout().await;
        (uv as i64, fetched)
    } else {
        if since > 0 {
            tracing::warn!("--since ignored: not yet supported on JMAP / Graph");
        }
        let rows = provider.fetch_headers(&folder, None, body_limit).await?;
        // Derive UIDVALIDITY from first row; JMAP/Graph rows carry uidvalidity=1.
        let uv: i64 = rows.first().map(|r| r.uidvalidity as i64).unwrap_or(1);
        (uv, rows)
    };

    let prev = store.folder_uidvalidity(&folder).await?;
    if let Some(prev) = prev
        && prev != uidvalidity
        && uidvalidity != 0
    {
        tracing::warn!(prev, new = uidvalidity, %folder, "UIDVALIDITY changed; wiping");
        store.wipe_folder_messages(&folder).await?;
    }
    let pre_max = store
        .folder_max_uid(&folder, uidvalidity)
        .await?
        .unwrap_or(0);
    store
        .upsert_folder(&inbx_store::FolderRow {
            name: folder.clone(),
            delim: None,
            special_use: None,
            attrs: None,
            uidvalidity: Some(uidvalidity),
            uidnext: None,
            delta_link: None,
        })
        .await?;
    for h in &rows {
        store
            .upsert_message(&inbx_store::MessageRow {
                folder: folder.clone(),
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
    println!("{folder}: {} messages indexed", rows.len());

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
        let pending = store.list_unfetched(&folder, body_limit).await?;
        if !pending.is_empty() {
            tracing::info!(count = pending.len(), "fetching bodies");
            // Open contacts store once for Autocrypt harvest (best-effort).
            let contacts = inbx_contacts::ContactsStore::open(&acct.name).await.ok();
            let pairs = provider.fetch_bodies(&folder, &pending).await?;
            for (uid, raw) in pairs {
                let path = store.write_maildir(&folder, &raw, "\\Seen")?;
                store
                    .set_maildir_path(&folder, uid, uidvalidity, &path.to_string_lossy())
                    .await?;
                index_message(&store, &folder, uid, uidvalidity, &raw).await?;
                // Harvest Autocrypt: header into contacts (best-effort).
                if let Some(cs) = &contacts
                    && let Ok(rendered) = inbx_render::render_message_with_pgp(
                        &raw,
                        inbx_render::RemotePolicy::Block,
                        None,
                        None,
                    )
                    .await
                    && let Some(ac) = rendered.autocrypt
                    && let Err(e) = cs
                        .store_autocrypt(&ac.addr, &ac.keydata_armored, &ac.fingerprint)
                        .await
                {
                    tracing::debug!(addr = %ac.addr, %e, "autocrypt harvest: ignored");
                }
            }
            println!("{folder}: bodies downloaded");
        }
    }

    drop(provider);
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
        let date = m
            .date_unix
            .map(format_unix_relative)
            .unwrap_or_else(|| "—".into());
        let unread = !m.flags.to_ascii_lowercase().contains("seen");
        let starred = m.flags.contains("\\Flagged");
        let mark = format!(
            "{}{}",
            if unread { "●" } else { " " },
            if starred { "★" } else { " " }
        );
        let from = m.from_addr.unwrap_or_default();
        let subj = m.subject.unwrap_or_default();
        println!(
            "{:>4}  {mark}  {:>6}  {:<28}  {}",
            m.uid,
            date,
            truncate(&from, 28),
            subj
        );
    }
    Ok(())
}

async fn cmd_show(account: Option<String>, folder: String, uid: i64) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let raw = read_message_raw(&acct.name, &folder, uid).await?;
    let auth = inbx_render::auth::evaluate(&raw);
    let security = inbx_render::pgp::detect(&raw);
    let banner = format!(
        "[spf={:?} dkim={:?} dmarc={:?}]{}",
        auth.auth.spf,
        auth.auth.dkim,
        auth.auth.dmarc,
        security
            .label
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default(),
    );
    let parsed = mail_parser::MessageParser::default()
        .parse(&raw)
        .with_context(|| "parse message")?;
    println!("{banner}");
    println!("From:    {}", header_addr(&parsed, "From"));
    println!("To:      {}", header_addr(&parsed, "To"));
    if let Some(s) = parsed.subject() {
        println!("Subject: {s}");
    }
    if let Some(d) = parsed
        .header_values("Date")
        .next()
        .and_then(|v| v.as_text())
    {
        println!("Date:    {d}");
    }
    println!();
    let r = inbx_render::render_message(&raw, inbx_render::RemotePolicy::Block)?;
    if r.blocked_remote > 0 || !r.trackers.is_empty() {
        println!(
            "[remote blocked: {} url(s); trackers: {}]",
            r.blocked_remote,
            r.trackers.len()
        );
        println!();
    }
    println!("{}", r.plain);
    Ok(())
}

fn header_addr(parsed: &mail_parser::Message<'_>, name: &str) -> String {
    let group = match name {
        "From" => parsed.from(),
        "To" => parsed.to(),
        "Cc" => parsed.cc(),
        "Bcc" => parsed.bcc(),
        _ => None,
    };
    group
        .map(|g| {
            g.iter()
                .map(|a| match (a.name(), a.address()) {
                    (Some(n), Some(addr)) => format!("{n} <{addr}>"),
                    (_, Some(addr)) => addr.to_string(),
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

async fn cmd_headers(account: Option<String>, folder: String, uid: i64) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let raw = read_message_raw(&acct.name, &folder, uid).await?;
    // Headers end at the first blank line.
    let mut end = raw.len();
    let mut i = 0;
    while i + 3 < raw.len() {
        if &raw[i..i + 4] == b"\r\n\r\n" {
            end = i + 2;
            break;
        }
        if &raw[i..i + 2] == b"\n\n" {
            end = i + 1;
            break;
        }
        i += 1;
    }
    use std::io::Write as _;
    std::io::stdout().write_all(&raw[..end])?;
    Ok(())
}

async fn cmd_body(account: Option<String>, folder: String, uid: i64) -> Result<()> {
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, account.as_deref())?;
    let raw = read_message_raw(&acct.name, &folder, uid).await?;
    use std::io::Write as _;
    std::io::stdout().write_all(&raw)?;
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

    tracing::info!(account = %acct.name, transport = ?acct.transport, bytes = raw.len(), "sending");
    let send_result = match &acct.transport {
        inbx_config::Transport::Imap => inbx_net::send_message(&acct, &raw)
            .await
            .map_err(anyhow::Error::from),
        inbx_config::Transport::Graph => {
            let client = inbx_net::graph::GraphClient::connect(&acct).await?;
            client
                .send_mime(&raw, !no_save)
                .await
                .map_err(anyhow::Error::from)
        }
        inbx_config::Transport::Jmap { session_url } => {
            let client = inbx_net::jmap::JmapClient::connect(&acct, session_url).await?;
            client.send_mime(&raw).await.map_err(anyhow::Error::from)
        }
    };
    if let Err(e) = send_result {
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

    // Sent-folder append only applies to IMAP; Graph + JMAP both save to
    // Sent server-side via their send paths (saveToSentItems / send_mime).
    if !matches!(acct.transport, inbx_config::Transport::Imap) {
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

fn prompt_tls_with_default(
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    msg: &str,
    default: TlsMode,
) -> Result<TlsMode> {
    let raw = prompt(stdin, stdout, msg)?;
    match raw.to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "tls" => Ok(TlsMode::Tls),
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

/// Human-friendly timestamp for list views: "3m", "5h", "Mon", "12 Apr", "2024".
fn format_unix_relative(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = (now - ts).max(0);
    if delta < 60 {
        return "now".into();
    }
    if delta < 3600 {
        return format!("{}m", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h", delta / 3600);
    }
    if delta < 86_400 * 7 {
        let secs = ts.max(0) as u64;
        let days = (secs / 86400) as i64;
        let dow = ((days % 7) + 4).rem_euclid(7);
        return ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][dow as usize].into();
    }
    if delta < 86_400 * 365 {
        let s = format_unix(ts);
        if let (Some(m), Some(d)) = (
            s.get(5..7).and_then(|m| m.parse::<u32>().ok()),
            s.get(8..10),
        ) {
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            if let Some(name) = MONTHS.get(m.saturating_sub(1) as usize) {
                return format!("{d} {name}");
            }
        }
        return s;
    }
    format_unix(ts).get(0..4).unwrap_or("").to_string()
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_flag_returns_pkg_version() {
        let cmd = Cli::command();
        let version = cmd.render_version();
        assert!(
            version.contains(env!("CARGO_PKG_VERSION")),
            "render_version output {version:?} missing CARGO_PKG_VERSION"
        );
    }

    #[test]
    fn long_help_contains_ascii_art() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains(include_str!("art.txt")),
            "long_help missing embedded art.txt block; got:\n{help}"
        );
    }

    #[test]
    fn long_help_contains_pkg_version() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains(env!("CARGO_PKG_VERSION")),
            "long_help missing CARGO_PKG_VERSION; got:\n{help}"
        );
    }
}
