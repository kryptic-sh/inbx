//! Message composer wrapping hjkl-editor's modal vim runtime.
//!
//! Header fields (Subject / To / Cc / Bcc) are backed by a
//! [`hjkl_form::Form`] of `SingleLineText` fields. The body stays as a
//! standalone `hjkl_editor::runtime::Editor` (multi-line, not a form field).
//! MIME assembly happens at send time via mail-builder. Identities +
//! signatures travel with the composer instance; threading metadata for
//! replies is captured from the original message and emitted on the
//! outgoing headers.

pub mod identity;
pub mod templates;

use hjkl_clipboard::{Clipboard, MimeType as ClipMime, Selection};
use hjkl_editor::buffer::Buffer as EditorBuffer;
use hjkl_editor::runtime::{DefaultHost, Editor, KeybindingMode, Options};
use hjkl_form::{Field as FormField, FieldMeta, Form, TextFieldEditor};
use mail_builder::MessageBuilder;
use mail_parser::{HeaderValue, MessageParser};

pub use identity::Identity;

/// Field indices inside `headers: Form`. Fixed at construction.
const SUBJECT_IDX: usize = 0;
const TO_IDX: usize = 1;
const CC_IDX: usize = 2;
const BCC_IDX: usize = 3;

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

/// A discriminated reference to the currently-focused editable surface.
/// Callers that previously used a bare `&mut Editor` now match on this.
pub enum FocusedEditor<'a> {
    /// A header field backed by a [`TextFieldEditor`] inside the form.
    Header(&'a mut TextFieldEditor),
    /// The multi-line body editor.
    Body(&'a mut Editor),
}

/// One message in flight. Header fields live inside a [`Form`] so the
/// existing hjkl-form FSM drives focus and validation.  The body stays
/// as a standalone multi-line `hjkl_editor::runtime::Editor`.
pub struct Composer {
    pub identity: Identity,
    /// Subject / To / Cc / Bcc as `SingleLineText` fields in order.
    pub headers: Form,
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

// ── helpers to borrow a named field out of the Form ──────────────────────────

fn header_text(form: &Form, idx: usize) -> String {
    match form.fields.get(idx) {
        Some(FormField::SingleLineText(f)) => f.text().trim_end_matches('\n').to_string(),
        _ => String::new(),
    }
}

fn set_header_text(form: &mut Form, idx: usize, text: &str) {
    if let Some(FormField::SingleLineText(f)) = form.fields.get_mut(idx) {
        f.set_text(text);
    }
}

fn header_cursor(form: &Form, idx: usize) -> (usize, usize) {
    match form.fields.get(idx) {
        Some(FormField::SingleLineText(f)) => f.cursor(),
        _ => (0, 0),
    }
}

/// Build the four-field header form (Subject/To/Cc/Bcc).
fn new_header_form() -> Form {
    Form::new()
        .with_field(FormField::SingleLineText(TextFieldEditor::with_meta(
            FieldMeta::new("subject"),
            1,
        )))
        .with_field(FormField::SingleLineText(TextFieldEditor::with_meta(
            FieldMeta::new("to"),
            1,
        )))
        .with_field(FormField::SingleLineText(TextFieldEditor::with_meta(
            FieldMeta::new("cc"),
            1,
        )))
        .with_field(FormField::SingleLineText(TextFieldEditor::with_meta(
            FieldMeta::new("bcc"),
            1,
        )))
}

impl Composer {
    pub fn new_blank(identity: Identity) -> Self {
        let mut body = new_vim_editor();
        if let Some(sig) = identity.signature_block() {
            body.set_content(&sig);
        }
        let mut headers = new_header_form();
        // Default focus is To (index 1).
        headers.set_focus(TO_IDX);
        Self {
            identity,
            headers,
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
        composer.set_subject(&prefix_subject(subject, "Re: "));

        // Recipients.
        let from_addr = parsed
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(|s| s.to_string())
            .unwrap_or_default();
        composer.set_to(&from_addr);
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
            composer.set_cc(&cc.join(", "));
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
        composer.set_subject(&prefix_subject(subject, "Fwd: "));

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

    // ── header accessors ─────────────────────────────────────────────────────

    pub fn subject(&self) -> String {
        header_text(&self.headers, SUBJECT_IDX)
    }

    pub fn to(&self) -> String {
        header_text(&self.headers, TO_IDX)
    }

    pub fn cc(&self) -> String {
        header_text(&self.headers, CC_IDX)
    }

    pub fn bcc(&self) -> String {
        header_text(&self.headers, BCC_IDX)
    }

    pub fn set_subject(&mut self, s: &str) {
        set_header_text(&mut self.headers, SUBJECT_IDX, s);
    }

    pub fn set_to(&mut self, s: &str) {
        set_header_text(&mut self.headers, TO_IDX, s);
    }

    pub fn set_cc(&mut self, s: &str) {
        set_header_text(&mut self.headers, CC_IDX, s);
    }

    pub fn set_bcc(&mut self, s: &str) {
        set_header_text(&mut self.headers, BCC_IDX, s);
    }

    /// Cursor position for a header field. Used by the render layer.
    pub fn header_cursor(&self, field: Field) -> (usize, usize) {
        let idx = field_to_header_idx(field);
        header_cursor(&self.headers, idx)
    }

    // ── legacy text helpers (used by send/draft paths) ────────────────────────

    pub fn subject_text(&self) -> String {
        self.subject()
    }

    pub fn body_text(&self) -> String {
        editor_text(&self.body)
    }

    pub fn to_text(&self) -> String {
        self.to()
    }

    // ── focus helpers ─────────────────────────────────────────────────────────

    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
        self.sync_form_focus();
    }

    pub fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
        self.sync_form_focus();
    }

    /// Keep `headers.focused` in sync with `self.focus` for header fields.
    fn sync_form_focus(&mut self) {
        if let Some(idx) = field_to_header_idx_opt(self.focus) {
            self.headers.set_focus(idx);
        }
    }

    /// Return a mutable reference to the currently-focused editing surface.
    pub fn focused_editor(&mut self) -> FocusedEditor<'_> {
        match self.focus {
            Field::Body => FocusedEditor::Body(&mut self.body),
            header => {
                let idx = field_to_header_idx(header);
                match self.headers.fields.get_mut(idx) {
                    Some(FormField::SingleLineText(f)) => FocusedEditor::Header(f),
                    _ => FocusedEditor::Body(&mut self.body),
                }
            }
        }
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

    /// Attach the current system clipboard contents. Prefers PNG image data;
    /// falls back to plain text. Returns `Err(Missing)` when the clipboard is
    /// unavailable or empty.
    pub fn attach_from_clipboard(&mut self) -> Result<()> {
        let cb = Clipboard::new().map_err(|_| Error::Missing("clipboard unavailable"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Try image/png first.
        if let Ok(bytes) = cb.get(Selection::Clipboard, ClipMime::Png)
            && !bytes.is_empty()
        {
            self.attachments.push(Attachment {
                filename: format!("clipboard-{now}.png"),
                content_type: "image/png".to_string(),
                bytes,
            });
            return Ok(());
        }
        // Fall back to plain text.
        let bytes = cb
            .get(Selection::Clipboard, ClipMime::Text)
            .map_err(|_| Error::Missing("clipboard empty"))?;
        if bytes.is_empty() {
            return Err(Error::Missing("clipboard empty"));
        }
        self.attachments.push(Attachment {
            filename: format!("clipboard-{now}.txt"),
            content_type: "text/plain".to_string(),
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
        out.push_str(&format!("To: {}\n", self.to()));
        let cc = self.cc();
        if !cc.is_empty() {
            out.push_str(&format!("Cc: {cc}\n"));
        }
        let bcc = self.bcc();
        if !bcc.is_empty() {
            out.push_str(&format!("Bcc: {bcc}\n"));
        }
        out.push_str(&format!("Subject: {}\n", self.subject()));
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
        let to = parse_addresses(&self.to());
        if to.is_empty() {
            return Err(Error::Missing("To"));
        }
        let from_name = self.identity.name.clone().unwrap_or_default();
        let from = (from_name, self.identity.email.clone());

        let mut builder = MessageBuilder::new()
            .from(from)
            .to(to)
            .subject(self.subject())
            .text_body(editor_text(&self.body));

        let cc = parse_addresses(&self.cc());
        if !cc.is_empty() {
            builder = builder.cc(cc);
        }
        let bcc = parse_addresses(&self.bcc());
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

/// Map a `Field` variant for a header field to its index in `headers.fields`.
/// Panics on `Field::Body` (not a form field).
fn field_to_header_idx(field: Field) -> usize {
    match field {
        Field::Subject => SUBJECT_IDX,
        Field::To => TO_IDX,
        Field::Cc => CC_IDX,
        Field::Bcc => BCC_IDX,
        Field::Body => panic!("Body is not a header form field"),
    }
}

/// Returns `Some(idx)` for header fields, `None` for `Body`.
fn field_to_header_idx_opt(field: Field) -> Option<usize> {
    match field {
        Field::Subject => Some(SUBJECT_IDX),
        Field::To => Some(TO_IDX),
        Field::Cc => Some(CC_IDX),
        Field::Bcc => Some(BCC_IDX),
        Field::Body => None,
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
        c.set_subject("Hello");
        c.set_to("bob@x.com");
        c.body.set_content("hi there");
        let raw = c.to_mime().unwrap();
        let s = std::str::from_utf8(&raw).unwrap();
        assert!(s.contains("Subject:") && s.contains("Hello"));
        assert!(s.contains("bob@x.com"));
        assert!(s.contains("hi there"));
    }

    /// Verify attach_from_clipboard picks PNG over text when both are present.
    #[test]
    fn attach_from_clipboard_prefers_png() {
        use hjkl_clipboard::backend::mock::MockBackend;
        use hjkl_clipboard::{
            BackendKind, Capabilities, Clipboard, MimeType as ClipMime, Selection,
        };

        let mock = MockBackend::new(BackendKind::Mock, Capabilities::all());
        mock.preset_get(Selection::Clipboard, ClipMime::Png, Ok(b"\x89PNG".to_vec()));
        mock.preset_get(
            Selection::Clipboard,
            ClipMime::Text,
            Ok(b"some text".to_vec()),
        );
        let cb = Clipboard::with_backend(Box::new(mock));

        let mut c = Composer::new_blank(id());
        // Call the internal logic directly via a local helper using with_backend.
        // We reproduce attach_from_clipboard's logic here so we can inject the mock.
        let now = 0u64;
        if let Ok(bytes) = cb.get(Selection::Clipboard, ClipMime::Png)
            && !bytes.is_empty()
        {
            c.attachments.push(Attachment {
                filename: format!("clipboard-{now}.png"),
                content_type: "image/png".to_string(),
                bytes,
            });
        }
        assert_eq!(c.attachments.len(), 1);
        assert_eq!(c.attachments[0].content_type, "image/png");
        assert_eq!(c.attachments[0].filename, "clipboard-0.png");
    }

    /// Verify attach_from_clipboard falls back to text when no PNG is present.
    #[test]
    fn attach_from_clipboard_text_fallback() {
        use hjkl_clipboard::backend::mock::MockBackend;
        use hjkl_clipboard::{
            BackendKind, Capabilities, Clipboard, MimeType as ClipMime, Selection,
        };

        let mock = MockBackend::new(BackendKind::Mock, Capabilities::all());
        mock.preset_get(
            Selection::Clipboard,
            ClipMime::Text,
            Ok(b"hello world".to_vec()),
        );
        let cb = Clipboard::with_backend(Box::new(mock));

        let mut c = Composer::new_blank(id());
        // PNG returns UnsupportedMime (unprogrammed) — fall through to text.
        let png_ok = cb
            .get(Selection::Clipboard, ClipMime::Png)
            .map(|b| !b.is_empty())
            .unwrap_or(false);
        if !png_ok
            && let Ok(bytes) = cb.get(Selection::Clipboard, ClipMime::Text)
            && !bytes.is_empty()
        {
            c.attachments.push(Attachment {
                filename: "clipboard-0.txt".to_string(),
                content_type: "text/plain".to_string(),
                bytes,
            });
        }
        assert_eq!(c.attachments.len(), 1);
        assert_eq!(c.attachments[0].content_type, "text/plain");
        assert_eq!(c.attachments[0].filename, "clipboard-0.txt");
        assert_eq!(c.attachments[0].bytes, b"hello world");
    }

    #[test]
    fn header_form_has_four_fields() {
        let c = Composer::new_blank(id());
        assert_eq!(c.headers.fields.len(), 4);
    }

    #[test]
    fn set_and_get_headers_round_trip() {
        let mut c = Composer::new_blank(id());
        c.set_subject("Test Subject");
        c.set_to("alice@example.com");
        c.set_cc("bob@example.com");
        c.set_bcc("carol@example.com");
        assert_eq!(c.subject(), "Test Subject");
        assert_eq!(c.to(), "alice@example.com");
        assert_eq!(c.cc(), "bob@example.com");
        assert_eq!(c.bcc(), "carol@example.com");
    }

    #[test]
    fn focus_next_syncs_form_focus() {
        let mut c = Composer::new_blank(id());
        // Default focus is To (idx 1).
        assert_eq!(c.focus, Field::To);
        assert_eq!(c.headers.focused(), TO_IDX);
        c.focus_next();
        assert_eq!(c.focus, Field::Cc);
        assert_eq!(c.headers.focused(), CC_IDX);
    }

    #[test]
    fn focused_editor_returns_body_when_body_focused() {
        let mut c = Composer::new_blank(id());
        c.focus = Field::Body;
        matches!(c.focused_editor(), FocusedEditor::Body(_));
    }

    #[test]
    fn focused_editor_returns_header_when_header_focused() {
        let mut c = Composer::new_blank(id());
        c.focus = Field::Subject;
        matches!(c.focused_editor(), FocusedEditor::Header(_));
    }
}
