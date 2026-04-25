pub mod imap;
pub mod smtp;

pub use imap::{
    Error as ImapError, FolderInfo, HeaderRow, ImapSession, append_message, connect_imap,
    fetch_inbox_headers, find_sent_folder, list_folders,
};
pub use smtp::{Error as SmtpError, send_message};
