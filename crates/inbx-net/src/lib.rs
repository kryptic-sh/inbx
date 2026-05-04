pub mod graph;
pub mod idle;
pub mod imap;
pub mod jmap;
pub mod mdn;
pub mod oauth;
pub mod provider;
pub mod proxy;
pub mod sieve;
pub mod smtp;
pub mod unsubscribe;

pub use graph::graph_id_to_uid;
pub use imap::{
    Error as ImapError, FolderInfo, HeaderRow, ImapSession, append_draft, append_message,
    connect_imap, create_folder, delete_folder, expunge_folder, fetch_bodies, fetch_headers,
    fetch_headers_uids, fetch_inbox_headers, find_drafts_folder, find_sent_folder, list_folders,
    rename_folder, search_since, store_flags, subscribe_folder, uid_copy, uid_move,
};
pub use mdn::{Disposition as MdnDisposition, MdnContext, build_mdn};
pub use oauth::{Error as OAuthError, TokenSet, login as oauth_login, refresh as oauth_refresh};
pub use provider::{Error as ProviderError, ImapProvider, MailProvider, connect_provider};
pub use smtp::{Error as SmtpError, send_message};
