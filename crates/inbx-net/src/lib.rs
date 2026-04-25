pub mod graph;
pub mod idle;
pub mod imap;
pub mod oauth;
pub mod sieve;
pub mod smtp;
pub mod unsubscribe;

pub use imap::{
    Error as ImapError, FolderInfo, HeaderRow, ImapSession, append_message, connect_imap,
    fetch_bodies, fetch_inbox_headers, find_sent_folder, list_folders,
};
pub use oauth::{Error as OAuthError, TokenSet, login as oauth_login, refresh as oauth_refresh};
pub use smtp::{Error as SmtpError, send_message};
