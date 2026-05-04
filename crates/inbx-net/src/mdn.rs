//! MDN (Message Disposition Notification) composer — RFC 8098 §3.
//!
//! Builds a `multipart/report; report-type=disposition-notification` message
//! ready for SMTP send.  **Never called automatically** — callers must obtain
//! explicit user consent before invoking [`build_mdn`].

use mail_builder::MessageBuilder;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::{BodyPart, MimePart};

/// All inputs needed to compose a well-formed MDN.
#[derive(Debug, Clone)]
pub struct MdnContext {
    /// `"Alice <alice@example.com>"` — the user sending the MDN (MDN From:).
    pub from: String,
    /// Where to send the MDN (taken from `Disposition-Notification-To:` of the
    /// original message).
    pub to: Vec<String>,
    /// The `Message-ID` of the message being acknowledged.
    pub original_message_id: String,
    /// Original sender's email address.  Becomes `Original-Recipient` in the
    /// machine-readable part (optional per RFC 8098 §3.2.4).
    pub original_recipient: Option<String>,
    /// Subject of the original message; MDN subject becomes `"Read: <orig>"`.
    pub original_subject: String,
    /// Disposition action.  We only support the `displayed` action here.
    pub disposition: Disposition,
    /// Hostname for the `Reporting-UA` field.  Pass `gethostname()` output.
    pub reporting_ua: String,
}

/// RFC 8098 disposition action + sending mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// RFC 8098: `manual-action/MDN-sent-manually; displayed`
    DisplayedManualAction,
}

impl Disposition {
    /// Serialise to the RFC 8098 §3.2.6 wire value.
    fn wire_value(self) -> &'static str {
        match self {
            Disposition::DisplayedManualAction => "manual-action/MDN-sent-manually; displayed",
        }
    }
}

/// Extract a bare email address from a `"Display Name <addr@host>"` or plain
/// `"addr@host"` string.
fn bare_address(s: &str) -> &str {
    if let (Some(lt), Some(gt)) = (s.rfind('<'), s.rfind('>'))
        && lt < gt
    {
        return s[lt + 1..gt].trim();
    }
    s.trim()
}

/// Build a `multipart/report; report-type=disposition-notification` MDN
/// per RFC 8098 §3.  Returns the complete RFC 5322 bytes ready for SMTP send.
///
/// # Errors
/// Propagates any I/O error from `mail-builder`'s write path.
pub fn build_mdn(ctx: &MdnContext) -> std::io::Result<Vec<u8>> {
    // --- Part 1: human-readable text/plain summary ---
    let human_text = format!(
        "The message with subject \"{}\" has been read.\r\n\r\n\
         This is an automatically generated Message Disposition Notification.\r\n",
        ctx.original_subject
    );

    // --- Part 2: machine-readable message/disposition-notification ---
    // Reporting-UA: hostname; inbx <version>  (RFC 8098 §3.2.1)
    // Format: "mta-name-type; product" — we use hostname as the UA name and
    // "inbx" + the crate version as the product identifier.
    let reporting_ua_line = format!(
        "Reporting-UA: {}; inbx {}\r\n",
        ctx.reporting_ua,
        env!("CARGO_PKG_VERSION"),
    );

    // Final-Recipient: rfc822;<from-address-of-MDN-sender>  (RFC 8098 §3.2.4)
    let final_recipient_addr = bare_address(&ctx.from);
    let final_recipient_line = format!("Final-Recipient: rfc822;{final_recipient_addr}\r\n");

    // Original-Recipient (optional) — the original sender's email.
    let original_recipient_line = ctx
        .original_recipient
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|addr| format!("Original-Recipient: rfc822;{addr}\r\n"))
        .unwrap_or_default();

    // Original-Message-ID  (RFC 8098 §3.2.5)
    let original_msg_id_line = format!(
        "Original-Message-ID: <{}>\r\n",
        ctx.original_message_id.trim_matches('<').trim_matches('>')
    );

    // Disposition  (RFC 8098 §3.2.6)
    let disposition_line = format!("Disposition: {}\r\n", ctx.disposition.wire_value());

    let machine_body = format!(
        "{reporting_ua_line}{original_recipient_line}{final_recipient_line}{original_msg_id_line}{disposition_line}"
    );

    // --- Assemble multipart/report ---
    // mail-builder doesn't have a first-class multipart/report helper, so we
    // compose the two sub-parts as MimeParts and attach them manually.
    let text_part = MimePart::new(
        ContentType::new("text/plain").attribute("charset", "utf-8"),
        BodyPart::Text(human_text.into()),
    );

    let machine_part = MimePart::new(
        ContentType::new("message/disposition-notification"),
        BodyPart::Text(machine_body.into()),
    );

    // Build the outer multipart/report with both parts.
    let report_part = MimePart::new(
        ContentType::new("multipart/report").attribute("report-type", "disposition-notification"),
        BodyPart::Multipart(vec![text_part, machine_part]),
    );

    let to_pairs: Vec<(String, String)> = ctx
        .to
        .iter()
        .map(|addr| (String::new(), addr.clone()))
        .collect();

    let subject = format!("Read: {}", ctx.original_subject);

    let bytes = MessageBuilder::new()
        .from((String::new(), ctx.from.clone()))
        .to(to_pairs)
        .subject(subject)
        .body(report_part)
        .write_to_vec()?;

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};

    fn sample_ctx() -> MdnContext {
        MdnContext {
            from: "Bob <bob@example.com>".into(),
            to: vec!["alice@example.com".into()],
            original_message_id: "abc123@example.com".into(),
            original_recipient: Some("alice@example.com".into()),
            original_subject: "Hello World".into(),
            disposition: Disposition::DisplayedManualAction,
            reporting_ua: "testhost".into(),
        }
    }

    #[test]
    fn build_mdn_round_trip() {
        let ctx = sample_ctx();
        let bytes = build_mdn(&ctx).expect("build_mdn io ok");

        let parsed = MessageParser::default().parse(&bytes).expect("re-parse ok");

        // Subject must be "Read: Hello World".
        assert_eq!(parsed.subject().unwrap_or(""), "Read: Hello World",);

        // Top-level Content-Type: multipart/report
        let ct_header = parsed.content_type().expect("Content-Type present");
        assert_eq!(ct_header.ctype(), "multipart");
        assert_eq!(ct_header.subtype().unwrap_or(""), "report");
        assert_eq!(
            ct_header.attribute("report-type").unwrap_or(""),
            "disposition-notification"
        );

        // Check that both MIME parts are present and correctly typed.
        use mail_parser::PartType;
        let text_parts: Vec<_> = parsed
            .parts
            .iter()
            .filter(|p| {
                p.content_type()
                    .map(|ct| ct.ctype() == "text" && ct.subtype().unwrap_or("") == "plain")
                    .unwrap_or(false)
                    && matches!(p.body, PartType::Text(_))
            })
            .collect();
        assert!(!text_parts.is_empty(), "text/plain part present");

        let machine_parts: Vec<_> = parsed
            .parts
            .iter()
            .filter(|p| {
                p.content_type()
                    .map(|ct| {
                        ct.ctype() == "message"
                            && ct.subtype().unwrap_or("") == "disposition-notification"
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            !machine_parts.is_empty(),
            "message/disposition-notification part present"
        );

        // Disposition line must match RFC 8098 exactly — check raw bytes since
        // mail-parser represents message/* parts as nested Message, not Text.
        let raw_str = String::from_utf8_lossy(&bytes);
        assert!(
            raw_str.contains("Disposition: manual-action/MDN-sent-manually; displayed"),
            "RFC 8098 Disposition line present in raw output"
        );
    }

    #[test]
    fn build_mdn_includes_original_message_id() {
        let ctx = sample_ctx();
        let bytes = build_mdn(&ctx).expect("build_mdn ok");

        let raw_str = String::from_utf8_lossy(&bytes);
        assert!(
            raw_str.contains("Original-Message-ID: <abc123@example.com>"),
            "Original-Message-ID present"
        );
    }
}
