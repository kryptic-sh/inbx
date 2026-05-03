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
use tokio::time::timeout;

mod app;
mod keys;
mod picker;
mod render;
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

        // While an async operation is in flight, time-out at 120 ms so the
        // spinner frame advances smoothly. Outside of busy state, block until
        // the next event with no artificial wake-ups.
        let maybe_ev = if app.busy {
            match timeout(Duration::from_millis(120), events.next()).await {
                Ok(inner) => inner,
                Err(_) => continue, // timeout — redraw for spinner tick
            }
        } else {
            events.next().await
        };

        let Some(ev) = maybe_ev else {
            break;
        };
        let ev = ev?;
        if let Event::Key(key) = ev {
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
            if keys::handle_list_key(app, key).await? {
                break;
            }
        }
    }
    Ok(())
}
