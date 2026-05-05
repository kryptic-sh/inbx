use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use hjkl_clipboard::{Clipboard, MimeType as ClipMime, Selection};
use hjkl_picker::Picker;
use inbx_composer::{Composer, FocusedEditor, Identity};
use inbx_config::Account;
use inbx_contacts::{Contact, ContactsStore, PubkeyLookup};
use inbx_render::RemotePolicy;
use inbx_store::{FolderRow, MessageRow, OutboxRow, Store};
use ratatui::widgets::ListState;

use super::ACTIVE_THEME;
use super::render::render_path;
use super::tasks::{TaskRx, TaskTx};

/// Tracks the leader-key (`<Space>`) prefix state. Reset to `None` after any
/// second key is consumed or on an unrecognised chord.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LeaderState {
    /// `<Space>` was pressed; waiting for the chord key.
    Pending,
}

/// Which hjkl-picker overlay is currently open.
pub(super) enum ActivePicker {
    Folder(Picker, Arc<Mutex<Option<String>>>),
    Account(Picker, Arc<Mutex<Option<String>>>),
    Message(Picker, Arc<Mutex<Option<i64>>>),
    Attachment(Picker, Arc<Mutex<Option<usize>>>, Vec<(String, Vec<u8>)>),
    /// Sieve script picker — payload is the script name.
    Sieve(Picker, Arc<Mutex<Option<String>>>),
    /// Template picker — payload is the template name.
    Template(Picker, Arc<Mutex<Option<String>>>),
}

/// Which sub-action the folder CRUD overlay is performing.
#[derive(Clone, Debug)]
pub(super) enum FolderCrudPrompt {
    Create(String),
    Rename(String, String), // (from, new_name)
    Delete(String, bool),   // (name, confirmed)
}

/// State for the folder CRUD action-choice overlay.
pub(super) struct FolderCrudState {
    /// 0 = Create, 1 = Rename, 2 = Delete.
    pub(super) state: ratatui::widgets::ListState,
}

impl FolderCrudState {
    pub(super) fn new() -> Self {
        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(0));
        Self { state }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Folders,
    Messages,
    Preview,
}

/// Modal indicator surfaced in the status line. Mirrors vim's idea of a
/// global mode but collapsed to the four states inbx actually has today:
/// `Normal` for list navigation, `Insert` while the composer is open,
/// `Search` while the `/` overlay accepts query input, and `Visual` —
/// reserved for a future per-message-range visual mode (no current
/// binding emits it; included so render code can match exhaustively
/// without churn when visual lands).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mode {
    Normal,
    Insert,
    #[allow(dead_code)]
    Visual,
    Search,
}

impl Mode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::Search => "SEARCH",
        }
    }
}

pub(super) struct App {
    pub(super) account: Account,
    pub(super) store: Store,
    pub(super) folders: Vec<FolderRow>,
    pub(super) folder_state: ListState,
    pub(super) messages: Vec<MessageRow>,
    pub(super) msg_state: ListState,
    pub(super) pane: Pane,
    pub(super) pending_g: bool,
    /// Leader-key (`<Space>`) prefix state. `Some(Pending)` after the first
    /// `<Space>` press; cleared after the chord is consumed or cancelled.
    pub(super) pending_leader: Option<LeaderState>,
    /// `Ctrl-G` chord state (composer-only). `true` after `Ctrl-G` is pressed;
    /// the next key (`s`/`e`) toggles `pgp.sign`/`pgp.encrypt`; any other key
    /// cancels.  Never set outside the composer pane.
    pub(super) pending_pgp_chord: bool,
    pub(super) body: String,
    pub(super) body_scroll: u16,
    /// When `true`, the preview pane shows raw RFC 5322 headers instead of
    /// the rendered body.  Toggled by `H` in the list pane.
    pub(super) preview_raw_headers: bool,
    pub(super) status: String,
    pub(super) composer: Option<Composer>,
    pub(super) show_help: bool,
    pub(super) move_picker: Option<MovePickerState>,
    pub(super) outbox: Option<OutboxState>,
    pub(super) search: Option<SearchState>,
    pub(super) thread: Option<ThreadState>,
    pub(super) account_picker: Option<AccountPickerState>,
    pub(super) contacts: Option<ContactsState>,
    pub(super) ical: Option<IcalState>,
    /// Last completed search. Persists across closes of the `/` overlay so
    /// that `n` / `N` from the main list jumps through results without
    /// retyping the query, and reopening `/` restores the prior session.
    pub(super) last_search: Option<LastSearch>,
    /// Unix timestamp of the most recent successful manual sync. `None`
    /// when no sync has run yet this session. Surfaced in the status line
    /// as a relative age (`12s`, `3m`, `1h`).
    pub(super) last_sync_unix: Option<i64>,
    /// Active hjkl-picker overlay. `None` when no picker is open.
    pub(super) active_picker: Option<ActivePicker>,
    /// Folder CRUD action-choice overlay (`<Space>F`). `None` when not open.
    pub(super) folder_crud: Option<FolderCrudState>,
    /// Active folder CRUD prompt (text input for name). `None` when not open.
    pub(super) folder_crud_prompt: Option<FolderCrudPrompt>,
    /// Active account-creation wizard. `None` when not open.
    pub(super) active_wizard: Option<super::wizard::AccountWizard>,
    /// Active Sieve-script edit wizard. `None` when not open.
    pub(super) active_sieve_wizard: Option<super::wizard::SieveEditWizard>,
    /// Cached ManageSieve session shared across the picker → edit → save flow.
    /// `None` means no live connection — the next sieve op will lazy-connect.
    /// Wrapped in `Arc<Mutex>` so the spawned task can hold it across `.await`
    /// without blocking the event loop and so the cache survives between tasks.
    pub(super) sieve_session:
        std::sync::Arc<tokio::sync::Mutex<Option<inbx_net::sieve::SieveClient>>>,
    /// True while an async operation (manual sync, fetch, outbox drain) is
    /// in flight. The event loop forces redraws at 120 ms while busy so the
    /// spinner in the status line animates.
    pub(super) busy: bool,
    /// Count of background tasks currently in flight.  `busy` mirrors
    /// `pending_op_count > 0`.  Increment on spawn; decrement on result.
    pub(super) pending_op_count: u32,
    /// Sender half of the background-task result channel.
    pub(super) task_tx: TaskTx,
    /// Receiver half of the background-task result channel.
    pub(super) task_rx: TaskRx,
    /// UIDs for which the user has already responded to a read-receipt prompt
    /// (either sent or declined).  Not persisted across sessions — best effort.
    pub(super) receipt_responded: HashSet<i64>,
    /// Read-receipt request for the currently previewed message, if any.
    /// Populated by `refresh_body`; cleared when the user responds or the
    /// message changes.
    pub(super) current_receipt: Option<inbx_render::ReadReceiptRequest>,
    /// Most recent `Rendered` for the previewed message. Used by the
    /// tree-sitter highlight path in render.rs.
    pub(super) current_rendered: Option<inbx_render::Rendered>,
    /// Cached `ContactsStore` opened lazily on first access (open is async +
    /// hits the disk; reused across reply / contacts-pane / pubkey-lookup).
    /// `Some(None)` after a failed open so we don't keep retrying.
    contacts_store: Option<Option<Arc<ContactsStore>>>,
    /// Handle for the currently running background watch task. Aborted and
    /// replaced whenever the active folder changes via `respawn_watch`.
    watch_handle: Option<tokio::task::JoinHandle<()>>,
    /// Name of the folder the current `watch_handle` is bound to.  `None`
    /// while no watch task is running.
    watched_folder: Option<String>,
    /// Live connection to the inbx-sync daemon IPC socket. `Some` when the
    /// daemon was detected at startup; `None` when using in-process watch.
    /// Present on unix only; always `None` on Windows.
    pub(super) sync_ipc: Option<()>,
    /// Unix timestamp of the last `Heartbeat` event from the IPC daemon.
    /// Used by a future status-line indicator (e.g. "synced 3m ago").
    pub(super) last_ipc_heartbeat_unix: Option<i64>,
    /// Async grammar loader for tree-sitter highlighting.
    #[cfg(feature = "tree-sitter")]
    pub(super) grammar_loader:
        std::sync::Arc<hjkl_bonsai::runtime::async_loader::AsyncGrammarLoader>,
    /// Registry for lang-name → LangSpec lookups.
    #[cfg(feature = "tree-sitter")]
    pub(super) grammar_registry: hjkl_bonsai::runtime::GrammarRegistry,
    /// Cache of loaded grammars keyed by language name.
    /// `Some(None)` means load failed — do not retry.
    #[cfg(feature = "tree-sitter")]
    pub(super) grammar_cache: GrammarCache,
}

#[cfg(feature = "tree-sitter")]
pub(super) type GrammarCache = std::sync::Arc<
    tokio::sync::Mutex<
        std::collections::HashMap<
            &'static str,
            Option<std::sync::Arc<hjkl_bonsai::runtime::Grammar>>,
        >,
    >,
>;

#[derive(Clone, Copy)]
pub(super) enum IcalResponse {
    Accept,
    Tentative,
    Decline,
}

pub(super) struct IcalState {
    pub(super) summary: String,
    pub(super) start: String,
    pub(super) end: String,
    pub(super) location: String,
    pub(super) organizer: String,
    pub(super) raw: Vec<u8>,
}

pub(super) struct ContactsState {
    pub(super) all: Vec<Contact>,
    pub(super) filter: String,
    pub(super) state: ListState,
}

impl ContactsState {
    pub(super) fn new(all: Vec<Contact>) -> Self {
        let mut state = ListState::default();
        if !all.is_empty() {
            state.select(Some(0));
        }
        Self {
            all,
            filter: String::new(),
            state,
        }
    }
}

pub(super) struct AccountPickerState {
    pub(super) accounts: Vec<Account>,
    pub(super) state: ListState,
}

impl AccountPickerState {
    pub(super) fn new(accounts: Vec<Account>) -> Self {
        let mut state = ListState::default();
        if !accounts.is_empty() {
            state.select(Some(0));
        }
        Self { accounts, state }
    }
}

/// Whether the move-picker is in move or copy mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MovePickerMode {
    Move,
    Copy,
}

pub(super) struct MovePickerState {
    pub(super) filter: String,
    pub(super) state: ListState,
    pub(super) mode: MovePickerMode,
}

pub(super) struct OutboxState {
    pub(super) entries: Vec<OutboxRow>,
    pub(super) state: ListState,
}

pub(super) struct SearchState {
    pub(super) query: String,
    pub(super) results: Vec<MessageRow>,
    pub(super) state: ListState,
}

/// Snapshot of the most recent `/` search retained after the overlay closes.
/// Used for `n` / `N` cursor-jump on the main list and for restoring the
/// overlay state when the user reopens `/`.
#[derive(Clone)]
pub(super) struct LastSearch {
    pub(super) query: String,
    pub(super) results: Vec<MessageRow>,
    /// Index into `results` of the most recently visited match.
    pub(super) cursor: usize,
}

/// Count unread messages in a row slice. Pure so the status-line tests
/// don't need a `Store`. "Unread" mirrors `draw_messages` — a row is
/// considered unread when its cached IMAP flags don't contain `\Seen`
/// (case-insensitive).
pub(super) fn unread_count(rows: &[MessageRow]) -> usize {
    rows.iter()
        .filter(|m| !m.flags.to_ascii_lowercase().contains("seen"))
        .count()
}

/// Pure cursor-step helper for n/N navigation. Returns the next index after
/// stepping `delta` (+1 for `n`, -1 for `N`) with wraparound. Returns `None`
/// when there are no results.
pub(super) fn search_step(len: usize, cursor: usize, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let len_i = len as i32;
    let next = (cursor as i32 + delta).rem_euclid(len_i);
    Some(next as usize)
}

/// Build a fresh `SearchState` for the `/` overlay, restoring from the
/// caller-supplied prior session when available. Pure so the open-search
/// behavior can be unit tested without a `Store`.
pub(super) fn build_search_state(prior: Option<&LastSearch>) -> SearchState {
    let Some(ls) = prior else {
        return SearchState::new();
    };
    let mut state = ListState::default();
    if !ls.results.is_empty() {
        state.select(Some(ls.cursor.min(ls.results.len() - 1)));
    }
    SearchState {
        query: ls.query.clone(),
        results: ls.results.clone(),
        state,
    }
}

pub(super) struct ThreadState {
    pub(super) messages: Vec<MessageRow>,
    pub(super) state: ListState,
}

impl SearchState {
    pub(super) fn new() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            state: ListState::default(),
        }
    }
}

impl MovePickerState {
    pub(super) fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            filter: String::new(),
            state,
            mode: MovePickerMode::Move,
        }
    }

    pub(super) fn new_copy() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            filter: String::new(),
            state,
            mode: MovePickerMode::Copy,
        }
    }
}

impl App {
    pub(super) async fn new(account: Account, store: Store) -> Result<Self> {
        let folders = store.list_folders().await?;
        let mut folder_state = ListState::default();
        if !folders.is_empty() {
            folder_state.select(Some(0));
        }
        let theme = inbx_config::theme::load_theme().unwrap_or_default();
        let _ = ACTIVE_THEME.set(theme);
        let (task_tx, task_rx) = super::tasks::channel();
        // Attempt to connect to a running inbx-sync daemon over IPC.
        // On unix: try with 500 ms timeout; suppress in-process watch if found.
        // On non-unix (Windows): always skip.
        #[cfg(unix)]
        let (sync_ipc, ipc_rx) = match inbx_ipc::Client::connect().await {
            Ok(mut client) => {
                tracing::debug!("ipc: connected to inbx-sync daemon");
                let rx = client.receiver();
                (Some(()), Some(rx))
            }
            Err(e) => {
                tracing::debug!(%e, "ipc: no sync daemon detected, using in-process watch");
                (None, None)
            }
        };
        #[cfg(not(unix))]
        let (sync_ipc, ipc_rx): (
            Option<()>,
            Option<tokio::sync::mpsc::Receiver<inbx_ipc::Event>>,
        ) = (None, None);

        #[cfg(feature = "tree-sitter")]
        let (grammar_loader, grammar_registry) = {
            let registry = hjkl_bonsai::runtime::GrammarRegistry::embedded()?;
            let loader = hjkl_bonsai::runtime::GrammarLoader::user_default(registry.meta())?;
            let async_loader = std::sync::Arc::new(
                hjkl_bonsai::runtime::async_loader::AsyncGrammarLoader::new(loader),
            );
            (async_loader, registry)
        };

        let mut app = Self {
            account,
            store,
            folders,
            folder_state,
            messages: Vec::new(),
            msg_state: ListState::default(),
            pane: Pane::Folders,
            pending_g: false,
            pending_leader: None,
            pending_pgp_chord: false,
            body: String::new(),
            body_scroll: 0,
            preview_raw_headers: false,
            status: String::new(),
            composer: None,
            show_help: false,
            move_picker: None,
            outbox: None,
            search: None,
            thread: None,
            account_picker: None,
            contacts: None,
            ical: None,
            last_search: None,
            last_sync_unix: None,
            active_picker: None,
            folder_crud: None,
            folder_crud_prompt: None,
            active_wizard: None,
            active_sieve_wizard: None,
            sieve_session: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            busy: false,
            pending_op_count: 0,
            task_tx,
            task_rx,
            contacts_store: None,
            receipt_responded: HashSet::new(),
            current_receipt: None,
            current_rendered: None,
            watch_handle: None,
            watched_folder: None,
            sync_ipc,
            last_ipc_heartbeat_unix: None,
            #[cfg(feature = "tree-sitter")]
            grammar_loader,
            #[cfg(feature = "tree-sitter")]
            grammar_registry,
            #[cfg(feature = "tree-sitter")]
            grammar_cache: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::<
                    &'static str,
                    Option<std::sync::Arc<hjkl_bonsai::runtime::Grammar>>,
                >::new(),
            )),
        };

        // If IPC connected, spawn a pump task that forwards Events into the
        // existing task channel as SyncIpcEvent variants.
        if let Some(mut rx) = ipc_rx {
            let tx = app.task_tx.0.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if tx
                        .send(super::tasks::TaskResult::SyncIpcEvent(event))
                        .is_err()
                    {
                        // TUI exited — stop pumping.
                        break;
                    }
                }
                tracing::warn!("ipc: daemon connection closed");
            });
        }

        app.reload_messages().await?;

        Ok(app)
    }

    /// Increment the pending-op counter and set busy. Call before spawning a
    /// background task.  Returns `false` (and sets a status) if an op is
    /// already in flight — callers should abort in that case.
    pub(super) fn spawn_pending(&mut self) -> bool {
        if self.pending_op_count > 0 {
            self.status = "busy — wait for current op".into();
            return false;
        }
        self.pending_op_count += 1;
        self.busy = true;
        true
    }

    /// Decrement the pending-op counter. When it reaches zero, clears busy.
    pub(super) fn complete_pending(&mut self) {
        self.pending_op_count = self.pending_op_count.saturating_sub(1);
        if self.pending_op_count == 0 {
            self.busy = false;
        }
    }

    pub(super) fn open_blank(&mut self) {
        self.composer = Some(Composer::new_blank(Identity::from_account(&self.account)));
        self.status = "compose: new draft".into();
    }

    /// Lazily open + cache the per-account `ContactsStore`. Returns `None`
    /// when the open fails (logged once).
    pub(super) async fn contacts_store(&mut self) -> Option<Arc<ContactsStore>> {
        if self.contacts_store.is_none() {
            self.contacts_store = Some(
                ContactsStore::open(&self.account.name)
                    .await
                    .map(Arc::new)
                    .map_err(|e| tracing::warn!("contacts store open failed: {e}"))
                    .ok(),
            );
        }
        self.contacts_store.as_ref().and_then(|o| o.clone())
    }

    pub(super) async fn open_contacts(&mut self) -> Result<()> {
        let store = self
            .contacts_store()
            .await
            .ok_or_else(|| anyhow::anyhow!("contacts store unavailable"))?;
        let all = store.list(u32::MAX).await?;
        let n = all.len();
        self.contacts = Some(ContactsState::new(all));
        self.status = format!("contacts: {n}");
        Ok(())
    }

    pub(super) fn contacts_filtered(&self) -> Vec<Contact> {
        let Some(c) = self.contacts.as_ref() else {
            return Vec::new();
        };
        let needle = c.filter.to_ascii_lowercase();
        if needle.is_empty() {
            return c.all.clone();
        }
        c.all
            .iter()
            .filter(|x| {
                x.email.to_ascii_lowercase().contains(&needle)
                    || x.name
                        .as_deref()
                        .map(|n| n.to_ascii_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    pub(super) fn compose_to_contact(&mut self, email: &str) {
        let mut composer = Composer::new_blank(Identity::from_account(&self.account));
        composer.set_to(email);
        self.composer = Some(composer);
        self.status = format!("compose: to {email}");
    }

    pub(super) async fn open_reply(&mut self, all: bool) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let Some(path) = msg.maildir_path else {
            self.status = "no body fetched — `inbx fetch --bodies` first".into();
            return Ok(());
        };
        let raw = std::fs::read(&path)?;
        // Use the async variant with PGP lookup when a contacts store is
        // available — enables Autocrypt 1.1 §4 mutual-mode auto-encrypt.
        let store = self.contacts_store().await;
        let result = if let Some(store) = store.as_deref() {
            Composer::new_reply_with_pgp_lookup(
                Identity::from_account(&self.account),
                &raw,
                all,
                Some(store as &dyn PubkeyLookup),
            )
            .await
            .map_err(anyhow::Error::from)
        } else {
            Composer::new_reply(Identity::from_account(&self.account), &raw, all)
                .map_err(anyhow::Error::from)
        };
        match result {
            Ok(c) => {
                self.composer = Some(c);
                self.status = if all {
                    "compose: reply-all".into()
                } else {
                    "compose: reply".into()
                };
            }
            Err(e) => {
                self.status = format!("reply failed: {e}");
            }
        }
        Ok(())
    }

    pub(super) async fn open_forward(&mut self) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let Some(path) = msg.maildir_path else {
            self.status = "no body fetched — `inbx fetch --bodies` first".into();
            return Ok(());
        };
        let raw = std::fs::read(&path)?;
        match Composer::new_forward(Identity::from_account(&self.account), &raw) {
            Ok(c) => {
                self.composer = Some(c);
                self.status = "compose: forward".into();
            }
            Err(e) => {
                self.status = format!("forward failed: {e}");
            }
        }
        Ok(())
    }

    pub(super) async fn unsubscribe_current(&mut self) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let Some(path) = msg.maildir_path else {
            self.status = "no body fetched — press Enter or F first".into();
            return Ok(());
        };
        let raw = std::fs::read(&path)?;
        let header = match extract_list_unsubscribe(&raw) {
            Some(h) => h,
            None => {
                self.status = "no List-Unsubscribe header".into();
                return Ok(());
            }
        };
        let uris = parse_unsubscribe_uris(&header);
        if let Some(mailto) = uris.iter().find(|u| u.starts_with("mailto:")) {
            let (to, subject, body) = parse_mailto(mailto);
            let raw = build_unsubscribe_mime(&self.account.email, &to, &subject, &body);
            match inbx_net::send_message(&self.account, &raw).await {
                Ok(()) => self.status = format!("unsubscribe sent to {to}"),
                Err(e) => self.status = format!("unsubscribe failed: {e}"),
            }
            return Ok(());
        }
        if let Some(https) = uris.iter().find(|u| u.starts_with("https:")) {
            match open_url(https) {
                Ok(()) => self.status = format!("opened {https}"),
                Err(e) => self.status = format!("open failed: {e}"),
            }
            return Ok(());
        }
        self.status = "no List-Unsubscribe header".into();
        Ok(())
    }

    pub(super) async fn open_ical(&mut self) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let Some(path) = msg.maildir_path else {
            self.status = "no body fetched — press Enter or F first".into();
            return Ok(());
        };
        let raw = std::fs::read(&path)?;
        let invite = match inbx_ical::parse_message(&raw) {
            Ok(i) => i,
            Err(_) => {
                self.status = "no calendar invite in this message".into();
                return Ok(());
            }
        };
        let method = invite.method.as_deref().unwrap_or("").to_ascii_uppercase();
        if method != "REQUEST" {
            self.status = "no calendar invite in this message".into();
            return Ok(());
        }
        self.ical = Some(IcalState {
            summary: invite.summary.unwrap_or_default(),
            start: invite.start.unwrap_or_default(),
            end: invite.end.unwrap_or_default(),
            location: invite.location.unwrap_or_default(),
            organizer: invite.organizer.unwrap_or_default(),
            raw: invite.raw.into_bytes(),
        });
        self.status = "ical: a accept · t tentative · d decline · Esc cancel".into();
        Ok(())
    }

    pub(super) async fn respond_ical(&mut self, response: IcalResponse) -> Result<()> {
        let Some(state) = self.ical.as_ref() else {
            return Ok(());
        };
        let invite = match inbx_ical::parse_ics(&String::from_utf8_lossy(&state.raw)) {
            Ok(i) => i,
            Err(e) => {
                self.status = format!("ical parse failed: {e}");
                self.ical = None;
                return Ok(());
            }
        };
        let attendee = format!("mailto:{}", self.account.email);
        let rsvp = match response {
            IcalResponse::Accept => inbx_ical::RsvpResponse::Accept,
            IcalResponse::Tentative => inbx_ical::RsvpResponse::Tentative,
            IcalResponse::Decline => inbx_ical::RsvpResponse::Decline,
        };
        let reply_ics = match inbx_ical::build_reply(&invite, rsvp, &attendee) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("ical reply failed: {e}");
                self.ical = None;
                return Ok(());
            }
        };
        let organizer_email = invite
            .organizer
            .as_deref()
            .map(strip_mailto)
            .unwrap_or_default()
            .to_string();
        if organizer_email.is_empty() {
            self.status = "ical reply: no organizer to send to".into();
            self.ical = None;
            return Ok(());
        }
        let subject = format!(
            "{}: {}",
            match response {
                IcalResponse::Accept => "Accepted",
                IcalResponse::Tentative => "Tentative",
                IcalResponse::Decline => "Declined",
            },
            invite.summary.as_deref().unwrap_or(""),
        );
        let raw = build_ical_reply_mime(
            &self.account.email,
            &organizer_email,
            &subject,
            reply_ics.as_bytes(),
        );
        match inbx_net::send_message(&self.account, &raw).await {
            Ok(()) => {
                self.status = format!(
                    "ical reply sent to {organizer_email} ({})",
                    match response {
                        IcalResponse::Accept => "accepted",
                        IcalResponse::Tentative => "tentative",
                        IcalResponse::Decline => "declined",
                    }
                );
            }
            Err(e) => {
                self.status = format!("ical reply failed: {e}");
            }
        }
        self.ical = None;
        Ok(())
    }

    pub(super) fn close_ical(&mut self) {
        self.ical = None;
        self.status = "ical: cancelled".into();
    }

    /// Send a read receipt (MDN) for the current message.  Called only when
    /// the user explicitly presses `Y` in the Preview pane.
    pub(super) async fn send_read_receipt(&mut self) -> Result<()> {
        let req = match self.current_receipt.clone() {
            Some(r) => r,
            None => return Ok(()),
        };
        let uid = match self.current_message().map(|m| m.uid) {
            Some(u) => u,
            None => return Ok(()),
        };
        let subject = self
            .current_message()
            .and_then(|m| m.subject.clone())
            .unwrap_or_default();

        let ctx = inbx_net::MdnContext {
            from: self.account.email.clone(),
            to: req.notify_to.clone(),
            original_message_id: req.original_message_id.unwrap_or_default(),
            original_recipient: req.original_from,
            original_subject: subject,
            disposition: inbx_net::MdnDisposition::DisplayedManualAction,
            reporting_ua: gethostname::gethostname().to_string_lossy().into_owned(),
        };

        let addrs = req.notify_to.join(", ");
        match inbx_net::build_mdn(&ctx) {
            Ok(raw) => match inbx_net::send_message(&self.account, &raw).await {
                Ok(()) => {
                    self.status = format!("sent read receipt to {addrs}");
                }
                Err(e) => {
                    self.status = format!("read receipt send failed: {e}");
                }
            },
            Err(e) => {
                self.status = format!("read receipt build failed: {e}");
            }
        }
        self.receipt_responded.insert(uid);
        self.current_receipt = None;
        Ok(())
    }

    /// Decline a read receipt for the current message.  Called when the user
    /// explicitly presses `N` in the Preview pane.
    pub(super) fn decline_read_receipt(&mut self) {
        let uid = match self.current_message().map(|m| m.uid) {
            Some(u) => u,
            None => return,
        };
        self.receipt_responded.insert(uid);
        self.current_receipt = None;
        self.status = "declined read receipt".into();
    }

    pub(super) async fn save_draft(&mut self) -> Result<()> {
        let Some(composer) = self.composer.as_ref() else {
            return Ok(());
        };
        let raw = composer.to_mime()?;
        // Hot-path: use MailProvider so JMAP accounts use Email/import.
        let mut provider = inbx_net::connect_provider(&self.account, Some(&self.store)).await?;
        let folders = provider.list_folders().await?;
        let drafts = inbx_net::find_drafts_folder(&folders);
        match drafts {
            Some(drafts_name) => {
                provider.append_draft(&drafts_name, &raw).await?;
                drop(provider);
                self.composer = None;
                self.status = format!("draft saved to {drafts_name}");
            }
            None => {
                drop(provider);
                self.status = "no Drafts folder discovered".into();
            }
        }
        Ok(())
    }

    pub(super) async fn send_composer(&mut self) -> Result<()> {
        let Some(composer) = self.composer.as_ref() else {
            return Ok(());
        };

        let raw = if composer.pgp.sign || composer.pgp.encrypt {
            // PGP path.
            let pgp_cfg = match &self.account.pgp {
                Some(cfg) => cfg.clone(),
                None => {
                    self.status =
                        "pgp: account has no pgp config; run `inbx pgp keygen` first".into();
                    return Ok(());
                }
            };
            // Only the inbx-managed source is supported for the TUI PGP send path.
            let source = match inbx_pgp::key_source_for(&pgp_cfg) {
                Ok(s) => s,
                Err(e) => {
                    self.status = format!("pgp: key source error: {e}");
                    return Ok(());
                }
            };

            // Resolve signer key: prefer configured fingerprint, else first key.
            let signer_key = if let Some(fpr) = &pgp_cfg.key_fingerprint {
                inbx_pgp::KeyId(fpr.clone())
            } else {
                match source.list_keys().await {
                    Ok(keys) if !keys.is_empty() => {
                        let k = keys.into_iter().next().unwrap().0;
                        tracing::info!(
                            "pgp: no key_fingerprint configured; using first key {}",
                            k.0
                        );
                        k
                    }
                    Ok(_) => {
                        self.status = "pgp: no keys found in managed dir".into();
                        return Ok(());
                    }
                    Err(e) => {
                        self.status = format!("pgp: list keys failed: {e}");
                        return Ok(());
                    }
                }
            };

            // For encrypt: load all *.pub.asc files from the managed_dir.
            // Recipients not in the dir are silently missing — the user must
            // manually drop their pubkeys into managed_dir.
            let recipient_pubkeys: Vec<inbx_pgp::ArmoredKey> = if composer.pgp.encrypt {
                let managed_dir = pgp_cfg.managed_dir.clone().unwrap_or_else(|| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    std::path::PathBuf::from(home)
                        .join(".local")
                        .join("share")
                        .join("inbx")
                        .join("pgp")
                });
                tracing::warn!(
                    "pgp encrypt: loading all pubkeys from {}; \
                     drop recipient pubkeys into that dir manually for now",
                    managed_dir.display()
                );
                let mut keys = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&managed_dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        let name = p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        if name.ends_with(".pub.asc")
                            && let Ok(raw) = std::fs::read_to_string(&p)
                        {
                            keys.push(inbx_pgp::ArmoredKey(raw));
                        }
                    }
                }
                if keys.is_empty() {
                    self.status = "pgp encrypt: no recipient pubkeys found in managed_dir; \
                                   drop *.pub.asc files there"
                        .into();
                    return Ok(());
                }
                keys
            } else {
                Vec::new()
            };

            let c = composer;
            match c
                .to_mime_with_pgp(Some(&*source), Some(&signer_key), &recipient_pubkeys)
                .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.status = format!("pgp send failed: {e}");
                    return Ok(());
                }
            }
        } else {
            // Plain (non-PGP) path.
            composer.to_mime()?
        };

        match inbx_net::send_message(&self.account, &raw).await {
            Ok(()) => {
                self.composer = None;
                self.status = format!("sent ({} bytes)", raw.len());
            }
            Err(e) => {
                let id = self.store.outbox_enqueue(&raw).await?;
                self.store.outbox_record_failure(id, &e.to_string()).await?;
                self.composer = None;
                self.status = format!("queued in outbox (id={id}): {e}");
            }
        }
        Ok(())
    }

    pub(super) fn close_composer(&mut self) {
        self.composer = None;
        self.status = "draft discarded".into();
    }

    /// Copy the focused editor's full text to the system clipboard (`<leader>y`).
    pub(super) fn yank_to_clipboard(&mut self) {
        let Some(composer) = self.composer.as_mut() else {
            self.status = "yank: no composer open".into();
            return;
        };
        let text = match composer.focused_editor() {
            FocusedEditor::Body(ed) => ed.content(),
            FocusedEditor::Header(f) => f.text(),
        };
        match Clipboard::new() {
            Ok(cb) => match cb.set(Selection::Clipboard, ClipMime::Text, text.as_bytes()) {
                Ok(()) => self.status = format!("yanked {} bytes to clipboard", text.len()),
                Err(e) => self.status = format!("yank failed: {e}"),
            },
            Err(e) => self.status = format!("clipboard unavailable: {e}"),
        }
    }

    /// Replace the focused editor's text with the system clipboard contents (`<leader>p`).
    pub(super) fn put_from_clipboard(&mut self) {
        let Some(composer) = self.composer.as_mut() else {
            self.status = "put: no composer open".into();
            return;
        };
        match Clipboard::new() {
            Ok(cb) => match cb.get(Selection::Clipboard, ClipMime::Text) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => {
                        let len = text.len();
                        match composer.focused_editor() {
                            FocusedEditor::Body(ed) => ed.set_content(&text),
                            FocusedEditor::Header(f) => f.set_text(&text),
                        }
                        self.status = format!("put {len} bytes from clipboard");
                    }
                    Err(_) => self.status = "put: clipboard data is not valid UTF-8".into(),
                },
                Err(e) => self.status = format!("put failed: {e}"),
            },
            Err(e) => self.status = format!("clipboard unavailable: {e}"),
        }
    }

    pub(super) async fn toggle_seen(&mut self) -> Result<()> {
        self.toggle_flag("\\Seen").await
    }

    pub(super) async fn toggle_starred(&mut self) -> Result<()> {
        self.toggle_flag("\\Flagged").await
    }

    pub(super) async fn toggle_deleted(&mut self) -> Result<()> {
        self.toggle_flag("\\Deleted").await
    }

    async fn toggle_flag(&mut self, flag: &str) -> Result<()> {
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let has = msg.flags.contains(flag);
        let (add, remove): (Vec<&str>, Vec<&str>) = if has {
            (vec![], vec![flag])
        } else {
            (vec![flag], vec![])
        };
        // Hot-path: use MailProvider so JMAP accounts get Email/set.
        let mut provider = inbx_net::connect_provider(&self.account, Some(&self.store)).await?;
        provider
            .set_flags(&folder_name, msg.uid, &add, &remove)
            .await?;
        drop(provider);
        self.store
            .mutate_flags(&folder_name, &[msg.uid], &add, &remove)
            .await?;
        self.reload_messages().await?;
        self.status = format!("{}{flag}", if has { "removed " } else { "added " });
        Ok(())
    }

    /// Manual sync trigger (`F`). Spawns a background task that connects,
    /// refreshes the current folder's headers, and posts `TaskResult::SyncDone`.
    pub(super) fn manual_sync(&mut self) {
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return;
        };
        if !self.spawn_pending() {
            return;
        }
        self.status = format!("syncing {folder_name}…");
        let account = self.account.clone();
        let store = self.store.clone();
        let tx = self.task_tx.0.clone();
        tokio::spawn(async move {
            let result = do_manual_sync(account, store, folder_name).await;
            let _ = tx.send(result);
        });
    }

    /// Current modal state for the status-line indicator. The composer
    /// implies `Insert`, the `/` overlay implies `Search`, and otherwise
    /// the app is `Normal`. (`Visual` is reserved — see `Mode`.)
    pub(super) fn mode(&self) -> Mode {
        if self.composer.is_some() {
            Mode::Insert
        } else if self.search.is_some() {
            Mode::Search
        } else {
            Mode::Normal
        }
    }

    /// Count unread messages in the currently-loaded message list. The
    /// status line surfaces this for the focused folder; "unread" means
    /// the cached IMAP flags don't include `\Seen`.
    pub(super) fn unread_in_current_folder(&self) -> usize {
        unread_count(&self.messages)
    }

    /// Lazy body fetch. If the selected message is header-only, spawns a
    /// background task to pull from IMAP. Result comes back as
    /// `TaskResult::BodyFetched`.
    pub(super) fn fetch_current_body(&mut self) {
        let Some(msg) = self.current_message().cloned() else {
            return;
        };
        if msg.maildir_path.is_some() {
            return;
        }
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return;
        };
        if !self.spawn_pending() {
            return;
        }
        self.status = format!("fetching body for uid {}…", msg.uid);
        let account = self.account.clone();
        let store = self.store.clone();
        let tx = self.task_tx.0.clone();
        tokio::spawn(async move {
            let result = do_fetch_body(
                account,
                store,
                folder_name,
                msg.uid,
                msg.uidvalidity,
                msg.flags,
            )
            .await;
            let _ = tx.send(result);
        });
    }

    pub(super) async fn oauth_login(&mut self) -> Result<()> {
        let provider = match &self.account.auth {
            inbx_config::AuthMethod::OAuth2 { provider, .. } => provider.clone(),
            _ => {
                self.status = "not an oauth2 account".into();
                return Ok(());
            }
        };
        self.status = "oauth login: opening browser…".into();
        match inbx_net::oauth_login(&self.account.auth, &provider, self.account.proxy.as_ref())
            .await
        {
            Ok(token) => {
                inbx_config::store_refresh_token(&self.account.name, &token.refresh)?;
                self.status = "oauth login complete".into();
            }
            Err(e) => {
                self.status = format!("oauth login failed: {e}");
            }
        }
        Ok(())
    }

    pub(super) async fn expunge(&mut self) -> Result<()> {
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        let mut provider = inbx_net::connect_provider(&self.account, Some(&self.store)).await?;
        let n = provider.expunge_folder(&folder_name).await?;
        drop(provider);
        let purged = self.store.purge_deleted(&folder_name).await?;
        self.reload_messages().await?;
        self.status = format!("expunged {n} (server) / {purged} (local) in {folder_name}");
        Ok(())
    }

    pub(super) async fn move_current_to(&mut self, target: &str) -> Result<()> {
        let Some(source) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        if source == target {
            self.status = format!("already in {target}");
            return Ok(());
        }
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        // Hot-path: use MailProvider so JMAP accounts get Email/set move.
        let mut provider = inbx_net::connect_provider(&self.account, Some(&self.store)).await?;
        provider.move_message(&source, msg.uid, target).await?;
        drop(provider);
        self.store.delete_messages(&source, &[msg.uid]).await?;
        self.reload_messages().await?;
        self.status = format!("moved uid {} → {target}", msg.uid);
        Ok(())
    }

    /// UID COPY the current message to `target` folder (IMAP-only).
    pub(super) async fn copy_current_to(&mut self, target: &str) -> Result<()> {
        let Some(source) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        // UID COPY is IMAP-only — connect_imap directly.
        let mut session = inbx_net::connect_imap(&self.account).await?;
        inbx_net::uid_copy(&mut session, &source, &[msg.uid as u32], target).await?;
        let _ = session.logout().await;
        self.status = format!("copied uid {} → {target}", msg.uid);
        Ok(())
    }

    /// Open a template picker (`<Space>t`).
    pub(super) fn open_template_picker(&mut self) {
        let names = match inbx_composer::templates::list(&self.account.name) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("templates: {e}");
                return;
            }
        };
        if names.is_empty() {
            self.status = "templates: none saved (use `inbx template save`)".into();
            return;
        }
        let (p, slot) = super::picker::template_picker(names);
        self.active_picker = Some(ActivePicker::Template(p, slot));
        self.status = "<Space>t templates: Enter open · Esc cancel".into();
    }

    /// Open a composer pre-filled from the named template (`<Space>t`).
    pub(super) fn open_template(&mut self, name: String) {
        let identity = inbx_composer::Identity::from_account(&self.account);
        match inbx_composer::templates::from_template(identity, &self.account.name, &name) {
            Ok(composer) => {
                self.composer = Some(composer);
                self.status = format!("compose: template '{name}'");
            }
            Err(e) => {
                self.status = format!("template '{name}': {e}");
            }
        }
    }

    /// Open the folder CRUD overlay (`<Space>F`).
    pub(super) fn open_folder_crud(&mut self) {
        self.folder_crud = Some(FolderCrudState::new());
        self.status = "folder: c create · r rename · d delete · Esc cancel".into();
    }

    /// Execute the selected folder CRUD action.
    pub(super) fn confirm_folder_crud(&mut self) {
        let Some(crud) = self.folder_crud.as_ref() else {
            return;
        };
        let op = crud.state.selected().unwrap_or(0);
        let current_folder = self.current_folder().map(|f| f.name.clone());
        match op {
            // Create
            0 => {
                self.folder_crud = None;
                self.status = "folder create: type new name (Esc cancel)".into();
                self.folder_crud_prompt = Some(FolderCrudPrompt::Create(String::new()));
            }
            // Rename
            1 => {
                let Some(name) = current_folder else {
                    self.status = "no folder selected".into();
                    self.folder_crud = None;
                    return;
                };
                self.folder_crud = None;
                self.status = format!("folder rename '{name}': type new name (Esc cancel)");
                self.folder_crud_prompt = Some(FolderCrudPrompt::Rename(name, String::new()));
            }
            // Delete
            2 => {
                let Some(name) = current_folder else {
                    self.status = "no folder selected".into();
                    self.folder_crud = None;
                    return;
                };
                self.folder_crud = None;
                self.status = format!("folder delete '{name}': y to confirm · Esc cancel");
                self.folder_crud_prompt = Some(FolderCrudPrompt::Delete(name, false));
            }
            _ => {
                self.folder_crud = None;
            }
        }
    }

    /// Apply a folder CRUD prompt action (spawns background task).
    pub(super) fn apply_folder_crud_prompt(&mut self) {
        let Some(prompt) = self.folder_crud_prompt.take() else {
            return;
        };
        let account = self.account.clone();
        let store = self.store.clone();
        let tx = self.task_tx.0.clone();
        match prompt {
            FolderCrudPrompt::Create(name) => {
                if name.is_empty() {
                    self.status = "folder create: name cannot be empty".into();
                    return;
                }
                if !self.spawn_pending() {
                    return;
                }
                self.status = format!("creating folder '{name}'…");
                tokio::spawn(async move {
                    let result = async {
                        let mut provider =
                            inbx_net::connect_provider(&account, Some(&store)).await?;
                        provider.create_folder(&name).await?;
                        drop(provider);
                        Ok::<_, anyhow::Error>(name)
                    }
                    .await;
                    let _ = tx.send(super::tasks::TaskResult::FolderOp(
                        result.map_err(|e| e.to_string()),
                    ));
                });
            }
            FolderCrudPrompt::Rename(from, to) => {
                if to.is_empty() {
                    self.status = "folder rename: new name cannot be empty".into();
                    return;
                }
                if !self.spawn_pending() {
                    return;
                }
                let msg = format!("renamed '{from}' → '{to}'");
                self.status = format!("renaming '{from}' → '{to}'…");
                tokio::spawn(async move {
                    let result = async {
                        let mut provider =
                            inbx_net::connect_provider(&account, Some(&store)).await?;
                        provider.rename_folder(&from, &to).await?;
                        drop(provider);
                        Ok::<_, anyhow::Error>(msg)
                    }
                    .await;
                    let _ = tx.send(super::tasks::TaskResult::FolderOp(
                        result.map_err(|e| e.to_string()),
                    ));
                });
            }
            FolderCrudPrompt::Delete(name, _confirmed) => {
                if !self.spawn_pending() {
                    return;
                }
                let msg = format!("deleted '{name}'");
                self.status = format!("deleting '{name}'…");
                tokio::spawn(async move {
                    let result = async {
                        let mut provider =
                            inbx_net::connect_provider(&account, Some(&store)).await?;
                        provider.delete_folder(&name).await?;
                        drop(provider);
                        Ok::<_, anyhow::Error>(msg)
                    }
                    .await;
                    let _ = tx.send(super::tasks::TaskResult::FolderOp(
                        result.map_err(|e| e.to_string()),
                    ));
                });
            }
        }
    }

    pub(super) async fn run_search(&mut self) -> Result<()> {
        let Some(s) = self.search.as_mut() else {
            return Ok(());
        };
        let q = s.query.trim().to_string();
        if q.is_empty() {
            s.results.clear();
            s.state.select(None);
            self.status = "search: empty query".into();
            return Ok(());
        }
        match self.store.search(&q, 200).await {
            Ok(rows) => {
                let n = rows.len();
                if let Some(s) = self.search.as_mut() {
                    s.results = rows.clone();
                    if s.results.is_empty() {
                        s.state.select(None);
                    } else {
                        s.state.select(Some(0));
                    }
                }
                if rows.is_empty() {
                    self.last_search = None;
                } else {
                    self.last_search = Some(LastSearch {
                        query: q,
                        results: rows,
                        cursor: 0,
                    });
                }
                self.status = format!("search: {n} results");
            }
            Err(e) => {
                if let Some(s) = self.search.as_mut() {
                    s.results.clear();
                    s.state.select(None);
                }
                self.status = format!("search failed: {e}");
            }
        }
        Ok(())
    }

    /// Open the `/` overlay, restoring state from `last_search` when present
    /// so the user can refine an existing query without retyping.
    pub(super) fn open_search(&mut self) {
        self.search = Some(build_search_state(self.last_search.as_ref()));
    }

    /// Step through `last_search` results. `delta = 1` advances (`n`), `-1`
    /// retreats (`N`). Wraps at boundaries. No-op when there are no results.
    pub(super) async fn step_last_search(&mut self, delta: i32) -> Result<()> {
        let Some(ls) = self.last_search.as_ref() else {
            self.status = "no prior search — press / to search".into();
            return Ok(());
        };
        let len = ls.results.len();
        let Some(next) = search_step(len, ls.cursor, delta) else {
            self.status = "no search results".into();
            return Ok(());
        };
        let target = ls.results[next].clone();
        if let Some(ls) = self.last_search.as_mut() {
            ls.cursor = next;
        }
        self.jump_to_message(&target.folder, target.uid).await?;
        let q = self
            .last_search
            .as_ref()
            .map(|s| s.query.clone())
            .unwrap_or_default();
        self.status = format!("/{q}  match {}/{}", next + 1, len);
        Ok(())
    }

    pub(super) async fn jump_to_message(&mut self, folder: &str, uid: i64) -> Result<()> {
        let Some(idx) = self.folders.iter().position(|f| f.name == folder) else {
            self.status = format!("folder {folder} not found");
            return Ok(());
        };
        self.folder_state.select(Some(idx));
        self.reload_messages().await?;
        if let Some(mi) = self.messages.iter().position(|m| m.uid == uid) {
            self.msg_state.select(Some(mi));
        }
        self.refresh_body();
        self.pane = Pane::Messages;
        self.status = format!("jumped to {folder}/uid {uid}");
        Ok(())
    }

    pub(super) async fn open_thread(&mut self) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        let messages = match msg.thread_id.as_deref() {
            Some(tid) => self.store.list_thread(tid).await?,
            None => vec![msg.clone()],
        };
        let mut state = ListState::default();
        if !messages.is_empty() {
            // Select the row matching the current message when present.
            let pick = messages
                .iter()
                .position(|m| m.folder == msg.folder && m.uid == msg.uid)
                .unwrap_or(0);
            state.select(Some(pick));
        }
        let n = messages.len();
        self.thread = Some(ThreadState { messages, state });
        self.status = format!("thread: {n} message(s)");
        Ok(())
    }

    pub(super) fn open_account_picker(&mut self) -> Result<()> {
        let cfg = inbx_config::load()?;
        let accounts = cfg.accounts;
        if accounts.is_empty() {
            self.status = "no accounts configured".into();
            return Ok(());
        }
        let n = accounts.len();
        self.account_picker = Some(AccountPickerState::new(accounts));
        self.status = format!("accounts: {n}");
        Ok(())
    }

    /// Open hjkl-picker folder overlay (`<Space>f`).
    pub(super) fn open_folder_picker(&mut self) {
        let (p, slot) = super::picker::folder_picker(self.folders.clone());
        self.active_picker = Some(ActivePicker::Folder(p, slot));
        self.status = "<Space>f folders: Enter pick · Esc cancel".into();
    }

    /// Open hjkl-picker account overlay (`<Space>b`).
    pub(super) fn open_hjkl_account_picker(&mut self) -> Result<()> {
        let cfg = inbx_config::load()?;
        if cfg.accounts.is_empty() {
            self.status = "no accounts configured".into();
            return Ok(());
        }
        let (p, slot) = super::picker::account_picker(&cfg.accounts);
        self.active_picker = Some(ActivePicker::Account(p, slot));
        self.status = "<Space>b accounts: Enter pick · Esc cancel".into();
        Ok(())
    }

    /// Open hjkl-picker message-jump overlay (`<Space>m`).
    pub(super) fn open_message_picker(&mut self) {
        let (p, slot) = super::picker::message_picker(self.messages.clone());
        self.active_picker = Some(ActivePicker::Message(p, slot));
        self.status = "<Space>m messages: Enter jump · Esc cancel".into();
    }

    /// Open hjkl-picker attachment overlay (`<Space>a`).
    pub(super) fn open_attachment_picker(&mut self) {
        let Some(msg) = self.current_message().cloned() else {
            self.status = "no message selected".into();
            return;
        };
        let Some(path) = msg.maildir_path else {
            self.status = "no body fetched — press Enter or F first".into();
            return;
        };
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.status = format!("read error: {e}");
                return;
            }
        };
        let parts = super::picker::extract_attachments(&raw);
        if parts.is_empty() {
            self.status = "no attachments in this message".into();
            return;
        }
        let (p, slot) = super::picker::attachment_picker(&parts);
        self.active_picker = Some(ActivePicker::Attachment(p, slot, parts));
        self.status = "<Space>a attachments: Enter save · Esc cancel".into();
    }

    /// Drop the cached sieve session, sending LOGOUT if the connection is live.
    /// Spawned onto a task so the caller isn't blocked on the network round-trip.
    pub(super) fn drop_sieve_session(&mut self) {
        let cache = self.sieve_session.clone();
        tokio::spawn(async move {
            let mut guard = cache.lock().await;
            if let Some(client) = guard.take() {
                let _ = client.logout().await;
            }
        });
    }

    /// Connect to ManageSieve, list scripts, open a picker (`<Space>S`).
    /// Spawns a background task; result comes back as `TaskResult::SieveScripts`.
    pub(super) fn open_sieve_picker(&mut self) {
        if !self.spawn_pending() {
            return;
        }
        self.status = "sieve: connecting…".into();
        let account = self.account.clone();
        let cache = self.sieve_session.clone();
        let tx = self.task_tx.0.clone();
        tokio::spawn(async move {
            let result = do_sieve_list(account, cache).await;
            let _ = tx.send(result);
        });
    }

    /// Fetch a sieve script body and open the edit wizard.
    /// Spawns a background task; result comes back as `TaskResult::SieveBody`.
    pub(super) fn open_sieve_edit(&mut self, name: String) {
        if !self.spawn_pending() {
            return;
        }
        self.status = format!("sieve: fetching '{name}'…");
        let account = self.account.clone();
        let cache = self.sieve_session.clone();
        let tx = self.task_tx.0.clone();
        tokio::spawn(async move {
            let result = do_sieve_get(account, cache, name).await;
            let _ = tx.send(result);
        });
    }

    /// Save the current sieve wizard to the server.
    /// Spawns a background task; result comes back as `TaskResult::SieveSaved`.
    pub(super) fn save_sieve_wizard(&mut self) {
        let Some(wiz) = self.active_sieve_wizard.take() else {
            return;
        };
        match wiz.build() {
            Ok((name, body)) => {
                if !self.spawn_pending() {
                    // Restore wizard so user isn't left hanging.
                    self.active_sieve_wizard =
                        Some(super::wizard::SieveEditWizard::new(name, body));
                    return;
                }
                self.status = format!("sieve: saving '{name}'…");
                let account = self.account.clone();
                let cache = self.sieve_session.clone();
                let tx = self.task_tx.0.clone();
                tokio::spawn(async move {
                    let result = do_sieve_put(account, cache, name, body).await;
                    let _ = tx.send(result);
                });
            }
            Err(e) => {
                self.status = format!("sieve: {e}");
                self.active_sieve_wizard = Some(wiz);
            }
        }
    }

    pub(super) async fn switch_account(&mut self, target: Account) -> Result<()> {
        if target.name == self.account.name {
            self.status = format!("already on {}", target.name);
            return Ok(());
        }
        // Different account = different credentials; drop any cached session.
        self.drop_sieve_session();
        let store = Store::open(&target.name).await?;
        self.store = store;
        self.account = target;
        self.folders = self.store.list_folders().await?;
        self.folder_state = ListState::default();
        if !self.folders.is_empty() {
            self.folder_state.select(Some(0));
        }
        self.msg_state = ListState::default();
        self.body.clear();
        self.body_scroll = 0;
        self.reload_messages().await?;
        self.pane = Pane::Folders;
        self.status = format!("switched to {}", self.account.name);
        Ok(())
    }

    /// Switch the active folder by name (called from `<Space>f` picker).
    pub(super) async fn switch_folder(&mut self, folder: String) -> Result<()> {
        let Some(idx) = self.folders.iter().position(|f| f.name == folder) else {
            self.status = format!("folder {folder} not found");
            return Ok(());
        };
        self.folder_state.select(Some(idx));
        self.reload_messages().await?;
        self.pane = Pane::Messages;
        self.status = format!("switched to {folder} ({} msgs)", self.messages.len());
        Ok(())
    }

    /// Switch account by name string (called from `<Space>b` picker).
    pub(super) async fn switch_account_by_name(&mut self, name: String) -> Result<()> {
        let cfg = inbx_config::load()?;
        let Some(target) = cfg.accounts.into_iter().find(|a| a.name == name) else {
            self.status = format!("account {name} not found in config");
            return Ok(());
        };
        self.switch_account(target).await
    }

    /// Move the message-list cursor to the row matching `uid`.
    pub(super) fn jump_to_uid(&mut self, uid: i64) {
        if let Some(idx) = self.messages.iter().position(|m| m.uid == uid) {
            self.msg_state.select(Some(idx));
            self.refresh_body();
            self.pane = Pane::Messages;
            self.status = format!("jumped to uid {uid}");
        } else {
            self.status = format!("uid {uid} not in current message list");
        }
    }

    /// Save attachment at `idx` from `parts` to `~/Downloads/<filename>`.
    pub(super) async fn save_attachment(
        &mut self,
        parts: &[(String, Vec<u8>)],
        idx: usize,
    ) -> Result<()> {
        let Some((name, data)) = parts.get(idx) else {
            self.status = "attachment index out of range".into();
            return Ok(());
        };
        let downloads = directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                std::path::PathBuf::from(home).join("Downloads")
            });
        std::fs::create_dir_all(&downloads)?;
        let dest = downloads.join(name);
        std::fs::write(&dest, data)?;
        self.status = format!("saved {} bytes → {}", data.len(), dest.display());
        Ok(())
    }

    pub(super) async fn open_outbox(&mut self) -> Result<()> {
        let entries = self.store.outbox_list().await?;
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        let n = entries.len();
        self.outbox = Some(OutboxState { entries, state });
        self.status = format!("outbox: {n} queued");
        Ok(())
    }

    /// Drain all outbox entries. Spawns a background task; result comes
    /// back as `TaskResult::OutboxDrained`.
    pub(super) fn drain_outbox(&mut self) {
        if !self.spawn_pending() {
            return;
        }
        self.status = "draining outbox…".into();
        let account = self.account.clone();
        let store = self.store.clone();
        let tx = self.task_tx.0.clone();
        tokio::spawn(async move {
            let result = do_drain_outbox(account, store).await;
            let _ = tx.send(result);
        });
    }

    pub(super) async fn drain_outbox_one(&mut self) -> Result<()> {
        let Some(row) = self.selected_outbox_entry().cloned() else {
            return Ok(());
        };
        match inbx_net::send_message(&self.account, &row.raw).await {
            Ok(()) => {
                self.store.outbox_delete(row.id).await?;
                self.status = format!("outbox: sent id={}", row.id);
            }
            Err(e) => {
                self.store
                    .outbox_record_failure(row.id, &e.to_string())
                    .await?;
                self.status = format!("outbox: id={} failed: {e}", row.id);
            }
        }
        self.reload_outbox().await?;
        Ok(())
    }

    pub(super) async fn delete_outbox_one(&mut self) -> Result<()> {
        let Some(row) = self.selected_outbox_entry().cloned() else {
            return Ok(());
        };
        self.store.outbox_delete(row.id).await?;
        self.reload_outbox().await?;
        self.status = format!("outbox: deleted id={}", row.id);
        Ok(())
    }

    fn selected_outbox_entry(&self) -> Option<&OutboxRow> {
        let ob = self.outbox.as_ref()?;
        ob.state.selected().and_then(|i| ob.entries.get(i))
    }

    async fn reload_outbox(&mut self) -> Result<()> {
        let entries = self.store.outbox_list().await?;
        if let Some(ob) = self.outbox.as_mut() {
            let prior = ob.state.selected();
            ob.entries = entries;
            if ob.entries.is_empty() {
                ob.state.select(None);
            } else {
                let next = prior.map(|i| i.min(ob.entries.len() - 1)).unwrap_or(0);
                ob.state.select(Some(next));
            }
        }
        Ok(())
    }

    /// Public wrapper around `reload_outbox` for use from the event-loop.
    pub(super) async fn reload_outbox_pub(&mut self) -> Result<()> {
        self.reload_outbox().await
    }

    pub(super) fn picker_targets(&self) -> Vec<String> {
        let filter = self
            .move_picker
            .as_ref()
            .map(|p| p.filter.to_ascii_lowercase())
            .unwrap_or_default();
        self.folders
            .iter()
            .filter(|f| f.name.to_ascii_lowercase().contains(&filter))
            .map(|f| f.name.clone())
            .collect()
    }

    pub(super) fn current_folder(&self) -> Option<&FolderRow> {
        self.folder_state
            .selected()
            .and_then(|i| self.folders.get(i))
    }

    pub(super) fn current_message(&self) -> Option<&MessageRow> {
        self.msg_state.selected().and_then(|i| self.messages.get(i))
    }

    pub(super) async fn reload_messages(&mut self) -> Result<()> {
        let folder = self.current_folder().map(|f| f.name.clone());
        let prior = self.msg_state.selected();
        self.messages = match folder {
            Some(name) => self.store.list_messages(&name, 200).await?,
            None => Vec::new(),
        };
        if self.messages.is_empty() {
            self.msg_state.select(None);
        } else {
            // Preserve the previous index when possible so toggling a flag
            // doesn't fling the cursor back to the top.
            let next = prior.map(|i| i.min(self.messages.len() - 1)).unwrap_or(0);
            self.msg_state.select(Some(next));
        }
        self.refresh_body();
        self.respawn_watch();
        Ok(())
    }

    pub(super) fn refresh_body(&mut self) {
        self.body_scroll = 0;
        self.current_receipt = None;
        self.current_rendered = None;
        // Clone what we need before dropping the borrow on self.
        let snapshot = self.current_message().map(|m| {
            (
                m.uid,
                m.maildir_path.clone(),
                m.folder.clone(),
                m.from_addr.clone(),
                m.subject.clone(),
                m.flags.clone(),
            )
        });
        let raw_headers = self.preview_raw_headers;
        match snapshot {
            None => {
                self.body.clear();
            }
            Some((uid, Some(path), ..)) => {
                if raw_headers {
                    // Show raw RFC 5322 headers (everything before the first blank line).
                    self.body = match std::fs::read(&path) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            // Split on \r\n\r\n or \n\n — whichever comes first.
                            let crlf_pos = text.find("\r\n\r\n");
                            let lf_pos = text.find("\n\n");
                            let end = match (crlf_pos, lf_pos) {
                                (Some(a), Some(b)) => a.min(b),
                                (Some(a), None) => a,
                                (None, Some(b)) => b,
                                (None, None) => text.len(),
                            };
                            text[..end].to_string()
                        }
                        Err(e) => format!("[error reading file: {e}]"),
                    };
                } else {
                    self.body = render_path(&path);
                    // Parse the rendered message for receipt + code_body.
                    if let Ok(raw) = std::fs::read(&path)
                        && let Ok(rendered) = inbx_render::render_message(&raw, RemotePolicy::Block)
                    {
                        if !self.receipt_responded.contains(&uid) {
                            self.current_receipt = rendered.read_receipt_request.clone();
                        }
                        self.current_rendered = Some(rendered);
                    }
                }
            }
            Some((_, None, folder, from_addr, subject, flags)) => {
                self.body = format!(
                    "[body not yet fetched — run `inbx fetch --bodies` to download]\n\n\
                     folder: {}\nfrom: {}\nsubject: {}\nflags: {}",
                    folder,
                    from_addr.as_deref().unwrap_or(""),
                    subject.as_deref().unwrap_or(""),
                    flags,
                );
            }
        }
        #[cfg(feature = "tree-sitter")]
        self.kick_grammar_load();
    }

    /// If the current rendered message has a `code_body` and the grammar is not
    /// yet cached, kick off an async load via the `AsyncGrammarLoader` and send
    /// the result back as `TaskResult::GrammarReady`.
    #[cfg(feature = "tree-sitter")]
    pub(super) fn kick_grammar_load(&self) {
        let Some(rendered) = self.current_rendered.as_ref() else {
            return;
        };
        let Some(cb) = rendered.code_body.as_ref() else {
            return;
        };
        let lang = cb.lang;

        // Skip if already cached (success or failure sentinel).
        {
            // Non-blocking: if lock is contended just skip — next refresh will retry.
            let Ok(cache) = self.grammar_cache.try_lock() else {
                return;
            };
            if cache.contains_key(lang) {
                return;
            }
        }

        let Some(spec) = self.grammar_registry.by_name(lang) else {
            // Language not in registry — cache a failure sentinel immediately.
            if let Ok(mut cache) = self.grammar_cache.try_lock() {
                cache.insert(lang, None);
            }
            return;
        };

        // Dispatch load via AsyncGrammarLoader (deduplicates concurrent calls).
        let async_loader = self.grammar_loader.clone();
        let meta = self.grammar_registry.meta().clone();
        let spec = spec.clone();
        let cache = self.grammar_cache.clone();
        let tx = self.task_tx.0.clone();

        tokio::task::spawn_blocking(move || {
            let handle = async_loader.load_async(lang.to_owned(), spec.clone(), meta.clone());
            // recv_blocking returns the PathBuf to the compiled .so.
            // Then Grammar::load through the inner loader finds it in user tier.
            let result = handle
                .recv_blocking()
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    hjkl_bonsai::runtime::Grammar::load(lang, &spec, async_loader.inner(), &meta)
                        .map(std::sync::Arc::new)
                        .map_err(|e| e.to_string())
                });

            let send_result = match &result {
                Ok(g) => {
                    let mut guard = cache.blocking_lock();
                    guard.insert(lang, Some(g.clone()));
                    Ok(g.clone())
                }
                Err(e) => {
                    let mut guard = cache.blocking_lock();
                    guard.insert(lang, None);
                    Err(e.clone())
                }
            };
            let _ = tx.send(super::tasks::TaskResult::GrammarReady {
                lang,
                result: send_result,
            });
        });
    }

    pub(super) fn step_list(&mut self, delta: i32) {
        let (state, len) = match self.pane {
            Pane::Folders => (&mut self.folder_state, self.folders.len()),
            Pane::Messages => (&mut self.msg_state, self.messages.len()),
            Pane::Preview => return,
        };
        if len == 0 {
            return;
        }
        let i = state.selected().unwrap_or(0) as i32 + delta;
        let i = i.rem_euclid(len as i32) as usize;
        state.select(Some(i));
    }

    pub(super) fn jump_top(&mut self) {
        match self.pane {
            Pane::Folders if !self.folders.is_empty() => self.folder_state.select(Some(0)),
            Pane::Messages if !self.messages.is_empty() => self.msg_state.select(Some(0)),
            _ => {}
        }
    }

    pub(super) fn jump_bottom(&mut self) {
        match self.pane {
            Pane::Folders if !self.folders.is_empty() => {
                self.folder_state.select(Some(self.folders.len() - 1))
            }
            Pane::Messages if !self.messages.is_empty() => {
                self.msg_state.select(Some(self.messages.len() - 1))
            }
            _ => {}
        }
    }

    pub(super) fn cycle_pane(&mut self, forward: bool) {
        self.pane = match (self.pane, forward) {
            (Pane::Folders, true) => Pane::Messages,
            (Pane::Messages, true) => Pane::Preview,
            (Pane::Preview, true) => Pane::Folders,
            (Pane::Folders, false) => Pane::Preview,
            (Pane::Messages, false) => Pane::Folders,
            (Pane::Preview, false) => Pane::Messages,
        };
    }

    /// Spawn (or re-spawn) the background watch loop bound to the currently-
    /// selected folder.  No-op if already watching that folder.  Aborts the
    /// previous task before spawning a new one — the old IDLE / EventSource /
    /// poll drops cleanly when the future is cancelled (sockets close on drop).
    pub(super) fn respawn_watch(&mut self) {
        // IPC mode: the sync daemon drives reloads via FolderUpdated events.
        // Suppress the in-process watch entirely to avoid duplicate syncs.
        if self.sync_ipc.is_some() {
            return;
        }
        let Some(folder) = self.current_folder().map(|f| f.name.clone()) else {
            return; // No folder selected — nothing to watch.
        };
        if self.watched_folder.as_deref() == Some(folder.as_str()) {
            return; // Already watching this folder.
        }
        if let Some(h) = self.watch_handle.take() {
            h.abort();
        }
        let account = self.account.clone();
        let store = self.store.clone();
        let tx = self.task_tx.0.clone();
        let f = folder.clone();
        self.watch_handle = Some(tokio::spawn(do_watch(account, f, store, tx)));
        self.watched_folder = Some(folder);
    }
}

// ---------------------------------------------------------------------------
// Free async helpers for background tasks
// ---------------------------------------------------------------------------

/// Perform a full folder sync off the event-loop thread.
///
/// Uses `connect_provider` so JMAP accounts skip the IMAP path automatically.
async fn do_manual_sync(
    account: inbx_config::Account,
    store: Store,
    folder_name: String,
) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let result: anyhow::Result<(i64, usize, usize)> = async {
        let mut provider = inbx_net::connect_provider(&account, Some(&store)).await?;
        let rows = provider.fetch_headers(&folder_name, None, 500).await?;
        // JMAP uses uidvalidity=0; IMAP rows carry real uidvalidity but
        // HeaderRow.uidvalidity is already u32→i64 clean.
        let uidvalidity: i64 = rows.first().map(|r| r.uidvalidity as i64).unwrap_or(0);
        let prev = store.folder_uidvalidity(&folder_name).await?;
        if let Some(prev) = prev
            && prev as u32 != uidvalidity as u32
            && uidvalidity != 0
        {
            store.wipe_folder_messages(&folder_name).await?;
        }
        store
            .upsert_folder(&inbx_store::FolderRow {
                name: folder_name.clone(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: Some(uidvalidity),
                uidnext: None,
                delta_link: None,
            })
            .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let pre_max = store
            .folder_max_uid(&folder_name, uidvalidity)
            .await?
            .unwrap_or(0);
        let mut new_count = 0usize;
        for h in &rows {
            if (h.uid as i64) > pre_max {
                new_count += 1;
            }
            store
                .upsert_message(&inbx_store::MessageRow {
                    folder: folder_name.clone(),
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
                    fetched_at_unix: now,
                    in_reply_to: None,
                    refs: None,
                    thread_id: None,
                    provider_id: h.provider_id.clone(),
                })
                .await?;
        }
        // IMAP sessions need logout; JMAP is stateless HTTP (no-op).
        drop(provider);
        Ok((now, new_count, rows.len()))
    }
    .await;
    match result {
        Ok((now, new_count, total)) => TaskResult::SyncDone {
            last_sync_unix: Some(now),
            error: None,
            new_messages: new_count,
            folder_name,
            total_messages: total,
        },
        Err(e) => TaskResult::SyncDone {
            last_sync_unix: None,
            error: Some(e.to_string()),
            new_messages: 0,
            folder_name,
            total_messages: 0,
        },
    }
}

/// Fetch a single message body off the event-loop thread.
///
/// Uses `connect_provider` so JMAP accounts skip the IMAP path automatically.
async fn do_fetch_body(
    account: inbx_config::Account,
    store: Store,
    folder_name: String,
    uid: i64,
    uidvalidity: i64,
    flags: String,
) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let result: anyhow::Result<()> = async {
        let mut provider = inbx_net::connect_provider(&account, Some(&store)).await?;
        let raw = provider.fetch_body(&folder_name, uid).await?;
        drop(provider);
        let path = store.write_maildir(&folder_name, &raw, &flags)?;
        store
            .set_maildir_path(&folder_name, uid, uidvalidity, &path.to_string_lossy())
            .await?;
        Ok(())
    }
    .await;
    TaskResult::BodyFetched {
        uid,
        error: result.err().map(|e| e.to_string()),
    }
}

/// Drain the outbox off the event-loop thread.
async fn do_drain_outbox(account: inbx_config::Account, store: Store) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let entries = match store.outbox_list().await {
        Ok(e) => e,
        Err(_) => {
            return TaskResult::OutboxDrained { sent: 0, failed: 0 };
        }
    };
    let mut sent = 0usize;
    let mut failed = 0usize;
    for row in entries {
        match inbx_net::send_message(&account, &row.raw).await {
            Ok(()) => {
                let _ = store.outbox_delete(row.id).await;
                sent += 1;
            }
            Err(e) => {
                let _ = store.outbox_record_failure(row.id, &e.to_string()).await;
                failed += 1;
            }
        }
    }
    TaskResult::OutboxDrained { sent, failed }
}

type SieveCache = std::sync::Arc<tokio::sync::Mutex<Option<inbx_net::sieve::SieveClient>>>;

/// Lazy-connect helper: reuses the cached `SieveClient` if live, or
/// establishes a fresh connection. On any op error the cache is cleared so
/// the next call reconnects from scratch (the session may be in an
/// unrecoverable state after a server error).
async fn ensure_sieve_connected(
    account: &inbx_config::Account,
    cache: &SieveCache,
) -> std::result::Result<(), String> {
    let mut guard = cache.lock().await;
    if guard.is_none() {
        let client = inbx_net::sieve::SieveClient::connect(account)
            .await
            .map_err(|e| e.to_string())?;
        *guard = Some(client);
    }
    Ok(())
}

/// Connect to ManageSieve and list scripts off the event-loop thread.
async fn do_sieve_list(
    account: inbx_config::Account,
    cache: SieveCache,
) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let result = async {
        ensure_sieve_connected(&account, &cache).await?;
        let mut guard = cache.lock().await;
        let client = guard.as_mut().expect("just connected");
        match client.list_scripts().await.map_err(|e| e.to_string()) {
            Ok(scripts) => Ok(scripts),
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }
    .await;
    TaskResult::SieveScripts(result)
}

/// Fetch a sieve script body off the event-loop thread.
async fn do_sieve_get(
    account: inbx_config::Account,
    cache: SieveCache,
    name: String,
) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let body = async {
        ensure_sieve_connected(&account, &cache).await?;
        let mut guard = cache.lock().await;
        let client = guard.as_mut().expect("just connected");
        match client.get_script(&name).await.map_err(|e| e.to_string()) {
            Ok(body) => Ok(body),
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }
    .await;
    TaskResult::SieveBody { name, body }
}

/// Save a sieve script off the event-loop thread.
async fn do_sieve_put(
    account: inbx_config::Account,
    cache: SieveCache,
    name: String,
    body: String,
) -> super::tasks::TaskResult {
    use super::tasks::TaskResult;
    let result = async {
        ensure_sieve_connected(&account, &cache).await?;
        let mut guard = cache.lock().await;
        let client = guard.as_mut().expect("just connected");
        match client
            .put_script(&name, &body)
            .await
            .map_err(|e| e.to_string())
        {
            Ok(()) => Ok(()),
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }
    .await;
    TaskResult::SieveSaved { name, body, result }
}

fn extract_list_unsubscribe(raw: &[u8]) -> Option<String> {
    let parsed = mail_parser::MessageParser::default().parse(raw)?;
    let val = parsed.header_values("List-Unsubscribe").next()?;
    val.as_text().map(|s| s.to_string())
}

fn parse_unsubscribe_uris(header: &str) -> Vec<String> {
    header
        .split(',')
        .filter_map(|part| {
            let s = part.trim();
            let s = s.strip_prefix('<').unwrap_or(s);
            let s = s.strip_suffix('>').unwrap_or(s);
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
        .collect()
}

fn parse_mailto(uri: &str) -> (String, String, String) {
    let stripped = uri.strip_prefix("mailto:").unwrap_or(uri);
    let (addr, query) = match stripped.split_once('?') {
        Some((a, q)) => (a, q),
        None => (stripped, ""),
    };
    let mut subject = String::new();
    let mut body = String::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let decoded = url_decode(v);
        match k.to_ascii_lowercase().as_str() {
            "subject" => subject = decoded,
            "body" => body = decoded,
            _ => {}
        }
    }
    if subject.is_empty() {
        subject = "unsubscribe".to_string();
    }
    (addr.to_string(), subject, body)
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_unsubscribe_mime(from: &str, to: &str, subject: &str, body: &str) -> Vec<u8> {
    mail_builder::MessageBuilder::new()
        .from((String::new(), from.to_string()))
        .to(vec![(String::new(), to.to_string())])
        .subject(subject)
        .text_body(body)
        .write_to_vec()
        .unwrap_or_default()
}

fn strip_mailto(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("mailto:") {
        &s[s.len() - rest.len()..]
    } else {
        s
    }
}

fn build_ical_reply_mime(from: &str, to: &str, subject: &str, ics: &[u8]) -> Vec<u8> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::content_type::ContentType;
    use mail_builder::mime::{BodyPart, MimePart};

    let part = MimePart::new(
        ContentType::new("text/calendar")
            .attribute("method", "REPLY")
            .attribute("charset", "utf-8"),
        BodyPart::Binary(ics.to_vec().into()),
    );
    MessageBuilder::new()
        .from((String::new(), from.to_string()))
        .to(vec![(String::new(), to.to_string())])
        .subject(subject)
        .body(part)
        .write_to_vec()
        .unwrap_or_default()
}

fn open_url(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let primary = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if primary.is_ok() {
        return Ok(());
    }
    Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Background watch loop
// ---------------------------------------------------------------------------

/// Long-lived background task: waits for the server to signal new data, then
/// posts `TaskResult::WatchSignal` so the TUI event loop triggers a sync.
///
/// Dispatches on `account.transport`:
/// - `Transport::Imap` → RFC 2177 IDLE via `inbx_net::idle::wait_for_new_in`.
/// - `Transport::Jmap` → RFC 8620 EventSource (`open_event_source`).  Any
///   SSE event (or stream close + reopen) is treated as a signal.
/// - `Transport::Graph` → no push path today; loop sleeps 5 min and signals.
///   (A delta-link poll is tracked as a separate TODO.)
///
/// Reconnect / backoff: any error backs off 30 s before retrying, matching the
/// CLI `inbx watch` and `inbx-sync` patterns.
///
/// The task is per-folder: `App::respawn_watch` aborts and replaces it whenever
/// the active folder changes, so the watch always tracks the current view.
///
/// `tx.send` errors (receiver dropped on TUI exit) terminate the loop cleanly.
async fn do_watch(
    account: inbx_config::Account,
    folder: String,
    store: inbx_store::Store,
    tx: tokio::sync::mpsc::UnboundedSender<super::tasks::TaskResult>,
) {
    use super::tasks::TaskResult;
    use inbx_config::Transport;

    const BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

    loop {
        match &account.transport {
            Transport::Imap => {
                match inbx_net::idle::wait_for_new_in(&account, &folder).await {
                    Ok(inbx_net::idle::IdleEvent::NewData) => {
                        tracing::debug!("watch: IMAP IDLE new data");
                    }
                    Ok(inbx_net::idle::IdleEvent::Timeout) => {
                        // Keepalive cycle — re-issue IDLE without triggering sync.
                        tracing::debug!("watch: IMAP IDLE keepalive");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(%e, "watch: IMAP IDLE error; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                }
            }
            Transport::Jmap { session_url } => {
                // Connect and open the EventSource.  Any SSE event signals new
                // data; stream close is treated as a signal too (reconnect loop).
                let client = match inbx_net::jmap::JmapClient::connect(&account, session_url).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(%e, "watch: JMAP connect failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                let mut stream = match client.open_event_source().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(%e, "watch: JMAP EventSource open failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                tracing::debug!("watch: JMAP EventSource open");
                // Block until the first event arrives (or the stream closes).
                match stream.next_event().await {
                    Ok(Some(payload)) => {
                        tracing::debug!(%payload, "watch: JMAP push event");
                    }
                    Ok(None) => {
                        // Stream closed; reconnect on next iteration.
                        tracing::debug!("watch: JMAP EventSource closed; reconnecting");
                    }
                    Err(e) => {
                        tracing::warn!(%e, "watch: JMAP EventSource error; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                }
            }
            Transport::Graph => {
                // Delta-link poll: connect, resolve folder id, fetch changes.
                let client = match inbx_net::graph::GraphClient::connect(&account).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(%e, "watch: Graph connect failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                // Resolve folder display name → Graph folder id.
                let folder_id = match client.list_folders().await {
                    Ok(folders) => {
                        match folders
                            .iter()
                            .find(|f| f.display_name.eq_ignore_ascii_case(&folder))
                            .map(|f| f.id.clone())
                        {
                            Some(id) => id,
                            None => {
                                tracing::warn!(
                                    folder,
                                    "watch: Graph folder not found; backing off 30s"
                                );
                                tokio::time::sleep(BACKOFF).await;
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%e, "watch: Graph list_folders failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                // Load stored delta link (None on first run).
                let stored_link = match store.get_delta_link(&folder).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(%e, "watch: Graph get_delta_link failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                // Call delta endpoint.
                let (messages, new_link) = match client
                    .delta_messages(&folder_id, stored_link.as_deref())
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(%e, "watch: Graph delta_messages failed; backing off 30s");
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                };
                // Persist new delta link.
                if let Err(e) = store.set_delta_link(&folder, new_link.as_deref()).await {
                    tracing::warn!(%e, "watch: Graph set_delta_link failed (ignored)");
                }
                if messages.is_empty() {
                    // No changes — sleep before next poll, don't signal.
                    tracing::debug!("watch: Graph delta — no changes; sleeping 75s");
                    tokio::time::sleep(std::time::Duration::from_secs(75)).await;
                    continue;
                }
                tracing::debug!(count = messages.len(), "watch: Graph delta — new messages");
            }
        }

        // Signal the TUI; exit cleanly if the receiver is gone.
        if tx.send(TaskResult::WatchSignal).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_step_wraps_forward_at_end() {
        // Cursor at last index, +1 wraps back to 0.
        assert_eq!(search_step(3, 2, 1), Some(0));
    }

    #[test]
    fn search_step_wraps_backward_at_start() {
        // Cursor at 0, -1 wraps to last index.
        assert_eq!(search_step(3, 0, -1), Some(2));
    }

    #[test]
    fn search_step_advances_within_range() {
        assert_eq!(search_step(5, 2, 1), Some(3));
        assert_eq!(search_step(5, 2, -1), Some(1));
    }

    #[test]
    fn search_step_empty_returns_none() {
        // No results — n/N must be a no-op.
        assert_eq!(search_step(0, 0, 1), None);
        assert_eq!(search_step(0, 0, -1), None);
    }

    #[test]
    fn search_step_single_result_stays_put() {
        // One result — wraparound returns the same index either direction.
        assert_eq!(search_step(1, 0, 1), Some(0));
        assert_eq!(search_step(1, 0, -1), Some(0));
    }

    fn fake_msg(folder: &str, uid: i64) -> MessageRow {
        MessageRow {
            folder: folder.to_string(),
            uid,
            uidvalidity: 1,
            message_id: None,
            subject: Some(format!("subj-{uid}")),
            from_addr: Some("a@b".into()),
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            maildir_path: None,
            headers_only: 1,
            fetched_at_unix: 0,
            in_reply_to: None,
            refs: None,
            thread_id: None,
            provider_id: None,
        }
    }

    #[test]
    fn build_search_state_empty_when_no_prior() {
        let s = build_search_state(None);
        assert!(s.query.is_empty());
        assert!(s.results.is_empty());
        assert_eq!(s.state.selected(), None);
    }

    #[test]
    fn build_search_state_restores_query_results_and_cursor() {
        let prior = LastSearch {
            query: "alpha".into(),
            results: vec![
                fake_msg("INBOX", 1),
                fake_msg("INBOX", 2),
                fake_msg("INBOX", 3),
            ],
            cursor: 1,
        };
        let s = build_search_state(Some(&prior));
        assert_eq!(s.query, "alpha");
        assert_eq!(s.results.len(), 3);
        assert_eq!(s.state.selected(), Some(1));
    }

    #[test]
    fn build_search_state_clamps_stale_cursor() {
        // last_search.cursor exceeds new result length (defensive — currently
        // unreachable, but cheap to guard against).
        let prior = LastSearch {
            query: "x".into(),
            results: vec![fake_msg("INBOX", 7)],
            cursor: 99,
        };
        let s = build_search_state(Some(&prior));
        assert_eq!(s.state.selected(), Some(0));
    }

    #[test]
    fn build_search_state_no_selection_when_results_empty() {
        let prior = LastSearch {
            query: "noresults".into(),
            results: vec![],
            cursor: 0,
        };
        let s = build_search_state(Some(&prior));
        assert_eq!(s.state.selected(), None);
        assert_eq!(s.query, "noresults");
    }

    fn flagged(uid: i64, flags: &str) -> MessageRow {
        let mut m = fake_msg("INBOX", uid);
        m.flags = flags.to_string();
        m
    }

    #[test]
    fn unread_count_zero_when_all_seen() {
        let rows = vec![flagged(1, "\\Seen"), flagged(2, "\\Seen \\Flagged")];
        assert_eq!(unread_count(&rows), 0);
    }

    #[test]
    fn unread_count_counts_missing_seen() {
        // Empty flag string and a "starred but unread" row both count as unread.
        let rows = vec![
            flagged(1, "\\Seen"),
            flagged(2, ""),
            flagged(3, "\\Flagged"),
        ];
        assert_eq!(unread_count(&rows), 2);
    }

    #[test]
    fn unread_count_case_insensitive() {
        // Defensive: real flag tokens are `\Seen` but normalize anyway.
        let rows = vec![flagged(1, "\\SEEN"), flagged(2, "\\seen"), flagged(3, "")];
        assert_eq!(unread_count(&rows), 1);
    }
}
