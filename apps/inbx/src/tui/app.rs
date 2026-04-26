use anyhow::Result;
use inbx_composer::{Composer, Identity};
use inbx_config::Account;
use inbx_store::{FolderRow, MessageRow, OutboxRow, Store};
use ratatui::widgets::ListState;

use super::ACTIVE_THEME;
use super::render::render_path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Folders,
    Messages,
    Preview,
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
    pub(super) body: String,
    pub(super) body_scroll: u16,
    pub(super) status: String,
    pub(super) composer: Option<Composer>,
    pub(super) show_help: bool,
    pub(super) move_picker: Option<MovePickerState>,
    pub(super) outbox: Option<OutboxState>,
}

pub(super) struct MovePickerState {
    pub(super) filter: String,
    pub(super) state: ListState,
}

pub(super) struct OutboxState {
    pub(super) entries: Vec<OutboxRow>,
    pub(super) state: ListState,
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
            body: String::new(),
            body_scroll: 0,
            status: String::new(),
            composer: None,
            show_help: false,
            move_picker: None,
            outbox: None,
        };
        app.reload_messages().await?;
        Ok(app)
    }

    pub(super) fn open_blank(&mut self) {
        self.composer = Some(Composer::new_blank(Identity::from_account(&self.account)));
        self.status = "compose: new draft".into();
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
        self.status = format!(
            "synced {folder_name} ({} msgs, {new_count} new)",
            rows.len()
        );
        Ok(())
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
