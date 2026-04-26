//! Message composer wrapping hjkl-editor's modal vim runtime.
//!
//! Separate Editor instances back the body and each single-line header
//! so the user gets full vim motions everywhere. MIME assembly happens
//! at send time via mail-builder. Identities + signatures travel with
//! the composer instance; threading metadata for replies is captured
//! from the original message and emitted on the outgoing headers.

pub mod identity;
pub mod templates;

use hjkl_editor::buffer::Buffer as EditorBuffer;
use hjkl_editor::runtime::{DefaultHost, Editor, KeybindingMode, Options};
use mail_builder::MessageBuilder;
use mail_parser::{HeaderValue, MessageParser};

pub use identity::Identity;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: could not parse RFC 5322 input")]
    Parse,
    #[error("missing field: {0}")]
    Missing(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Subject,
    To,
    Cc,
    Bcc,
    Body,
}

impl Field {
    pub const ALL: [Self; 5] = [Self::Subject, Self::To, Self::Cc, Self::Bcc, Self::Body];

    pub fn next(self) -> Self {
        match self {
            Self::Subject => Self::To,
            Self::To => Self::Cc,
            Self::Cc => Self::Bcc,
            Self::Bcc => Self::Body,
            Self::Body => Self::Subject,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Subject => Self::Body,
            Self::To => Self::Subject,
            Self::Cc => Self::To,
            Self::Bcc => Self::Cc,
            Self::Body => Self::Bcc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::To => "to",
            Self::Cc => "cc",
            Self::Bcc => "bcc",
            Self::Body => "body",
        }
    }
}

/// One message in flight. Header fields use a single-line vim editor;
/// the body uses a multi-line editor with the identity's signature
/// pre-populated below the cursor.
pub struct Composer {
    pub identity: Identity,
    pub subject: Editor,
    pub to: Editor,
    pub cc: Editor,
    pub bcc: Editor,
    pub body: Editor,
    pub focus: Field,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl Composer {
    pub fn new_blank(identity: Identity) -> Self {
        let mut body = new_vim_editor();
        if let Some(sig) = identity.signature_block() {
            body.set_content(&sig);
        }
        Self {
            identity,
            subject: new_vim_editor(),
            to: new_vim_editor(),
            cc: new_vim_editor(),
            bcc: new_vim_editor(),
            body,
            focus: Field::To,
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
        }
    }

    pub fn new_reply(identity: Identity, raw: &[u8], reply_all: bool) -> Result<Self> {
        let parsed = MessageParser::default().parse(raw).ok_or(Error::Parse)?;
        let mut composer = Self::new_blank(identity.clone());

        // Threading.
        composer.in_reply_to = parsed.message_id().map(|s| s.to_string());
        let mut refs: Vec<String> = parsed
            .references()
            .as_text_list()
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        if let Some(mid) = composer.in_reply_to.as_deref() {
            refs.push(mid.to_string());
        }
        composer.references = refs;

        // Subject.
        let subject = parsed.subject().unwrap_or_default();
        composer
            .subject
            .set_content(&prefix_subject(subject, "Re: "));

        // Recipients.
        let from_addr = parsed
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(|s| s.to_string())
            .unwrap_or_default();
        composer.to.set_content(&from_addr);
        if reply_all {
            let mut cc: Vec<String> = Vec::new();
            for group in [parsed.to(), parsed.cc()].into_iter().flatten() {
                for addr in group.iter() {
                    if let Some(s) = addr.address()
                        && s != identity.email
                        && s != from_addr
                        && !cc.iter().any(|c| c == s)
                    {
                        cc.push(s.to_string());
                    }
                }
            }
            composer.cc.set_content(&cc.join(", "));
        }

        // Quoted body.
        let original_body = parsed
            .body_text(0)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let attribution = format_attribution(&parsed);
        let mut quoted = String::new();
        if !attribution.is_empty() {
            quoted.push_str(&attribution);
            quoted.push_str("\n\n");
        }
        for line in original_body.lines() {
            quoted.push_str("> ");
            quoted.push_str(line);
            quoted.push('\n');
        }
        if let Some(sig) = composer.identity.signature_block() {
            quoted.push('\n');
            quoted.push_str(&sig);
        }
        composer.body.set_content(&quoted);
        composer.focus = Field::Body;
        Ok(composer)
    }

    pub fn new_forward(identity: Identity, raw: &[u8]) -> Result<Self> {
        let parsed = MessageParser::default().parse(raw).ok_or(Error::Parse)?;
        let mut composer = Self::new_blank(identity);

        let subject = parsed.subject().unwrap_or_default();
        composer
            .subject
            .set_content(&prefix_subject(subject, "Fwd: "));

        let from_addr = parsed
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .unwrap_or("");
        let date = parsed.date().map(|d| d.to_rfc3339()).unwrap_or_default();
        let original_body = parsed
            .body_text(0)
            .map(|s| s.to_string())
            .unwrap_or_default();

        let mut buf = String::new();
        if let Some(sig) = composer.identity.signature_block() {
            buf.push_str(&sig);
            buf.push_str("\n\n");
        }
        buf.push_str("---------- Forwarded message ----------\n");
        buf.push_str(&format!("From: {from_addr}\n"));
        if !date.is_empty() {
            buf.push_str(&format!("Date: {date}\n"));
        }
        buf.push_str(&format!("Subject: {subject}\n\n"));
        buf.push_str(&original_body);
        composer.body.set_content(&buf);
        composer.focus = Field::To;
        Ok(composer)
    }

    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    pub fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
    }

    pub fn editor_for(&mut self, field: Field) -> &mut Editor {
        match field {
            Field::Subject => &mut self.subject,
            Field::To => &mut self.to,
            Field::Cc => &mut self.cc,
            Field::Bcc => &mut self.bcc,
            Field::Body => &mut self.body,
        }
    }

    pub fn focused_editor(&mut self) -> &mut Editor {
        self.editor_for(self.focus)
    }

    pub fn subject_text(&self) -> String {
        editor_text(&self.subject)
    }

    pub fn body_text(&self) -> String {
        editor_text(&self.body)
    }

    pub fn to_text(&self) -> String {
        editor_text(&self.to)
    }

    /// Attach a file from disk. Content-type sniffed from extension.
    pub fn attach_path(&mut self, path: &std::path::Path) -> Result<()> {
        let bytes = std::fs::read(path).map_err(|_| Error::Missing("read attachment"))?;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment")
            .to_string();
        let content_type = guess_content_type(&filename);
        self.attachments.push(Attachment {
            filename,
            content_type,
            bytes,
        });
        Ok(())
    }

    /// Emit a lenient draft scaffold suitable for user editing.
    /// Unlike [`Composer::to_mime`], empty To is allowed and the output is
    /// plain RFC 5322-shaped text rather than fully canonical MIME.
    pub fn to_draft(&self) -> String {
        let mut out = String::new();
        let from = match self.identity.name.as_deref() {
            Some(n) if !n.is_empty() => format!("{n} <{}>", self.identity.email),
            _ => self.identity.email.clone(),
        };
        out.push_str(&format!("From: {from}\n"));
        out.push_str(&format!("To: {}\n", editor_text(&self.to)));
        let cc = editor_text(&self.cc);
        if !cc.is_empty() {
            out.push_str(&format!("Cc: {cc}\n"));
        }
        let bcc = editor_text(&self.bcc);
        if !bcc.is_empty() {
            out.push_str(&format!("Bcc: {bcc}\n"));
        }
        out.push_str(&format!("Subject: {}\n", editor_text(&self.subject)));
        if let Some(mid) = &self.in_reply_to {
            out.push_str(&format!("In-Reply-To: <{mid}>\n"));
        }
        if !self.references.is_empty() {
            let refs = self
                .references
                .iter()
                .map(|s| format!("<{s}>"))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("References: {refs}\n"));
        }
        out.push('\n');
        out.push_str(&editor_text(&self.body));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Assemble an RFC 5322 wire form via mail-builder. Returns the raw
    /// bytes the SMTP/Graph/JMAP send paths can dispatch as-is.
    pub fn to_mime(&self) -> Result<Vec<u8>> {
        let to = parse_addresses(&editor_text(&self.to));
        if to.is_empty() {
            return Err(Error::Missing("To"));
        }
        let from_name = self.identity.name.clone().unwrap_or_default();
        let from = (from_name, self.identity.email.clone());

        let mut builder = MessageBuilder::new()
            .from(from)
            .to(to)
            .subject(editor_text(&self.subject))
            .text_body(editor_text(&self.body));

        let cc = parse_addresses(&editor_text(&self.cc));
        if !cc.is_empty() {
            builder = builder.cc(cc);
        }
        let bcc = parse_addresses(&editor_text(&self.bcc));
        if !bcc.is_empty() {
            builder = builder.bcc(bcc);
        }
        if let Some(mid) = self.in_reply_to.as_deref() {
            builder = builder.in_reply_to(mid.to_string());
        }
        if !self.references.is_empty() {
            builder = builder.references(
                self.references
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            );
        }
        for a in &self.attachments {
            builder =
                builder.attachment(a.content_type.clone(), a.filename.clone(), a.bytes.clone());
        }
        let bytes = builder
            .write_to_vec()
            .map_err(|_| Error::Missing("write"))?;
        Ok(bytes)
    }
}

fn guess_content_type(filename: &str) -> String {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "ics" => "text/calendar",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn editor_text(ed: &Editor) -> String {
    let s = ed.content();
    s.trim_end_matches('\n').to_string()
}

/// Construct an `Editor` configured with the vim keybinding mode and the
/// pre-0.1.0 `shiftwidth = 2` default the composer relies on.
fn new_vim_editor() -> Editor {
    let opts = Options {
        shiftwidth: 2,
        ..Options::default()
    };
    let mut ed = Editor::new(EditorBuffer::new(), DefaultHost::new(), opts);
    ed.keybinding_mode = KeybindingMode::Vim;
    ed
}

fn parse_addresses(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .map(|piece| piece.trim())
        .filter(|piece| !piece.is_empty())
        .map(|piece| {
            // Accept "Name <addr>" or bare addr.
            if let Some(open) = piece.rfind('<')
                && let Some(close) = piece.rfind('>')
                && close > open
            {
                let name = piece[..open].trim().trim_matches('"').to_string();
                let addr = piece[open + 1..close].trim().to_string();
                return (name, addr);
            }
            (String::new(), piece.to_string())
        })
        .collect()
}

fn prefix_subject(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    let upper = trimmed.to_ascii_uppercase();
    let prefix_upper = prefix.trim().to_ascii_uppercase();
    if upper.starts_with(&prefix_upper) {
        trimmed.to_string()
    } else {
        format!("{prefix}{trimmed}")
    }
}

fn format_attribution(parsed: &mail_parser::Message<'_>) -> String {
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .unwrap_or("");
    if from.is_empty() {
        return String::new();
    }
    let date = parsed
        .header_values("Date")
        .next()
        .and_then(|v| match v {
            HeaderValue::Text(t) => Some(t.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    if date.is_empty() {
        format!("On <unknown date>, {from} wrote:")
    } else {
        format!("On {date}, {from} wrote:")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity {
            name: Some("Alice".into()),
            email: "alice@example.com".into(),
            signature: Some("--\nAlice".into()),
        }
    }

    #[test]
    fn blank_has_signature() {
        let c = Composer::new_blank(id());
        assert!(c.body.content().contains("--"));
    }

    #[test]
    fn reply_threads_message_id() {
        let raw = b"Message-Id: <abc@x>\r\n\
                    From: bob@x.com\r\n\
                    To: alice@example.com\r\n\
                    Subject: Hello\r\n\
                    Date: Mon, 01 Jan 2026 12:00:00 +0000\r\n\
                    \r\n\
                    body line\r\n";
        let c = Composer::new_reply(id(), raw, false).unwrap();
        assert_eq!(c.in_reply_to.as_deref(), Some("abc@x"));
        assert!(c.subject_text().starts_with("Re: Hello"));
        assert!(c.body_text().contains("> body line"));
        assert_eq!(c.to_text(), "bob@x.com");
    }

    #[test]
    fn reply_avoids_double_re() {
        let raw = b"From: bob@x.com\r\nSubject: Re: Hello\r\n\r\nx\r\n";
        let c = Composer::new_reply(id(), raw, false).unwrap();
        assert_eq!(c.subject_text(), "Re: Hello");
    }

    #[test]
    fn forward_prefixes() {
        let raw = b"From: bob@x.com\r\nSubject: Hi\r\n\r\nbody\r\n";
        let c = Composer::new_forward(id(), raw).unwrap();
        assert!(c.subject_text().starts_with("Fwd: Hi"));
        assert!(c.body_text().contains("Forwarded message"));
    }

    #[test]
    fn to_mime_round_trip() {
        let mut c = Composer::new_blank(id());
        c.subject.set_content("Hello");
        c.to.set_content("bob@x.com");
        c.body.set_content("hi there");
        let raw = c.to_mime().unwrap();
        let s = std::str::from_utf8(&raw).unwrap();
        assert!(s.contains("Subject:") && s.contains("Hello"));
        assert!(s.contains("bob@x.com"));
        assert!(s.contains("hi there"));
    }
}
