use std::io::{Stdout, stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use inbx_config::Account;
use inbx_config::theme::Theme;
use inbx_store::Store;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod app;
mod binds;
mod keys;
mod picker;
mod render;
mod tasks;
mod wizard;

use app::App;

/// Process-wide theme handle. `App::new` sets it before the event loop
/// starts; `render::pane_block` reads it. We keep a OnceLock instead of
/// threading `&Theme` through every draw function — there's only ever
/// one theme per process.
pub(crate) static ACTIVE_THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

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
        term.draw(|f| render::draw(f, app))?;

        // Select between: key events, background task results, and the 120 ms
        // spinner tick (only active while busy). This ensures redraws happen
        // even while a long op is in flight.
        enum Selected {
            Key(crossterm::event::KeyEvent),
            TaskResult(tasks::TaskResult),
            Tick,
            StreamEnd,
        }

        let selected = tokio::select! {
            ev = events.next() => {
                match ev {
                    None => Selected::StreamEnd,
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Event::Key(key))) => Selected::Key(key),
                    Some(Ok(_)) => continue,
                }
            }
            Some(result) = app.task_rx.0.recv() => {
                Selected::TaskResult(result)
            }
            _ = tokio::time::sleep(Duration::from_millis(120)), if app.busy => {
                Selected::Tick
            }
        };

        match selected {
            Selected::StreamEnd => break,
            Selected::Tick => {
                // Just redraw for spinner tick — no action needed.
            }
            Selected::TaskResult(result) => {
                handle_task_result(app, result).await?;
            }
            Selected::Key(key) => {
                if app.active_picker.is_some() {
                    keys::handle_active_picker_key(app, key).await?;
                    continue;
                }
                if app.composer.is_some() {
                    if keys::handle_composer_key(app, key).await? {
                        break;
                    }
                    continue;
                }
                if app.outbox.is_some() {
                    keys::handle_outbox_key(app, key).await?;
                    continue;
                }
                if app.search.is_some() {
                    keys::handle_search_key(app, key).await?;
                    continue;
                }
                if app.thread.is_some() {
                    keys::handle_thread_key(app, key).await?;
                    continue;
                }
                if app.move_picker.is_some() {
                    keys::handle_move_picker_key(app, key).await?;
                    continue;
                }
                if app.account_picker.is_some() {
                    keys::handle_account_picker_key(app, key).await?;
                    continue;
                }
                if app.contacts.is_some() {
                    keys::handle_contacts_key(app, key).await?;
                    continue;
                }
                if app.ical.is_some() {
                    keys::handle_ical_key(app, key).await?;
                    continue;
                }
                if app.active_wizard.is_some() {
                    keys::handle_wizard_key(app, key).await?;
                    continue;
                }
                if app.active_sieve_wizard.is_some() {
                    keys::handle_sieve_wizard_key(app, key).await?;
                    continue;
                }
                if keys::handle_list_key(app, key).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Dispatch a `TaskResult` that arrived from a background tokio task.
/// Updates App state and clears the pending-op counter.
async fn handle_task_result(app: &mut App, result: tasks::TaskResult) -> Result<()> {
    use tasks::TaskResult;
    match result {
        TaskResult::SyncDone {
            last_sync_unix,
            error,
            new_messages,
            folder_name,
            total_messages,
        } => {
            app.complete_pending();
            if let Some(ts) = last_sync_unix {
                app.last_sync_unix = Some(ts);
            }
            app.reload_messages().await?;
            app.status = match error {
                Some(e) => format!("sync error: {e}"),
                None => format!("synced {folder_name} ({total_messages} msgs, {new_messages} new)"),
            };
        }
        TaskResult::BodyFetched { uid, error } => {
            app.complete_pending();
            match error {
                Some(e) => {
                    app.status = format!("fetch body error: {e}");
                }
                None => {
                    app.reload_messages().await?;
                    app.refresh_body();
                    app.status = format!("fetched body uid {uid}");
                }
            }
        }
        TaskResult::OutboxDrained { sent, failed } => {
            app.complete_pending();
            app.reload_outbox_pub().await?;
            app.status = format!("outbox drain: {sent} sent, {failed} failed");
        }
        TaskResult::SieveScripts(Ok(scripts)) => {
            app.complete_pending();
            if scripts.is_empty() {
                app.status = "sieve: no scripts found".into();
            } else {
                let (p, slot) = picker::sieve_picker(scripts);
                app.active_picker = Some(app::ActivePicker::Sieve(p, slot));
                app.status = "<Space>S sieve: Enter edit · Esc cancel".into();
            }
        }
        TaskResult::SieveScripts(Err(e)) => {
            app.complete_pending();
            // Cache already cleared inside do_sieve_list on error; belt-and-suspenders.
            app.drop_sieve_session();
            app.status = format!("sieve list failed: {e}");
        }
        TaskResult::SieveBody {
            name,
            body: Ok(body),
        } => {
            app.complete_pending();
            app.active_sieve_wizard = Some(wizard::SieveEditWizard::new(name.clone(), body));
            app.status = format!("sieve: editing '{name}' — <Space>s save · Esc cancel");
        }
        TaskResult::SieveBody {
            name: _,
            body: Err(e),
        } => {
            app.complete_pending();
            app.status = format!("sieve get failed: {e}");
        }
        TaskResult::SieveSaved {
            name,
            body: _,
            result: Ok(()),
        } => {
            app.complete_pending();
            // Sieve flow complete; drop the cached session — no more ops expected
            // until the user re-enters the picker.
            app.drop_sieve_session();
            app.status = format!("sieve: saved {name}");
        }
        TaskResult::SieveSaved {
            name,
            body,
            result: Err(e),
        } => {
            app.complete_pending();
            // Restore the wizard so the user can retry.
            app.active_sieve_wizard = Some(wizard::SieveEditWizard::new(name, body));
            app.status = format!("sieve save failed: {e}");
        }
        TaskResult::WatchSignal => {
            // Background watch (IMAP IDLE or JMAP EventSource) fired.
            // Kick off a manual sync on the current folder, same as `F`.
            app.manual_sync();
        }
    }
    Ok(())
}
