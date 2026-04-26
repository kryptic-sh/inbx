use inbx_composer::{Composer, Field as ComposerField};
use inbx_config::theme::{Rgb, Theme};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::ACTIVE_THEME;
use super::app::{App, Pane};

pub(super) fn draw(f: &mut ratatui::Frame, app: &App) {
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
    if app.outbox.is_some() {
        draw_outbox(f, app, outer[0]);
    }
    if app.search.is_some() {
        draw_search(f, app, outer[0]);
    }
    if app.thread.is_some() {
        draw_thread(f, app, outer[0]);
    }
    if app.account_picker.is_some() {
        draw_account_picker(f, app, outer[0]);
    }
    if app.contacts.is_some() {
        draw_contacts(f, app, outer[0]);
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
        "    m           — move to folder",
        "    F           — manual sync",
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

fn theme() -> &'static Theme {
    ACTIVE_THEME.get_or_init(Theme::default)
}

pub(super) fn pane_block(title: &str, focused: bool) -> Block<'_> {
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

pub(super) fn render_path(path: &str) -> String {
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

fn draw_outbox(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(ob) = app.outbox.as_ref() else {
        return;
    };
    let height = (ob.entries.len() as u16 + 4).min(area.height).max(6);
    let width = 80u16.min(area.width);
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

    let header = Paragraph::new(format!("{} queued", ob.entries.len())).block(pane_block(
        "outbox (D drain all · d drain one · x delete · j/k · Esc)",
        true,
    ));
    f.render_widget(header, layout[0]);

    let items: Vec<ListItem> = ob
        .entries
        .iter()
        .map(|r| {
            let err = r
                .last_error
                .as_deref()
                .map(|s| truncate(s, 32))
                .unwrap_or_else(|| truncate("", 32));
            let line = format!(
                "id={:<5} att={:<3} q={:<11} err={}",
                r.id, r.attempts, r.enqueued_unix, err
            );
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("entries", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut ob.state.clone());
}

fn draw_search(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(s) = app.search.as_ref() else {
        return;
    };
    let height = (s.results.len() as u16 + 5).min(area.height).max(8);
    let width = 80u16.min(area.width);
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

    let query_para = Paragraph::new(format!("/{}", s.query))
        .block(pane_block("search (Enter run/jump · j/k · Esc)", true));
    f.render_widget(query_para, layout[0]);

    let items: Vec<ListItem> = s
        .results
        .iter()
        .map(|m| {
            let from = m.from_addr.clone().unwrap_or_default();
            let subj = m.subject.clone().unwrap_or_default();
            let line = format!(
                "{}  {}  {}",
                truncate(&m.folder, 14),
                truncate(&from, 18),
                truncate(&subj, 40)
            );
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("results", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut s.state.clone());
}

fn draw_thread(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(t) = app.thread.as_ref() else {
        return;
    };
    let height = (t.messages.len() as u16 + 4).min(area.height).max(6);
    let width = 90u16.min(area.width);
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

    let header = Paragraph::new(format!("{} message(s) in thread", t.messages.len()))
        .block(pane_block("thread (Enter jump · j/k · Esc)", true));
    f.render_widget(header, layout[0]);

    let items: Vec<ListItem> = t
        .messages
        .iter()
        .map(|m| {
            let date = m
                .date_unix
                .map(format_date_utc)
                .unwrap_or_else(|| "          ".into());
            let from = m.from_addr.clone().unwrap_or_default();
            let subj = m.subject.clone().unwrap_or_default();
            let line = format!("{}  {}  {}", date, truncate(&from, 20), truncate(&subj, 44));
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("messages", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut t.state.clone());
}

/// Format a unix timestamp as `YYYY-MM-DD` in UTC. Uses Howard Hinnant's
/// civil-from-days algorithm to avoid pulling in chrono/time for a single
/// call site.
fn format_date_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    // Shift epoch to 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn draw_account_picker(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(picker) = app.account_picker.as_ref() else {
        return;
    };
    let height = (picker.accounts.len() as u16 + 4).min(area.height).max(6);
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

    let header = Paragraph::new(format!("current: {}", app.account.name))
        .block(pane_block("switch account (Enter pick · j/k · Esc)", true));
    f.render_widget(header, layout[0]);

    let items: Vec<ListItem> = picker
        .accounts
        .iter()
        .map(|a| {
            let marker = if a.name == app.account.name { "*" } else { " " };
            ListItem::new(format!("{marker} {}  <{}>", a.name, a.email))
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("accounts", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut picker.state.clone());
}

fn draw_contacts(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(state) = app.contacts.as_ref() else {
        return;
    };
    let filtered = app.contacts_filtered();
    let height = (filtered.len() as u16 + 4).min(area.height).max(6);
    let width = 70u16.min(area.width);
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

    let filter_para = Paragraph::new(format!("/{}", state.filter)).block(pane_block(
        "contacts (Esc cancel · Enter compose · j/k)",
        true,
    ));
    f.render_widget(filter_para, layout[0]);

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|c| {
            let name = c.name.as_deref().unwrap_or("");
            let line = if name.is_empty() {
                c.email.clone()
            } else {
                format!("{}  <{}>", truncate(name, 28), c.email)
            };
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(pane_block("entries", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, layout[1], &mut state.state.clone());
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
