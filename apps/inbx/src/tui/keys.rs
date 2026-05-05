use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hjkl_engine::{Input as EngineInput, Key as EngineKey};
use hjkl_form::FormMode;
use hjkl_picker::PickerEvent;
use inbx_composer::FocusedEditor;

use super::app::{ActivePicker, App, FolderCrudPrompt, LeaderState};
use super::binds::{Action, Context};

/// Returns true to quit the TUI.
///
/// All key dispatch goes through `Action::from_key` → `Action::invoke`.
/// Gate predicates in the BINDS table encode the pane/state conditions that
/// previously lived as hand-rolled `if app.pane == …` blocks.
///
/// The only inline bookkeeping that remains:
/// • Help-overlay dismissal (pre-table; no action needed).
/// • Leader arming (`<Space>` consumed, not dispatched).
/// • `pending_g` arming for the `gg` chord (consumed on second `g`).
/// • Post-action pane-follow-up (reload messages / refresh body) that was
///   already baked into MoveDown/MoveUp::invoke — kept here only for the
///   cases where from_key falls through with no action (pending_g clear).
pub(super) async fn handle_list_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    // 1. Help overlay: any key dismisses it.
    if app.show_help {
        app.show_help = false;
        return Ok(false);
    }

    // 2. Leader-chord dispatch: consume when leader is armed.
    if app.pending_leader == Some(LeaderState::Pending) {
        app.pending_leader = None;
        if let Some(act) = Action::from_key(
            app,
            Context::List,
            key,
            Some(LeaderState::Pending),
            false,
            false,
        ) {
            return act.invoke(app).await;
        }
        // Unrecognised leader chord — fall through to plain dispatch.
    }

    // Arm the leader on <Space>.
    if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
        app.pending_leader = Some(LeaderState::Pending);
        return Ok(false);
    }

    // 3. g-chord arming / resolution.
    //    First `g` arms pending_g and waits for a second key.
    //    Second `g` is dispatched via from_key (JumpTop or ScrollBodyTop,
    //    depending on the gate).
    if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
        if app.pending_g {
            // Second `g` — dispatch through table (gate picks the right action).
            let act = Action::from_key(app, Context::List, key, None, false, true);
            if let Some(a) = act {
                return a.invoke(app).await;
            }
            // Shouldn't happen if table is correct, but reset state.
            app.pending_g = false;
            return Ok(false);
        } else {
            // First `g` — arm.
            app.pending_g = true;
            return Ok(false);
        }
    }

    // 4. Plain dispatch through the binds table.
    if let Some(act) = Action::from_key(
        app,
        Context::List,
        key,
        app.pending_leader,
        false,
        app.pending_g,
    ) {
        let was_g_consumer = matches!(act, Action::JumpTop | Action::ScrollBodyTop);
        let result = act.invoke(app).await?;
        if !was_g_consumer {
            app.pending_g = false;
        }
        return Ok(result);
    }

    // No binding matched.
    // Clear pending_g for keys that aren't `g`, Tab or BackTab.
    if !matches!(
        key.code,
        KeyCode::Char('g') | KeyCode::Tab | KeyCode::BackTab
    ) {
        app.pending_g = false;
    }
    Ok(false)
}

pub(super) async fn handle_composer_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    // Ctrl-G chord: arms the PGP toggle prefix. Only fires in the composer pane.
    if app.pending_pgp_chord {
        if let Some(action) = Action::from_key(app, Context::Composer, key, None, true, false) {
            match action {
                Action::ComposerPgpSign | Action::ComposerPgpEncrypt => {
                    return action.invoke(app).await;
                }
                _ => {}
            }
        }
        // Cancelled — fall through and process the key normally.
        app.pending_pgp_chord = false;
        app.status = "pgp chord cancelled".into();
    }

    // Global composer commands.
    if let Some(action) = Action::from_key(
        app,
        Context::Composer,
        key,
        app.pending_leader,
        false,
        false,
    ) {
        match action {
            Action::ComposerSend
            | Action::ComposerSaveDraft
            | Action::ComposerDiscard
            | Action::ComposerFocusNext
            | Action::ComposerFocusPrev
            | Action::ComposerPgpArm
            | Action::ComposerYank
            | Action::ComposerPaste => {
                // Clear leader before invoke.
                app.pending_leader = None;
                return action.invoke(app).await;
            }
            _ => {}
        }
    }

    // Arm leader if Space pressed without a bind.
    if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
        app.pending_leader = Some(LeaderState::Pending);
        return Ok(false);
    }
    // Clear leader for any other key.
    app.pending_leader = None;

    // Pass remaining keys to the editor / header field.
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
///
/// # TODO
/// `hjkl_ratatui::crossterm_bridge::crossterm_key_event_to_input` returns
/// `hjkl_engine::PlannedInput` (the SPEC variant), while `hjkl_form` and
/// `hjkl_editor` currently consume `hjkl_engine::Input` (the legacy runtime
/// variant). No `From<PlannedInput> for Input` conversion exists in
/// `hjkl-engine 0.3.3`. Replace this bespoke helper with the bridge function
/// once `Input` and `PlannedInput` unify upstream.
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

    // j/k/Up/Down navigation handled separately because it directly mutates
    // the overlay list state (not an App-level method).
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(ob) = app.outbox.as_mut()
            {
                let cur = ob.state.selected().unwrap_or(0);
                ob.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
            return Ok(());
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(ob) = app.outbox.as_mut()
            {
                let cur = ob.state.selected().unwrap_or(0);
                ob.state.select(Some((cur + 1) % len));
            }
            return Ok(());
        }
        _ => {}
    }

    if let Some(action) = Action::from_key(app, Context::Outbox, key, None, false, false) {
        action.invoke(app).await?;
    }
    Ok(())
}

pub(super) async fn handle_move_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // j/k/Up/Down navigation for the move picker.
    let targets = app.picker_targets();
    let len = targets.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(picker) = app.move_picker.as_mut()
            {
                let cur = picker.state.selected().unwrap_or(0);
                picker
                    .state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
            return Ok(());
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(picker) = app.move_picker.as_mut()
            {
                let cur = picker.state.selected().unwrap_or(0);
                picker.state.select(Some((cur + 1) % len));
            }
            return Ok(());
        }
        // Filter editing — typed characters filter the list.
        KeyCode::Backspace => {
            if let Some(picker) = app.move_picker.as_mut() {
                picker.filter.pop();
                picker.state.select(Some(0));
            }
            return Ok(());
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = app.move_picker.as_mut() {
                picker.filter.push(c);
                picker.state.select(Some(0));
            }
            return Ok(());
        }
        _ => {}
    }

    if let Some(action) = Action::from_key(app, Context::MovePicker, key, None, false, false) {
        action.invoke(app).await?;
    }
    Ok(())
}

pub(super) async fn handle_search_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Esc and Enter are handled by the binds table for close/confirm, but
    // search confirm requires checking whether results are available — keep
    // that logic here.
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

    // j/k/Up/Down navigate results when results exist.
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
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(p) = app.account_picker.as_mut()
            {
                let cur = p.state.selected().unwrap_or(0);
                p.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
            return Ok(());
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(p) = app.account_picker.as_mut()
            {
                let cur = p.state.selected().unwrap_or(0);
                p.state.select(Some((cur + 1) % len));
            }
            return Ok(());
        }
        _ => {}
    }

    if let Some(action) = Action::from_key(app, Context::AccountPicker, key, None, false, false) {
        action.invoke(app).await?;
    }
    Ok(())
}

pub(super) async fn handle_contacts_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let filtered = app.contacts_filtered();
    let len = filtered.len();

    match key.code {
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() && len > 0 => {
            if let Some(state) = app.contacts.as_mut() {
                let cur = state.state.selected().unwrap_or(0);
                state
                    .state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
            return Ok(());
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() && len > 0 => {
            if let Some(state) = app.contacts.as_mut() {
                let cur = state.state.selected().unwrap_or(0);
                state.state.select(Some((cur + 1) % len));
            }
            return Ok(());
        }
        // Filter editing.
        KeyCode::Backspace => {
            if let Some(state) = app.contacts.as_mut() {
                state.filter.pop();
                state.state.select(Some(0));
            }
            return Ok(());
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(state) = app.contacts.as_mut() {
                state.filter.push(c);
                state.state.select(Some(0));
            }
            return Ok(());
        }
        _ => {}
    }

    if let Some(action) = Action::from_key(app, Context::Contacts, key, None, false, false) {
        action.invoke(app).await?;
    }
    Ok(())
}

pub(super) async fn handle_ical_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if let Some(action) = Action::from_key(app, Context::Ical, key, None, false, false) {
        action.invoke(app).await?;
    }
    Ok(())
}

pub(super) async fn handle_thread_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.thread.as_ref().map(|t| t.messages.len()).unwrap_or(0);

    match key.code {
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(t) = app.thread.as_mut()
            {
                let cur = t.state.selected().unwrap_or(0);
                t.state
                    .select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            }
            return Ok(());
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if len > 0
                && let Some(t) = app.thread.as_mut()
            {
                let cur = t.state.selected().unwrap_or(0);
                t.state.select(Some((cur + 1) % len));
            }
            return Ok(());
        }
        _ => {}
    }

    if let Some(action) = Action::from_key(app, Context::Thread, key, None, false, false) {
        action.invoke(app).await?;
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
                    }
                }
                return Ok(());
            }
            // Any other <Space>X: fall through.
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
        ActivePicker::Sieve(p, _) => p.handle_key(key),
        ActivePicker::Template(p, _) => p.handle_key(key),
    };

    match event {
        PickerEvent::Cancel => {
            // Sieve picker cancelled without opening an edit wizard — drop the
            // cached session so the next picker entry reconnects clean.
            if matches!(picker_state, ActivePicker::Sieve(_, _)) {
                app.drop_sieve_session();
            }
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
                ActivePicker::Sieve(_, slot) => {
                    if let Some(name) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.open_sieve_edit(name);
                    }
                }
                ActivePicker::Template(_, slot) => {
                    if let Some(name) = slot.lock().ok().and_then(|mut g| g.take()) {
                        app.open_template(name);
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
                ActivePicker::Sieve(p, _) => {
                    p.refresh();
                }
                ActivePicker::Template(p, _) => {
                    p.refresh();
                }
            }
            app.active_picker = Some(picker_state);
        }
    }
    Ok(())
}

/// Route key events when the folder CRUD action-choice overlay is open.
pub(super) async fn handle_folder_crud_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.folder_crud = None;
            app.status = "folder: cancelled".into();
        }
        KeyCode::Enter => {
            app.confirm_folder_crud();
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if let Some(crud) = app.folder_crud.as_mut() {
                let cur = crud.state.selected().unwrap_or(0);
                crud.state.select(Some(if cur == 0 { 2 } else { cur - 1 }));
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if let Some(crud) = app.folder_crud.as_mut() {
                let cur = crud.state.selected().unwrap_or(0);
                crud.state.select(Some((cur + 1) % 3));
            }
        }
        KeyCode::Char('c') if key.modifiers.is_empty() => {
            if let Some(crud) = app.folder_crud.as_mut() {
                crud.state.select(Some(0));
            }
            app.confirm_folder_crud();
        }
        KeyCode::Char('r') if key.modifiers.is_empty() => {
            if let Some(crud) = app.folder_crud.as_mut() {
                crud.state.select(Some(1));
            }
            app.confirm_folder_crud();
        }
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            if let Some(crud) = app.folder_crud.as_mut() {
                crud.state.select(Some(2));
            }
            app.confirm_folder_crud();
        }
        _ => {}
    }
    Ok(())
}

/// Route key events when a folder CRUD text-input prompt is open
/// (create name / rename new name / delete confirm).
pub(super) async fn handle_folder_crud_prompt_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.folder_crud_prompt = None;
            app.status = "folder: cancelled".into();
            return Ok(());
        }
        KeyCode::Enter => {
            // For Delete prompts, the enter key only fires after 'y' confirms.
            let ready = match &app.folder_crud_prompt {
                Some(FolderCrudPrompt::Delete(_, confirmed)) => *confirmed,
                Some(FolderCrudPrompt::Create(name)) => !name.is_empty(),
                Some(FolderCrudPrompt::Rename(_, new_name)) => !new_name.is_empty(),
                None => false,
            };
            if ready {
                app.apply_folder_crud_prompt();
            } else if matches!(
                &app.folder_crud_prompt,
                Some(FolderCrudPrompt::Delete(_, false))
            ) {
                app.status = "folder delete: press y to confirm, Esc to cancel".into();
            }
            return Ok(());
        }
        KeyCode::Char('y') if key.modifiers.is_empty() => {
            // Confirm delete.
            if let Some(FolderCrudPrompt::Delete(_, confirmed)) = app.folder_crud_prompt.as_mut() {
                *confirmed = true;
                app.apply_folder_crud_prompt();
                return Ok(());
            }
            // For create/rename: treat as regular character input.
            match app.folder_crud_prompt.as_mut() {
                Some(FolderCrudPrompt::Create(name)) => name.push('y'),
                Some(FolderCrudPrompt::Rename(_, new_name)) => new_name.push('y'),
                _ => {}
            }
        }
        KeyCode::Backspace => match app.folder_crud_prompt.as_mut() {
            Some(FolderCrudPrompt::Create(name)) => {
                name.pop();
            }
            Some(FolderCrudPrompt::Rename(_, new_name)) => {
                new_name.pop();
            }
            _ => {}
        },
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            match app.folder_crud_prompt.as_mut() {
                Some(FolderCrudPrompt::Create(name)) => {
                    name.push(c);
                }
                Some(FolderCrudPrompt::Rename(_, new_name)) => {
                    new_name.push(c);
                }
                Some(FolderCrudPrompt::Delete(_, _)) => {
                    // Only 'y' does something for delete — handled above.
                }
                None => {}
            }
        }
        _ => {}
    }
    // Update status to show current input.
    match &app.folder_crud_prompt {
        Some(FolderCrudPrompt::Create(name)) => {
            app.status = format!("folder create: {name}_ (Enter confirm · Esc cancel)");
        }
        Some(FolderCrudPrompt::Rename(from, new_name)) => {
            app.status =
                format!("folder rename '{from}' → {new_name}_ (Enter confirm · Esc cancel)");
        }
        Some(FolderCrudPrompt::Delete(name, _)) => {
            app.status = format!("folder delete '{name}': y to confirm · Esc cancel");
        }
        None => {}
    }
    Ok(())
}

/// Route key events when the Sieve-edit wizard is open.
pub(super) async fn handle_sieve_wizard_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Some(wizard) = app.active_sieve_wizard.as_mut() else {
        return Ok(());
    };

    if wizard.form.mode == hjkl_form::FormMode::Normal {
        if key.code == KeyCode::Esc {
            app.active_sieve_wizard = None;
            // Wizard dismissed without saving; drop the cached session.
            app.drop_sieve_session();
            app.status = "sieve: cancelled".into();
            return Ok(());
        }
        // <Space>s = save.
        if app.pending_leader == Some(super::app::LeaderState::Pending) {
            app.pending_leader = None;
            if key.code == KeyCode::Char('s') {
                app.save_sieve_wizard();
                return Ok(());
            }
        }
        if key.code == KeyCode::Char(' ') && key.modifiers.is_empty() {
            app.pending_leader = Some(super::app::LeaderState::Pending);
            return Ok(());
        }
    }

    let Some(wizard) = app.active_sieve_wizard.as_mut() else {
        return Ok(());
    };
    wizard.form.handle_input(crossterm_key_to_engine_input(key));

    // Update status with focused field name.
    let label = wizard.focused_label().to_string();
    app.status = format!("sieve: {label} — <Space>s save · Esc cancel");
    Ok(())
}
