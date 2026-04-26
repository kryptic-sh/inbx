use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, MovePickerState, Pane};

/// Returns true to quit the TUI.
pub(super) async fn handle_list_key(app: &mut App, key: KeyEvent) -> Result<bool> {
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
    if key.code == KeyCode::Char('F') {
        app.manual_sync().await?;
        return Ok(false);
    }

    if key.code == KeyCode::Char('L') {
        app.oauth_login().await?;
        return Ok(false);
    }

    if key.code == KeyCode::Char('O') {
        app.open_outbox().await?;
        return Ok(false);
    }

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
            KeyCode::Char('U') => {
                app.unsubscribe_current().await?;
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
                // Lazy body fetch: if local body is missing, pull it from
                // IMAP before switching to preview.
                if let Some(m) = app.current_message()
                    && m.maildir_path.is_none()
                {
                    app.fetch_current_body().await?;
                }
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

pub(super) async fn handle_composer_key(app: &mut App, key: KeyEvent) -> Result<bool> {
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

pub(super) async fn handle_outbox_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.outbox.as_ref().map(|o| o.entries.len()).unwrap_or(0);
    match key.code {
        KeyCode::Esc => {
            app.outbox = None;
        }
        KeyCode::Char('D') => {
            app.drain_outbox().await?;
        }
        KeyCode::Char('d') => {
            app.drain_outbox_one().await?;
        }
        KeyCode::Char('x') => {
            app.delete_outbox_one().await?;
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(ob) = app.outbox.as_mut()
            {
                let cur = ob.state.selected().unwrap_or(0);
                ob.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(ob) = app.outbox.as_mut()
            {
                let cur = ob.state.selected().unwrap_or(0);
                ob.state.select(Some((cur + 1) % len));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn handle_move_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
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
