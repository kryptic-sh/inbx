// mbox format helpers: split, quoting, date formatting, and flag translation.

// ── From-line date formatter ─────────────────────────────────────────────────

/// Convert a Unix timestamp to the asctime-style date used in mbox `From_`
/// separator lines, e.g. `"Fri Feb 13 23:31:30 2009"`.
///
/// This matches RFC 4155 §2: `From <sender> <asctime>` where asctime is
/// `"www mmm dd HH:MM:SS yyyy"` (24-hour, no timezone).
pub fn format_unix_from_line(ts: i64) -> String {
    // Clamp negative timestamps to epoch; negative mbox dates are nonsensical.
    let secs = ts.max(0) as u64;
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    const DOW: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Day-of-week: epoch (1970-01-01) was Thursday = index 0.
    let days_since_epoch = (secs / 86400) as usize;
    let dow = DOW[days_since_epoch % 7];
    let mon = MON[(mo - 1) as usize];
    // mbox convention: single-digit day is space-padded on the left.
    format!("{dow} {mon} {d:2} {h:02}:{mi:02}:{s:02} {y}")
}

/// Decompose a Unix timestamp (seconds since 1970-01-01 00:00:00 UTC) into
/// (year, month 1-12, day 1-31, hour, minute, second).
///
/// Uses the civil-from-days algorithm by Howard Hinnant
/// (https://howardhinnant.github.io/date_algorithms.html).
fn civil_from_unix(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = secs % 86400;
    let h = (s / 3600) as u32;
    let mi = ((s % 3600) / 60) as u32;
    let sec = (s % 60) as u32;

    // Days since epoch (truncated toward zero for non-negative input).
    let z = (secs / 86400) as i64 + 719468; // shift to 0000-03-01 epoch
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = (yoe as i64 + era * 400) as u32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month of the March-based year [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // day [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // month [1, 12]
    let y = if mo <= 2 { y + 1 } else { y };

    (y, mo, d, h, mi, sec)
}

// ── mbox splitting ───────────────────────────────────────────────────────────

/// Split an mbox byte buffer into individual RFC 5322 message byte vectors.
///
/// Each `From_` separator line is consumed and discarded. The bodies returned
/// have mboxo `>From`-quoting reversed via [`strip_from_quoting`].
pub fn split_mbox(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut at_line_start = true;
    let mut i = 0;
    while i < buf.len() {
        if at_line_start && buf[i..].starts_with(b"From ") {
            if !current.is_empty() {
                out.push(strip_from_quoting(std::mem::take(&mut current)));
            }
            // Skip the From_ separator line.
            while i < buf.len() && buf[i] != b'\n' {
                i += 1;
            }
            if i < buf.len() {
                i += 1; // consume the newline
            }
            continue;
        }
        let b = buf[i];
        current.push(b);
        at_line_start = b == b'\n';
        i += 1;
    }
    if !current.is_empty() {
        out.push(strip_from_quoting(current));
    }
    out
}

// ── From-quoting ─────────────────────────────────────────────────────────────

/// Apply mboxo From-quoting: prepend `>` to every line that starts with
/// `From ` (after any existing `>`-prefixes).
pub fn apply_from_quoting(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 8);
    for line in raw.split(|b| *b == b'\n') {
        // Count leading '>' chars.
        let gt = line.iter().take_while(|&&b| b == b'>').count();
        let rest = &line[gt..];
        if rest.starts_with(b"From ") {
            // Need to add one more '>'.
            out.extend_from_slice(&line[..gt]);
            out.push(b'>');
            out.extend_from_slice(rest);
        } else {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }
    // The split above always produces a trailing empty slice after the final
    // '\n', which adds a spurious trailing newline.  Trim exactly one.
    if out.last() == Some(&b'\n') {
        out.pop();
    }
    out
}

/// Reverse mboxo From-quoting: `>From ` → `From `, `>>From ` → `>From `, etc.
///
/// Removes exactly one leading `>` from any line whose content (after stripping
/// all leading `>`s) starts with `From `.  Lines that don't match are unchanged.
pub fn strip_from_quoting(buf: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut at_line_start = true;
    let mut i = 0;
    while i < buf.len() {
        if at_line_start && buf[i] == b'>' {
            // Count how many leading '>'s there are.
            let gt_count = buf[i..].iter().take_while(|&&b| b == b'>').count();
            let after_gts = &buf[i + gt_count..];
            if after_gts.starts_with(b"From ") {
                // Drop exactly one leading '>'.
                i += 1;
            }
        }
        out.push(buf[i]);
        at_line_start = buf[i] == b'\n';
        i += 1;
    }
    out
}

// ── Status / X-Status flag translation ───────────────────────────────────────

/// Parse RFC 4155 `Status:` and `X-Status:` header values from a raw message
/// and return an IMAP-style flag string (space-separated `\Flag` tokens).
///
/// Mapping used (consistent with mutt / Dovecot convention):
///
/// | mbox char | header | IMAP flag       |
/// |-----------|--------|-----------------|
/// | R         | Status | `\\Seen`        |
/// | O         | Status | (old/not-recent, no direct IMAP flag) |
/// | F         | X-Status | `\\Flagged`   |
/// | A         | X-Status | `\\Answered`  |
/// | D         | X-Status | `\\Deleted`   |
/// | T         | X-Status | `\\Draft`     |
///
/// When no `Status:` / `X-Status:` headers are present the returned string is
/// empty (caller should not default to `\Seen`).
pub fn flags_from_status_headers(raw: &[u8]) -> String {
    let status = header_value(raw, b"Status");
    let x_status = header_value(raw, b"X-Status");
    let mut flags: Vec<&'static str> = Vec::new();
    if status.contains(&b'R') {
        flags.push("\\Seen");
    }
    if x_status.contains(&b'F') {
        flags.push("\\Flagged");
    }
    if x_status.contains(&b'A') {
        flags.push("\\Answered");
    }
    if x_status.contains(&b'D') {
        flags.push("\\Deleted");
    }
    if x_status.contains(&b'T') {
        flags.push("\\Draft");
    }
    flags.join(" ")
}

/// Build `Status:` and `X-Status:` header lines from a space-separated IMAP
/// flag string.  Returns an empty string when no relevant flags are set.
pub fn status_headers_from_flags(flags: &str) -> String {
    let seen = flags.contains("\\Seen");
    let flagged = flags.contains("\\Flagged");
    let answered = flags.contains("\\Answered");
    let deleted = flags.contains("\\Deleted");
    let draft = flags.contains("\\Draft");

    let mut status = String::new();
    if seen {
        status.push('R');
    }
    // 'O' means "old" (has been in the mailbox before); we set it whenever
    // we know the message has been seen or is being exported from a local
    // index (implying it has been delivered).
    status.push('O');

    let mut x_status = String::new();
    if flagged {
        x_status.push('F');
    }
    if answered {
        x_status.push('A');
    }
    if deleted {
        x_status.push('D');
    }
    if draft {
        x_status.push('T');
    }

    let mut out = String::new();
    if !status.is_empty() {
        out.push_str("Status: ");
        out.push_str(&status);
        out.push('\n');
    }
    if !x_status.is_empty() {
        out.push_str("X-Status: ");
        out.push_str(&x_status);
        out.push('\n');
    }
    out
}

/// Extract the value of a single-line ASCII header (case-insensitive name
/// match). Returns an empty slice when absent or multi-line.
fn header_value<'a>(raw: &'a [u8], name: &[u8]) -> &'a [u8] {
    for line in raw.split(|b| *b == b'\n') {
        // Headers end at the blank line.
        if line.is_empty() || line == b"\r" {
            break;
        }
        let colon = match line.iter().position(|b| *b == b':') {
            Some(p) => p,
            None => continue,
        };
        let hname = line[..colon].trim_ascii();
        if hname.eq_ignore_ascii_case(name) {
            let val = line[colon + 1..].trim_ascii();
            return val;
        }
    }
    b""
}

/// Insert `Status:` / `X-Status:` header lines into a raw RFC 5322 message
/// immediately after the existing headers, before the body separator.
///
/// If the message already contains `Status:` or `X-Status:` headers, they are
/// replaced in-place so callers do not need to strip them first.
pub fn inject_status_headers(raw: &[u8], flags: &str) -> Vec<u8> {
    let header_block = status_headers_from_flags(flags);
    if header_block.is_empty() {
        return raw.to_vec();
    }

    // Find the end of the header section (first blank line).
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + header_block.len() + 4);
    let lines = raw.split(|b| *b == b'\n');
    let mut in_headers = true;
    let mut status_written = false;

    for line in lines {
        let stripped = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };

        if in_headers {
            // Skip existing Status: / X-Status: headers so we replace them.
            if let Some(colon) = stripped.iter().position(|b| *b == b':') {
                let hname = stripped[..colon].trim_ascii();
                if hname.eq_ignore_ascii_case(b"Status") || hname.eq_ignore_ascii_case(b"X-Status")
                {
                    // Don't emit; we'll write our own below.
                    continue;
                }
            }

            if stripped.is_empty() {
                // Blank line = end of headers.  Inject before it.
                if !status_written {
                    out.extend_from_slice(header_block.as_bytes());
                    status_written = true;
                }
                in_headers = false;
            }
        }

        out.extend_from_slice(line);
        out.push(b'\n');
    }

    // If there was no blank line (headers-only message), append anyway.
    if !status_written {
        out.extend_from_slice(header_block.as_bytes());
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_unix_from_line ─────────────────────────────────────────────

    #[test]
    fn format_known_epoch() {
        // Unix timestamp 1234567890 = 2009-02-13 23:31:30 UTC (a Friday)
        assert_eq!(
            format_unix_from_line(1_234_567_890),
            "Fri Feb 13 23:31:30 2009"
        );
    }

    #[test]
    fn format_unix_zero() {
        // Epoch itself: 1970-01-01 00:00:00 Thursday
        assert_eq!(format_unix_from_line(0), "Thu Jan  1 00:00:00 1970");
    }

    #[test]
    fn format_negative_clamps_to_epoch() {
        assert_eq!(format_unix_from_line(-100), "Thu Jan  1 00:00:00 1970");
    }

    #[test]
    fn format_known_date_2() {
        // 2006-01-02 15:04:05 UTC = 1136214245 (a Monday)
        assert_eq!(
            format_unix_from_line(1_136_214_245),
            "Mon Jan  2 15:04:05 2006"
        );
    }

    // ── split_mbox ────────────────────────────────────────────────────────

    #[test]
    fn split_mbox_three_messages() {
        let mbox = b"\
From alice@example.com Mon Jan  1 00:00:00 2024\n\
From: alice@example.com\n\
Subject: one\n\
\n\
Body one.\n\
\n\
From bob@example.com Mon Jan  1 00:00:01 2024\n\
From: bob@example.com\n\
Subject: two\n\
\n\
Body two.\n\
\n\
From carol@example.com Mon Jan  1 00:00:02 2024\n\
From: carol@example.com\n\
Subject: three\n\
\n\
Body three.\n";

        let msgs = split_mbox(mbox);
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0].windows(9).any(|w| w == b"Body one."));
        assert!(msgs[1].windows(9).any(|w| w == b"Body two."));
        assert!(msgs[2].windows(11).any(|w| w == b"Body three."));
        // Separator lines must not appear in output.
        for m in &msgs {
            assert!(
                !m.windows(5).any(|w| w == b"From "),
                "From_ line leaked into message body"
            );
        }
    }

    #[test]
    fn split_mbox_empty_input() {
        assert!(split_mbox(b"").is_empty());
    }

    #[test]
    fn split_mbox_single_no_separator() {
        // A raw .eml with no From_ line is treated as a single message.
        let raw = b"From: a@b.c\r\nSubject: hi\r\n\r\nHello.\r\n";
        let msgs = split_mbox(raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], raw);
    }

    // ── strip_from_quoting ────────────────────────────────────────────────

    #[test]
    fn strip_single_quote() {
        let input = b">From alice@example.com\nsome body\n".to_vec();
        let out = strip_from_quoting(input);
        assert!(out.starts_with(b"From alice@example.com"));
    }

    #[test]
    fn strip_double_quote() {
        let input = b">>From alice@example.com\n".to_vec();
        let out = strip_from_quoting(input);
        assert!(out.starts_with(b">From alice@example.com"));
    }

    #[test]
    fn strip_leaves_other_gt_lines_alone() {
        let input = b"> quoted reply line\nsome body\n".to_vec();
        let out = strip_from_quoting(input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn strip_leaves_normal_from_header_alone() {
        // A "From:" header line (with colon) must not be touched.
        let input = b"From: alice@example.com\n".to_vec();
        let out = strip_from_quoting(input.clone());
        assert_eq!(out, input);
    }

    // ── flags_from_status_headers ─────────────────────────────────────────

    #[test]
    fn status_r_gives_seen() {
        let raw = b"From: a@b.c\nStatus: R\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "\\Seen");
    }

    #[test]
    fn status_ro_gives_seen() {
        let raw = b"From: a@b.c\nStatus: RO\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "\\Seen");
    }

    #[test]
    fn x_status_f_gives_flagged() {
        let raw = b"From: a@b.c\nX-Status: F\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "\\Flagged");
    }

    #[test]
    fn x_status_combined() {
        let raw = b"From: a@b.c\nStatus: R\nX-Status: FA\n\nBody\n";
        let flags = flags_from_status_headers(raw);
        assert!(flags.contains("\\Seen"));
        assert!(flags.contains("\\Flagged"));
        assert!(flags.contains("\\Answered"));
    }

    #[test]
    fn no_status_headers_gives_empty() {
        let raw = b"From: a@b.c\nSubject: hi\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "");
    }

    #[test]
    fn x_status_d_gives_deleted() {
        let raw = b"From: a@b.c\nX-Status: D\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "\\Deleted");
    }

    #[test]
    fn x_status_t_gives_draft() {
        let raw = b"From: a@b.c\nX-Status: T\n\nBody\n";
        assert_eq!(flags_from_status_headers(raw), "\\Draft");
    }

    // ── status_headers_from_flags ─────────────────────────────────────────

    #[test]
    fn flags_seen_answered_to_headers() {
        let h = status_headers_from_flags("\\Seen \\Answered");
        // Status must have 'R' and 'O'.
        assert!(h.contains("Status: RO") || h.contains("Status: R"));
        // X-Status must have 'A'.
        assert!(h.contains("X-Status: A") || h.contains("X-Status:"));
        let x: String = h
            .lines()
            .find(|l| l.starts_with("X-Status:"))
            .unwrap_or("")
            .to_string();
        assert!(x.contains('A'), "X-Status missing 'A': {h:?}");
    }

    #[test]
    fn flags_none_gives_status_o_only() {
        // No IMAP flags → Status: O (old), no X-Status.
        let h = status_headers_from_flags("");
        assert!(h.contains("Status: O"), "expected 'Status: O', got: {h:?}");
        assert!(!h.contains("X-Status:"));
    }

    #[test]
    fn flags_flagged_deleted_draft() {
        let h = status_headers_from_flags("\\Flagged \\Deleted \\Draft");
        let x: &str = h.lines().find(|l| l.starts_with("X-Status:")).unwrap_or("");
        assert!(x.contains('F'));
        assert!(x.contains('D'));
        assert!(x.contains('T'));
    }

    // ── round-trip ────────────────────────────────────────────────────────

    #[test]
    fn mbox_round_trip_preserves_bodies() {
        // Build a 3-message mbox, split it, verify bytes.
        let msg1 = b"From: alice@example.com\r\nSubject: one\r\n\r\nBody one.\r\n";
        let msg2 = b"From: bob@example.com\r\nSubject: two\r\n\r\nBody two.\r\n";
        let msg3 = b"From: carol@example.com\r\nSubject: three\r\n\r\nBody three.\r\n";

        let now = "Thu Jan  1 00:00:00 1970";
        let mbox: Vec<u8> = [
            format!("From alice@example.com {now}\n").as_bytes(),
            msg1.as_ref(),
            b"\n",
            format!("From bob@example.com {now}\n").as_bytes(),
            msg2.as_ref(),
            b"\n",
            format!("From carol@example.com {now}\n").as_bytes(),
            msg3.as_ref(),
            b"\n",
        ]
        .concat();

        let msgs = split_mbox(&mbox);
        assert_eq!(msgs.len(), 3);

        // Bodies must survive the round-trip (modulo trailing whitespace
        // from the blank-line separator strip).
        let trimmed = |v: &[u8]| v.trim_ascii_end().to_vec();
        assert_eq!(trimmed(&msgs[0]), trimmed(msg1));
        assert_eq!(trimmed(&msgs[1]), trimmed(msg2));
        assert_eq!(trimmed(&msgs[2]), trimmed(msg3));
    }
}
