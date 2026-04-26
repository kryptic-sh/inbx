use std::io::{Stdout, stdout};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use inbx_composer::{Composer, Field as ComposerField, Identity};
use inbx_config::Account;
use inbx_config::theme::{Rgb, Theme};
use inbx_store::{FolderRow, MessageRow, Store};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Folders,
    Messages,
    Preview,
}

struct App {
    account: Account,
    store: Store,
    folders: Vec<FolderRow>,
    folder_state: ListState,
    messages: Vec<MessageRow>,
    msg_state: ListState,
    pane: Pane,
    pending_g: bool,
    body: String,
    body_scroll: u16,
    status: String,
    composer: Option<Composer>,
    show_help: bool,
    move_picker: Option<MovePickerState>,
}

struct MovePickerState {
    filter: String,
    state: ListState,
}

impl MovePickerState {
    fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            filter: String::new(),
            state,
        }
    }
}

impl App {
    async fn new(account: Account, store: Store) -> Result<Self> {
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
        };
        app.reload_messages().await?;
        Ok(app)
    }

    fn open_blank(&mut self) {
        self.composer = Some(Composer::new_blank(Identity::from_account(&self.account)));
        self.status = "compose: new draft".into();
    }

    async fn open_reply(&mut self, all: bool) -> Result<()> {
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

    async fn open_forward(&mut self) -> Result<()> {
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

    async fn save_draft(&mut self) -> Result<()> {
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

    async fn send_composer(&mut self) -> Result<()> {
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

    fn close_composer(&mut self) {
        self.composer = None;
        self.status = "draft discarded".into();
    }

    async fn toggle_seen(&mut self) -> Result<()> {
        self.toggle_flag("\\Seen").await
    }

    async fn toggle_starred(&mut self) -> Result<()> {
        self.toggle_flag("\\Flagged").await
    }

    async fn toggle_deleted(&mut self) -> Result<()> {
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

    async fn expunge(&mut self) -> Result<()> {
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

    async fn move_current_to(&mut self, target: &str) -> Result<()> {
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

    fn picker_targets(&self) -> Vec<String> {
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

    fn current_folder(&self) -> Option<&FolderRow> {
        self.folder_state
            .selected()
            .and_then(|i| self.folders.get(i))
    }

    fn current_message(&self) -> Option<&MessageRow> {
        self.msg_state.selected().and_then(|i| self.messages.get(i))
    }

    async fn reload_messages(&mut self) -> Result<()> {
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

    fn refresh_body(&mut self) {
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

    fn step_list(&mut self, delta: i32) {
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

    fn jump_top(&mut self) {
        match self.pane {
            Pane::Folders if !self.folders.is_empty() => self.folder_state.select(Some(0)),
            Pane::Messages if !self.messages.is_empty() => self.msg_state.select(Some(0)),
            _ => {}
        }
    }

    fn jump_bottom(&mut self) {
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

    fn cycle_pane(&mut self, forward: bool) {
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

pub async fn run(account: Account) -> Result<()> {
    let store = Store::open(&account.name).await?;
    let mut app = App::new(account, store).await?;

    let mut terminal = setup_terminal()?;
    let res = event_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    res
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    term.show_cursor()?;
    Ok(())
}

async fn event_loop(term: &mut Term, app: &mut App) -> Result<()> {
    let mut events = EventStream::new();
    loop {
        term.draw(|f| draw(f, app))?;
        let Some(ev) = events.next().await else {
            break;
        };
        let ev = ev?;
        if let Event::Key(key) = ev {
            if app.composer.is_some() {
                if handle_composer_key(app, key).await? {
                    break;
                }
                continue;
            }
            if app.move_picker.is_some() {
                handle_move_picker_key(app, key).await?;
                continue;
            }
            if handle_list_key(app, key).await? {
                break;
            }
        }
    }
    Ok(())
}

/// Returns true to quit the TUI.
async fn handle_list_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if app.show_help {
        // Any key dismisses the help overlay.
        app.show_help = false;
        return Ok(false);
    }
    if key.code == KeyCode::Char('?') {
        app.show_help = true;
        return Ok(false);
    }
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Ok(true);
    }

    // Compose / reply / forward shortcuts.
    match key.code {
        KeyCode::Char('c') => {
            app.open_blank();
            return Ok(false);
        }
        KeyCode::Char('r') => {
            app.open_reply(false).await?;
            return Ok(false);
        }
        KeyCode::Char('R') => {
            app.open_reply(true).await?;
            return Ok(false);
        }
        KeyCode::Char('f') => {
            app.open_forward().await?;
            return Ok(false);
        }
        _ => {}
    }

    // Mutation shortcuts on the messages pane.
    if app.pane == Pane::Messages {
        match key.code {
            KeyCode::Char('s') => {
                app.toggle_seen().await?;
                return Ok(false);
            }
            KeyCode::Char('*') => {
                app.toggle_starred().await?;
                return Ok(false);
            }
            KeyCode::Char('d') => {
                app.toggle_deleted().await?;
                return Ok(false);
            }
            KeyCode::Char('e') => {
                app.expunge().await?;
                return Ok(false);
            }
            KeyCode::Char('m') => {
                app.move_picker = Some(MovePickerState::new());
                return Ok(false);
            }
            _ => {}
        }
    }

    // Body scroll keys when Preview pane is focused.
    if app.pane == Pane::Preview {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                app.body_scroll = app.body_scroll.saturating_add(1);
                return Ok(false);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.body_scroll = app.body_scroll.saturating_sub(1);
                return Ok(false);
            }
            KeyCode::PageDown => {
                app.body_scroll = app.body_scroll.saturating_add(10);
                return Ok(false);
            }
            KeyCode::PageUp => {
                app.body_scroll = app.body_scroll.saturating_sub(10);
                return Ok(false);
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.body_scroll = app.body_scroll.saturating_add(10);
                return Ok(false);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.body_scroll = app.body_scroll.saturating_sub(10);
                return Ok(false);
            }
            KeyCode::Char('g') => {
                if app.pending_g {
                    app.body_scroll = 0;
                    app.pending_g = false;
                } else {
                    app.pending_g = true;
                }
                return Ok(false);
            }
            KeyCode::Char('G') => {
                let lines = app.body.lines().count() as u16;
                app.body_scroll = lines.saturating_sub(1);
                app.pending_g = false;
                return Ok(false);
            }
            _ => {}
        }
    }

    // Pane movement (always available)
    match key.code {
        KeyCode::Tab => app.cycle_pane(true),
        KeyCode::BackTab => app.cycle_pane(false),
        KeyCode::Char('h') => app.cycle_pane(false),
        KeyCode::Char('l') => app.cycle_pane(true),
        _ => {}
    }

    // List navigation
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.pending_g = false;
            app.step_list(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.pending_g = false;
            app.step_list(-1);
        }
        KeyCode::Char('g') => {
            if app.pending_g {
                app.jump_top();
                app.pending_g = false;
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => {
            app.pending_g = false;
            app.jump_bottom();
        }
        KeyCode::Enter => {
            app.pending_g = false;
            if app.pane == Pane::Folders {
                app.reload_messages().await?;
                app.pane = Pane::Messages;
                app.status = format!("loaded {} messages", app.messages.len());
            } else if app.pane == Pane::Messages {
                app.refresh_body();
                app.pane = Pane::Preview;
            }
        }
        _ => {
            if !matches!(
                key.code,
                KeyCode::Char('g') | KeyCode::Tab | KeyCode::BackTab
            ) {
                app.pending_g = false;
            }
        }
    }

    // Selection updates body / messages.
    match app.pane {
        Pane::Folders => {
            app.reload_messages().await?;
        }
        Pane::Messages => app.refresh_body(),
        Pane::Preview => {}
    }
    Ok(false)
}

async fn handle_composer_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    // Global composer commands ride above the editor's input grammar.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                app.send_composer().await?;
                return Ok(false);
            }
            KeyCode::Char('d') => {
                app.save_draft().await?;
                return Ok(false);
            }
            KeyCode::Char('q') => {
                app.close_composer();
                return Ok(false);
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Tab {
        if let Some(c) = app.composer.as_mut() {
            c.focus_next();
        }
        return Ok(false);
    }
    if key.code == KeyCode::BackTab {
        if let Some(c) = app.composer.as_mut() {
            c.focus_prev();
        }
        return Ok(false);
    }
    if let Some(c) = app.composer.as_mut() {
        c.focused_editor().handle_key(key);
    }
    Ok(false)
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    if let Some(c) = app.composer.as_ref() {
        draw_composer(f, c, &app.status, outer[0]);
        draw_status(f, app, outer[1]);
        return;
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(28),
            Constraint::Length(40),
            Constraint::Min(20),
        ])
        .split(outer[0]);

    draw_folders(f, app, body[0]);
    draw_messages(f, app, body[1]);
    draw_preview(f, app, body[2]);
    if app.move_picker.is_some() {
        draw_move_picker(f, app, outer[0]);
    }
    if app.show_help {
        draw_help(f, outer[0]);
    }
    draw_status(f, app, outer[1]);
}

fn draw_help(f: &mut ratatui::Frame, area: Rect) {
    let lines = [
        "  navigation",
        "    j / k       — down / up",
        "    h / l, Tab  — switch pane",
        "    g g         — top of list",
        "    G           — bottom of list",
        "    Enter       — open folder / preview",
        "",
        "  message ops (messages pane)",
        "    s           — toggle \\Seen",
        "    *           — toggle \\Flagged",
        "    d           — toggle \\Deleted",
        "    e           — EXPUNGE folder",
        "",
        "  compose",
        "    c           — new draft",
        "    r / R       — reply / reply-all",
        "    f           — forward",
        "",
        "  composer",
        "    Tab / S-Tab — cycle field",
        "    Ctrl-S      — send",
        "    Ctrl-D      — save draft to server",
        "    Ctrl-Q      — discard",
        "",
        "  global",
        "    ?           — toggle this help",
        "    q / Ctrl-C  — quit",
    ];
    let height = (lines.len() as u16 + 2).min(area.height);
    let width = 56u16.min(area.width);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines.join("\n"))
        .block(pane_block("help (any key dismisses)", true))
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

/// Process-wide theme handle. The single `App::run` sets it before the
/// event loop starts; the standalone `pane_block` helper reads it. We
/// keep a OnceLock instead of threading `&Theme` through every draw
/// function — there's only ever one theme per process.
static ACTIVE_THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

fn theme() -> &'static Theme {
    ACTIVE_THEME.get_or_init(Theme::default)
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let t = theme();
    let color = if focused {
        rgb(&t.focused)
    } else {
        rgb(&t.unfocused)
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(color))
}

fn rgb(c: &Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn draw_folders(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .folders
        .iter()
        .map(|fld| {
            let suffix = fld
                .special_use
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            ListItem::new(format!("{}{}", fld.name, suffix))
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("folders", app.pane == Pane::Folders))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.folder_state.clone());
}

fn draw_messages(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| {
            let unread = !m.flags.to_ascii_lowercase().contains("seen");
            let from = m.from_addr.clone().unwrap_or_default();
            let subj = m.subject.clone().unwrap_or_default();
            let line = format!("{}  {}", truncate(&from, 18), truncate(&subj, 60));
            let style = if unread {
                Style::default()
                    .fg(rgb(&theme().unread))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("messages", app.pane == Pane::Messages))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.msg_state.clone());
}

fn draw_preview(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let title = app
        .current_message()
        .and_then(|m| m.subject.clone())
        .unwrap_or_else(|| "preview".into());
    let para = Paragraph::new(app.body.as_str())
        .block(pane_block(&title, app.pane == Pane::Preview))
        .wrap(Wrap { trim: false })
        .scroll((app.body_scroll, 0));
    f.render_widget(para, area);
}

fn draw_status(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pane = match app.pane {
        Pane::Folders => "folders",
        Pane::Messages => "messages",
        Pane::Preview => "preview",
    };
    let text = format!(
        " [{pane}]  ? help · q quit · h/l pane · j/k move · Enter open  {}",
        app.status
    );
    let t = theme();
    let para =
        Paragraph::new(text).style(Style::default().bg(rgb(&t.status_bg)).fg(rgb(&t.status_fg)));
    f.render_widget(para, area);
}

fn draw_composer(f: &mut ratatui::Frame, composer: &Composer, status: &str, area: Rect) {
    f.render_widget(Clear, area);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // subject
            Constraint::Length(3), // to
            Constraint::Length(3), // cc
            Constraint::Length(3), // bcc
            Constraint::Min(5),    // body
        ])
        .split(area);

    let fields = [
        (ComposerField::Subject, "subject", layout[0]),
        (ComposerField::To, "to", layout[1]),
        (ComposerField::Cc, "cc", layout[2]),
        (ComposerField::Bcc, "bcc", layout[3]),
    ];
    for (field, label, area) in fields {
        let focused = composer.focus == field;
        draw_field(
            f,
            label,
            composer_field_text(composer, field),
            focused,
            area,
        );
        if focused {
            place_cursor(f, composer, field, area);
        }
    }

    let body_title =
        format!("body — Tab field · Ctrl-S send · Ctrl-D draft · Ctrl-Q discard · {status}");
    let body_para = Paragraph::new(composer.body_text())
        .block(pane_block(
            &body_title,
            composer.focus == ComposerField::Body,
        ))
        .wrap(Wrap { trim: false });
    f.render_widget(body_para, layout[4]);
    if composer.focus == ComposerField::Body {
        place_cursor(f, composer, ComposerField::Body, layout[4]);
    }
}

fn place_cursor(f: &mut ratatui::Frame, composer: &Composer, field: ComposerField, area: Rect) {
    let editor = match field {
        ComposerField::Subject => &composer.subject,
        ComposerField::To => &composer.to,
        ComposerField::Cc => &composer.cc,
        ComposerField::Bcc => &composer.bcc,
        ComposerField::Body => &composer.body,
    };
    let (row, col) = editor.cursor();
    // Account for the surrounding border (1px) on every side.
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    if inner_w == 0 || inner_h == 0 {
        return;
    }
    let row = row.min(inner_h as usize - 1);
    let col = col.min(inner_w as usize - 1);
    f.set_cursor_position((area.x + 1 + col as u16, area.y + 1 + row as u16));
}

fn composer_field_text(c: &Composer, f: ComposerField) -> String {
    match f {
        ComposerField::Subject => c.subject_text(),
        ComposerField::To => c.to_text(),
        ComposerField::Cc => editor_text_ref(&c.cc),
        ComposerField::Bcc => editor_text_ref(&c.bcc),
        ComposerField::Body => c.body_text(),
    }
}

fn editor_text_ref(ed: &hjkl_editor::runtime::Editor<'static>) -> String {
    ed.content().trim_end_matches('\n').to_string()
}

fn draw_field(f: &mut ratatui::Frame, label: &str, value: String, focused: bool, area: Rect) {
    let para = Paragraph::new(value)
        .block(pane_block(label, focused))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        format!("{s:<n$}")
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn render_path(path: &str) -> String {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return format!("(unable to read {path}: {e})"),
    };
    let auth = inbx_render::auth::evaluate(&raw);
    let auth_line = format!(
        "[spf={:?} dkim={:?} dmarc={:?}]",
        auth.auth.spf, auth.auth.dkim, auth.auth.dmarc
    );
    let security = inbx_render::pgp::detect(&raw);
    let security_line = match security.label {
        Some(l) => format!("[{l}]\n"),
        None => String::new(),
    };
    let mut warnings: Vec<&str> = Vec::new();
    if auth.phishing.reply_to_mismatch {
        warnings.push("reply-to mismatch");
    }
    if auth.phishing.display_name_email {
        warnings.push("display name has @");
    }
    if auth.phishing.lookalike_from {
        warnings.push("lookalike domain");
    }
    let warn_line = if warnings.is_empty() {
        String::new()
    } else {
        format!("[!! {}]\n", warnings.join("; "))
    };
    match inbx_render::render_message(&raw, inbx_render::RemotePolicy::Block) {
        Ok(r) => {
            let banner = if r.blocked_remote > 0 || !r.trackers.is_empty() {
                format!(
                    "[remote content blocked: {} url(s); trackers: {}]\n",
                    r.blocked_remote,
                    r.trackers.len()
                )
            } else {
                String::new()
            };
            format!(
                "{auth_line}\n{security_line}{warn_line}{banner}\n{}",
                r.plain
            )
        }
        Err(e) => format!(
            "{auth_line}\n{security_line}{warn_line}(render error: {e})\n\n{}",
            String::from_utf8_lossy(&raw)
        ),
    }
}

async fn handle_move_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let targets = app.picker_targets();
    let Some(picker) = app.move_picker.as_mut() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => {
            app.move_picker = None;
        }
        KeyCode::Enter => {
            let idx = picker.state.selected().unwrap_or(0);
            if let Some(target) = targets.get(idx).cloned() {
                app.move_picker = None;
                app.move_current_to(&target).await?;
            }
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            let len = targets.len();
            if len > 0 {
                let cur = picker.state.selected().unwrap_or(0);
                picker
                    .state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            let len = targets.len();
            if len > 0 {
                let cur = picker.state.selected().unwrap_or(0);
                picker.state.select(Some((cur + 1) % len));
            }
        }
        KeyCode::Backspace => {
            picker.filter.pop();
            picker.state.select(Some(0));
        }
        KeyCode::Char(c) => {
            picker.filter.push(c);
            picker.state.select(Some(0));
        }
        _ => {}
    }
    Ok(())
}

fn draw_move_picker(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(picker) = app.move_picker.as_ref() else {
        return;
    };
    let targets = app.picker_targets();
    let height = (targets.len() as u16 + 4).min(area.height).max(6);
    let width = 60u16.min(area.width);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(popup);

    let filter_para = Paragraph::new(format!("/{}", picker.filter))
        .block(pane_block("move to (Esc cancel · Enter pick)", true));
    f.render_widget(filter_para, layout[0]);

    let items: Vec<ListItem> = targets
        .iter()
        .map(|name| ListItem::new(name.clone()))
        .collect();
    let list = List::new(items)
        .block(pane_block("folders", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut picker.state.clone());
}
