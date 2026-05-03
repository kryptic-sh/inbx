use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hjkl_engine::{Input as EngineInput, Key as EngineKey};
use hjkl_form::FormMode;
use hjkl_picker::PickerEvent;
use inbx_composer::FocusedEditor;

use super::app::{ActivePicker, App, IcalResponse, LeaderState, MovePickerState, Pane};
use super::wizard::AccountWizard;

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

    if key.code == KeyCode::Char('a') {
        app.open_account_picker()?;
        return Ok(false);
    }

    // List-pane leader: <Space> arms the prefix; second key opens a picker.
    if app.pending_leader == Some(LeaderState::Pending) {
        app.pending_leader = None;
        match key.code {
            KeyCode::Char('f') => {
                app.open_folder_picker();
                return Ok(false);
            }
            KeyCode::Char('b') => {
                app.open_hjkl_account_picker()?;
                return Ok(false);
            }
            KeyCode::Char('m') => {
                app.open_message_picker();
                return Ok(false);
            }
            KeyCode::Char('a') => {
                app.open_attachment_picker();
                return Ok(false);
            }
            KeyCode::Char('n') => {
                app.active_wizard = Some(AccountWizard::new());
                app.status = "wizard: new account — <Space>s save · Esc cancel".into();
                return Ok(false);
            }
            _ => {
                // Unrecognised chord — fall through.
            }
        }
    }
    if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
        app.pending_leader = Some(LeaderState::Pending);
        return Ok(false);
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

    if key.code == KeyCode::Char('/') {
        app.open_search();
        return Ok(false);
    }

    // n / N — jump to next / prev match from the most recent `/` search
    // without reopening the overlay. No-op when there's no prior search.
    if key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
        app.step_last_search(1).await?;
        return Ok(false);
    }
    if key.code == KeyCode::Char('N') && key.modifiers.is_empty() {
        app.step_last_search(-1).await?;
        return Ok(false);
    }

    if key.code == KeyCode::Char('C') {
        app.open_contacts().await?;
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
            KeyCode::Char('T') => {
                app.open_thread().await?;
                return Ok(false);
            }
            KeyCode::Char('i') => {
                app.open_ical().await?;
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

    // Leader-key chord: <Space> arms the prefix; a second key dispatches.
    // <Space>y — yank focused editor text to system clipboard.
    // <Space>p — replace focused editor text from system clipboard.
    if app.pending_leader == Some(LeaderState::Pending) {
        app.pending_leader = None;
        match key.code {
            KeyCode::Char('y') => {
                app.yank_to_clipboard();
                return Ok(false);
            }
            KeyCode::Char('p') => {
                app.put_from_clipboard();
                return Ok(false);
            }
            _ => {
                // Unrecognised chord — fall through and forward the key to the editor.
            }
        }
    }
    if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
        app.pending_leader = Some(LeaderState::Pending);
        return Ok(false);
    }

    if let Some(c) = app.composer.as_mut() {
        match c.focused_editor() {
            FocusedEditor::Body(ed) => {
                ed.handle_key(key);
            }
            FocusedEditor::Header(f) => {
                f.handle_input(crossterm_key_to_engine_input(key));
            }
        }
    }
    Ok(false)
}

/// Convert a crossterm `KeyEvent` to an `hjkl_engine::Input`.
/// Mirrors the `From<KeyEvent> for Input` impl in hjkl-engine (crossterm feature).
fn crossterm_key_to_engine_input(key: KeyEvent) -> EngineInput {
    let k = match key.code {
        KeyCode::Char(c) => EngineKey::Char(c),
        KeyCode::Backspace => EngineKey::Backspace,
        KeyCode::Delete => EngineKey::Delete,
        KeyCode::Enter => EngineKey::Enter,
        KeyCode::Left => EngineKey::Left,
        KeyCode::Right => EngineKey::Right,
        KeyCode::Up => EngineKey::Up,
        KeyCode::Down => EngineKey::Down,
        KeyCode::Home => EngineKey::Home,
        KeyCode::End => EngineKey::End,
        KeyCode::Tab => EngineKey::Tab,
        KeyCode::Esc => EngineKey::Esc,
        _ => EngineKey::Null,
    };
    EngineInput {
        key: k,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    }
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

pub(super) async fn handle_search_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.search = None;
            return Ok(());
        }
        KeyCode::Enter => {
            let has_results = app
                .search
                .as_ref()
                .map(|s| !s.results.is_empty())
                .unwrap_or(false);
            if has_results {
                let pick = app.search.as_ref().and_then(|s| {
                    s.state.selected().map(|i| {
                        let row = &s.results[i];
                        (i, row.folder.clone(), row.uid)
                    })
                });
                if let Some((idx, folder, uid)) = pick {
                    if let Some(ls) = app.last_search.as_mut() {
                        ls.cursor = idx;
                    }
                    app.search = None;
                    app.jump_to_message(&folder, uid).await?;
                }
            } else {
                app.run_search().await?;
            }
            return Ok(());
        }
        _ => {}
    }

    let has_results = app
        .search
        .as_ref()
        .map(|s| !s.results.is_empty())
        .unwrap_or(false);

    if has_results {
        let len = app.search.as_ref().map(|s| s.results.len()).unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                if len > 0
                    && let Some(s) = app.search.as_mut()
                {
                    let cur = s.state.selected().unwrap_or(0);
                    let next = if cur == 0 { len - 1 } else { cur - 1 };
                    s.state.select(Some(next));
                    if let Some(ls) = app.last_search.as_mut() {
                        ls.cursor = next;
                    }
                }
                return Ok(());
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                if len > 0
                    && let Some(s) = app.search.as_mut()
                {
                    let cur = s.state.selected().unwrap_or(0);
                    let next = (cur + 1) % len;
                    s.state.select(Some(next));
                    if let Some(ls) = app.last_search.as_mut() {
                        ls.cursor = next;
                    }
                }
                return Ok(());
            }
            _ => {}
        }
    }

    // Fall through: edit the query input.
    if let Some(s) = app.search.as_mut() {
        match key.code {
            KeyCode::Backspace => {
                s.query.pop();
                // Editing invalidates results so Enter re-runs.
                s.results.clear();
                s.state.select(None);
            }
            KeyCode::Char(c) => {
                s.query.push(c);
                s.results.clear();
                s.state.select(None);
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) async fn handle_account_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app
        .account_picker
        .as_ref()
        .map(|p| p.accounts.len())
        .unwrap_or(0);
    match key.code {
        KeyCode::Esc => {
            app.account_picker = None;
        }
        KeyCode::Enter => {
            let pick = app
                .account_picker
                .as_ref()
                .and_then(|p| p.state.selected().and_then(|i| p.accounts.get(i)).cloned());
            if let Some(acct) = pick {
                app.account_picker = None;
                app.switch_account(acct).await?;
            }
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(p) = app.account_picker.as_mut()
            {
                let cur = p.state.selected().unwrap_or(0);
                p.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(p) = app.account_picker.as_mut()
            {
                let cur = p.state.selected().unwrap_or(0);
                p.state.select(Some((cur + 1) % len));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn handle_contacts_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let filtered = app.contacts_filtered();
    let len = filtered.len();
    let Some(state) = app.contacts.as_mut() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => {
            app.contacts = None;
        }
        KeyCode::Enter => {
            let idx = state.state.selected().unwrap_or(0);
            if let Some(c) = filtered.get(idx).cloned() {
                app.contacts = None;
                app.compose_to_contact(&c.email);
            }
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() && len > 0 => {
            let cur = state.state.selected().unwrap_or(0);
            state
                .state
                .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() && len > 0 => {
            let cur = state.state.selected().unwrap_or(0);
            state.state.select(Some((cur + 1) % len));
        }
        KeyCode::Backspace => {
            state.filter.pop();
            state.state.select(Some(0));
        }
        KeyCode::Char(c) => {
            state.filter.push(c);
            state.state.select(Some(0));
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn handle_ical_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.close_ical(),
        KeyCode::Char('a') => app.respond_ical(IcalResponse::Accept).await?,
        KeyCode::Char('t') => app.respond_ical(IcalResponse::Tentative).await?,
        KeyCode::Char('d') => app.respond_ical(IcalResponse::Decline).await?,
        _ => {}
    }
    Ok(())
}

pub(super) async fn handle_thread_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.thread.as_ref().map(|t| t.messages.len()).unwrap_or(0);
    match key.code {
        KeyCode::Esc => {
            app.thread = None;
        }
        KeyCode::Enter => {
            let pick = app.thread.as_ref().and_then(|t| {
                t.state
                    .selected()
                    .and_then(|i| t.messages.get(i))
                    .map(|m| (m.folder.clone(), m.uid))
            });
            if let Some((folder, uid)) = pick {
                app.thread = None;
                app.jump_to_message(&folder, uid).await?;
            }
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(t) = app.thread.as_mut()
            {
                let cur = t.state.selected().unwrap_or(0);
                t.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(t) = app.thread.as_mut()
            {
                let cur = t.state.selected().unwrap_or(0);
                t.state.select(Some((cur + 1) % len));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Route key events when the account-creation wizard is open.
pub(super) async fn handle_wizard_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Some(wizard) = app.active_wizard.as_mut() else {
        return Ok(());
    };

    // In Normal mode, <Space>s = save, Esc = cancel.
    if wizard.form.mode == FormMode::Normal {
        if key.code == KeyCode::Esc {
            app.active_wizard = None;
            app.status = "wizard: cancelled".into();
            return Ok(());
        }
        // Leader chord for save: <Space>s.
        // We repurpose `pending_leader` already in App for this.
        if app.pending_leader == Some(super::app::LeaderState::Pending) {
            app.pending_leader = None;
            if key.code == KeyCode::Char('s') {
                // Take the wizard out so we can consume it.
                let wizard = app.active_wizard.take().unwrap();
                match wizard.build_account() {
                    Ok((acct, password)) => match save_wizard_account(acct, &password) {
                        Ok(name) => {
                            app.status = format!("added account {name}");
                        }
                        Err(e) => {
                            app.status = format!("wizard save failed: {e}");
                        }
                    },
                    Err(e) => {
                        app.status = format!("wizard validation: {e}");
                        // Put a fresh wizard back so the user can correct it.
                        // (Simple: just discard state and warn.)
                    }
                }
                return Ok(());
            }
            // Any other <Space>X: put leader back as unrecognised and fall through.
        }
        if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
            app.pending_leader = Some(super::app::LeaderState::Pending);
            return Ok(());
        }
    }

    let Some(wizard) = app.active_wizard.as_mut() else {
        return Ok(());
    };

    let prev_focus = wizard.form.focused();
    wizard.form.handle_input(crossterm_key_to_engine_input(key));
    let new_focus = wizard.form.focused();

    // Email-blur autoconfig hook.
    if prev_focus == 1 && new_focus != 1 && !wizard.suggestion_applied {
        wizard.maybe_apply_autoconfig();
    }
    wizard.last_focused = new_focus;

    // Update status with focused field name.
    let label = wizard.focused_label().to_string();
    app.status = format!("wizard: {label} — <Space>s save · Esc cancel");

    Ok(())
}

fn save_wizard_account(acct: inbx_config::Account, password: &str) -> Result<String> {
    let name = acct.name.clone();
    let mut cfg = inbx_config::load()?;
    cfg.accounts.push(acct);
    inbx_config::store_password(&name, password)?;
    inbx_config::save(&cfg)?;
    Ok(name)
}

/// Route key events when an `active_picker` overlay is open.
/// Returns `true` if the picker was closed (either cancelled or accepted).
pub(super) async fn handle_active_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Pull the picker out temporarily to avoid borrow-checker trouble.
    let Some(mut picker_state) = app.active_picker.take() else {
        return Ok(());
    };

    let event = match &mut picker_state {
        ActivePicker::Folder(p, _) => p.handle_key(key),
        ActivePicker::Account(p, _) => p.handle_key(key),
        ActivePicker::Message(p, _) => p.handle_key(key),
        ActivePicker::Attachment(p, _, _) => p.handle_key(key),
    };

    match event {
        PickerEvent::Cancel => {
            // picker_state dropped — overlay closed.
            app.status = "picker cancelled".into();
        }
        PickerEvent::Select(_) => {
            // Drain the stashed slot and dispatch the inbx action.
            match picker_state {
                ActivePicker::Folder(_, slot) => {
                    if let Some(folder) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.switch_folder(folder).await?;
                    }
                }
                ActivePicker::Account(_, slot) => {
                    if let Some(name) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.switch_account_by_name(name).await?;
                    }
                }
                ActivePicker::Message(_, slot) => {
                    if let Some(uid) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.jump_to_uid(uid);
                    }
                }
                ActivePicker::Attachment(_, slot, parts) => {
                    if let Some(idx) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.save_attachment(&parts, idx).await?;
                    }
                }
            }
        }
        PickerEvent::None => {
            // Refresh the picker filter then put it back.
            match &mut picker_state {
                ActivePicker::Folder(p, _) => {
                    p.refresh();
                }
                ActivePicker::Account(p, _) => {
                    p.refresh();
                }
                ActivePicker::Message(p, _) => {
                    p.refresh();
                }
                ActivePicker::Attachment(p, _, _) => {
                    p.refresh();
                }
            }
            app.active_picker = Some(picker_state);
        }
    }
    Ok(())
}
