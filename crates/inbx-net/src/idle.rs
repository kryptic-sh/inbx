//! IMAP IDLE wait loop (RFC 2177).
//!
//! `wait_for_new` opens a session, selects INBOX, issues IDLE, and returns
//! when the server signals new data or the keepalive window elapses. The
//! caller drives the outer loop, so refetching + notifying stays in
//! one place.

use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use inbx_config::Account;

use crate::imap;

/// RFC 2177 recommends re-issuing IDLE every 29 min. We use 25 to leave
/// margin for slow networks; the server's keepalives reset our timer.
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

#[derive(Debug, Clone, Copy)]
pub enum IdleEvent {
    /// EXISTS / EXPUNGE / FLAGS update arrived from the server.
    NewData,
    /// Hit the keepalive window without server data; caller should
    /// re-issue IDLE.
    Timeout,
}

pub async fn wait_for_new(account: &Account) -> Result<IdleEvent, imap::Error> {
    wait_for_new_in(account, "INBOX").await
}

/// Same as `wait_for_new` but watches an explicit folder.
pub async fn wait_for_new_in(account: &Account, folder: &str) -> Result<IdleEvent, imap::Error> {
    let mut session = imap::connect_imap(account).await?;
    session.select(folder).await?;
    let mut handle = session.idle();
    handle.init().await?;
    let (fut, _stop) = handle.wait_with_timeout(IDLE_TIMEOUT);
    let outcome = fut.await;
    let _ = handle.done().await;
    Ok(match outcome {
        Ok(IdleResponse::NewData(_)) => IdleEvent::NewData,
        Ok(IdleResponse::Timeout) => IdleEvent::Timeout,
        Ok(IdleResponse::ManualInterrupt) => IdleEvent::Timeout,
        Err(e) => return Err(imap::Error::Imap(e)),
    })
}
