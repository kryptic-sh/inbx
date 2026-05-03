use std::sync::{Arc, Mutex};

use anyhow::Result;
use hjkl_clipboard::{Clipboard, MimeType as ClipMime, Selection};
use hjkl_picker::Picker;
use inbx_composer::{Composer, FocusedEditor, Identity};
use inbx_config::Account;
use inbx_contacts::{Contact, ContactsStore};
use inbx_store::{FolderRow, MessageRow, OutboxRow, Store};
use ratatui::widgets::ListState;

use super::ACTIVE_THEME;
use super::render::render_path;

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
    pub(super) body: String,
    pub(super) body_scroll: u16,
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
    /// Active account-creation wizard. `None` when not open.
    pub(super) active_wizard: Option<super::wizard::AccountWizard>,
}

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

pub(super) struct MovePickerState {
    pub(super) filter: String,
    pub(super) state: ListState,
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
            body: String::new(),
            body_scroll: 0,
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
            active_wizard: None,
        };
        app.reload_messages().await?;
        Ok(app)
    }

    pub(super) fn open_blank(&mut self) {
        self.composer = Some(Composer::new_blank(Identity::from_account(&self.account)));
        self.status = "compose: new draft".into();
    }

    pub(super) async fn open_contacts(&mut self) -> Result<()> {
        let store = ContactsStore::open(&self.account.name).await?;
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
        match Composer::new_reply(Identity::from_account(&self.account), &raw, all) {
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

    pub(super) async fn save_draft(&mut self) -> Result<()> {
        let Some(composer) = self.composer.as_ref() else {
            return Ok(());
        };
        let raw = composer.to_mime()?;
        let mut session = inbx_net::connect_imap(&self.account).await?;
        let folders = inbx_net::list_folders(&mut session).await?;
        match inbx_net::find_drafts_folder(&folders) {
            Some(drafts) => {
                inbx_net::append_draft(&mut session, &drafts, &raw).await?;
                self.composer = None;
                self.status = format!("draft saved to {drafts}");
            }
            None => {
                self.status = "no Drafts folder discovered".into();
            }
        }
        let _ = session.logout().await;
        Ok(())
    }

    pub(super) async fn send_composer(&mut self) -> Result<()> {
        let Some(composer) = self.composer.as_ref() else {
            return Ok(());
        };
        let raw = composer.to_mime()?;
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
        let mut session = inbx_net::connect_imap(&self.account).await?;
        let op = if has { "-FLAGS" } else { "+FLAGS" };
        inbx_net::store_flags(&mut session, &folder_name, &[msg.uid as u32], op, flag).await?;
        let _ = session.logout().await;
        let (add, remove): (Vec<&str>, Vec<&str>) = if has {
            (vec![], vec![flag])
        } else {
            (vec![flag], vec![])
        };
        self.store
            .mutate_flags(&folder_name, &[msg.uid], &add, &remove)
            .await?;
        self.reload_messages().await?;
        self.status = format!("{}{flag}", if has { "removed " } else { "added " });
        Ok(())
    }

    /// Manual sync trigger (`F`). Connects, refreshes the current folder's
    /// headers, and downloads the body for the currently-selected message
    /// if it's still header-only.
    pub(super) async fn manual_sync(&mut self) -> Result<()> {
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        self.status = format!("syncing {folder_name}…");
        let mut session = inbx_net::connect_imap(&self.account).await?;
        let (uidvalidity, rows) = inbx_net::fetch_headers(&mut session, &folder_name).await?;
        let prev = self.store.folder_uidvalidity(&folder_name).await?;
        if let Some(prev) = prev
            && prev as u32 != uidvalidity
        {
            self.store.wipe_folder_messages(&folder_name).await?;
        }
        self.store
            .upsert_folder(&inbx_store::FolderRow {
                name: folder_name.clone(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: Some(uidvalidity as i64),
                uidnext: None,
                delta_link: None,
            })
            .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut new_count = 0usize;
        let pre_max = self
            .store
            .folder_max_uid(&folder_name, uidvalidity as i64)
            .await?
            .unwrap_or(0);
        for h in &rows {
            if (h.uid as i64) > pre_max {
                new_count += 1;
            }
            self.store
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
                })
                .await?;
        }
        let _ = session.logout().await;
        self.reload_messages().await?;
        self.last_sync_unix = Some(now);
        self.status = format!(
            "synced {folder_name} ({} msgs, {new_count} new)",
            rows.len()
        );
        Ok(())
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

    /// Lazy body fetch. If the selected message is header-only, pull its
    /// body from IMAP, write to Maildir, update store, and refresh the
    /// preview.
    pub(super) async fn fetch_current_body(&mut self) -> Result<()> {
        let Some(msg) = self.current_message().cloned() else {
            return Ok(());
        };
        if msg.maildir_path.is_some() {
            return Ok(());
        }
        let Some(folder_name) = self.current_folder().map(|f| f.name.clone()) else {
            return Ok(());
        };
        self.status = format!("fetching body for uid {}…", msg.uid);
        let mut session = inbx_net::connect_imap(&self.account).await?;
        let bodies = inbx_net::fetch_bodies(&mut session, &folder_name, &[msg.uid as u32]).await?;
        let _ = session.logout().await;
        if let Some((uid, raw)) = bodies.into_iter().next() {
            let path = self.store.write_maildir(&folder_name, &raw, &msg.flags)?;
            self.store
                .set_maildir_path(
                    &folder_name,
                    uid as i64,
                    msg.uidvalidity,
                    &path.to_string_lossy(),
                )
                .await?;
            self.reload_messages().await?;
            self.status = format!("fetched body uid {}", msg.uid);
        } else {
            self.status = "no body returned".into();
        }
        Ok(())
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
        match inbx_net::oauth_login(&self.account.auth, &provider).await {
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
        let mut session = inbx_net::connect_imap(&self.account).await?;
        let n = inbx_net::expunge_folder(&mut session, &folder_name).await?;
        let _ = session.logout().await;
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
        let mut session = inbx_net::connect_imap(&self.account).await?;
        inbx_net::uid_move(&mut session, &source, &[msg.uid as u32], target).await?;
        let _ = session.logout().await;
        self.store.delete_messages(&source, &[msg.uid]).await?;
        self.reload_messages().await?;
        self.status = format!("moved uid {} → {target}", msg.uid);
        Ok(())
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

    pub(super) async fn switch_account(&mut self, target: Account) -> Result<()> {
        if target.name == self.account.name {
            self.status = format!("already on {}", target.name);
            return Ok(());
        }
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

    pub(super) async fn drain_outbox(&mut self) -> Result<()> {
        let entries = self.store.outbox_list().await?;
        let total = entries.len();
        let mut sent = 0usize;
        let mut failed = 0usize;
        for row in entries {
            match inbx_net::send_message(&self.account, &row.raw).await {
                Ok(()) => {
                    self.store.outbox_delete(row.id).await?;
                    sent += 1;
                }
                Err(e) => {
                    self.store
                        .outbox_record_failure(row.id, &e.to_string())
                        .await?;
                    failed += 1;
                }
            }
        }
        self.reload_outbox().await?;
        self.status = format!("outbox drain: {sent}/{total} sent, {failed} failed");
        Ok(())
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
        Ok(())
    }

    pub(super) fn refresh_body(&mut self) {
        self.body_scroll = 0;
        match self.current_message() {
            None => {
                self.body.clear();
            }
            Some(m) => match m.maildir_path.as_deref() {
                Some(path) => self.body = render_path(path),
                None => {
                    self.body = format!(
                        "[body not yet fetched — run `inbx fetch --bodies` to download]\n\n\
                         folder: {}\nuid: {}\nfrom: {}\nsubject: {}\nflags: {}",
                        m.folder,
                        m.uid,
                        m.from_addr.as_deref().unwrap_or(""),
                        m.subject.as_deref().unwrap_or(""),
                        m.flags,
                    );
                }
            },
        }
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
