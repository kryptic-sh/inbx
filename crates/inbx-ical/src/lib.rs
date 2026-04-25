//! Parse and reply to iCalendar (RFC 5545) invites embedded in MIME mail.

use icalendar::{Calendar, CalendarComponent, Component, Event, EventLike, Property};
use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: {0}")]
    Parse(String),
    #[error("no calendar part in message")]
    NoCalendar,
    #[error("no VEVENT in calendar")]
    NoEvent,
    #[error("invalid attendee: {0}")]
    InvalidAttendee(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsvpResponse {
    Accept,
    Decline,
    Tentative,
}

impl RsvpResponse {
    fn partstat(&self) -> &'static str {
        match self {
            Self::Accept => "ACCEPTED",
            Self::Decline => "DECLINED",
            Self::Tentative => "TENTATIVE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Invite {
    pub uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub organizer: Option<String>,
    pub attendees: Vec<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub method: Option<String>,
    pub raw: String,
}

/// Find the first text/calendar part in a raw RFC 5322 message and parse it.
pub fn parse_message(raw: &[u8]) -> Result<Invite> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Parse("could not parse message".into()))?;

    for part in parsed.parts.iter() {
        if let PartType::Text(t) = &part.body
            && let Some(ct) = part.content_type()
        {
            let ct_lower = ct.ctype().to_ascii_lowercase();
            let st_lower = ct
                .subtype()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if ct_lower == "text" && st_lower == "calendar" {
                return parse_ics(t);
            }
        }
    }
    Err(Error::NoCalendar)
}

pub fn parse_ics(text: &str) -> Result<Invite> {
    let cal: Calendar = text.parse().map_err(Error::Parse)?;
    let method = cal.property_value("METHOD").map(|s| s.to_string());

    let event = cal
        .components
        .iter()
        .find_map(|c| match c {
            CalendarComponent::Event(e) => Some(e.clone()),
            _ => None,
        })
        .ok_or(Error::NoEvent)?;

    let attendees: Vec<String> = event
        .multi_properties()
        .get("ATTENDEE")
        .map(|v| v.iter().map(|p| p.value().to_string()).collect())
        .unwrap_or_default();

    let organizer = event.property_value("ORGANIZER").map(|s| s.to_string());
    let summary = event.get_summary().map(|s| s.to_string());
    let description = event.get_description().map(|s| s.to_string());
    let location = event.get_location().map(|s| s.to_string());
    let uid = event
        .get_uid()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("inbx-{}", rand_uid()));
    let start = event.property_value("DTSTART").map(|s| s.to_string());
    let end = event.property_value("DTEND").map(|s| s.to_string());

    Ok(Invite {
        uid,
        summary,
        description,
        location,
        organizer,
        attendees,
        start,
        end,
        method,
        raw: text.to_string(),
    })
}

/// Build a METHOD:REPLY .ics for the given invite + response. The attendee
/// argument should be a `mailto:` URI matching one of the original ATTENDEE
/// properties; only that line is emitted on the reply (per RFC 5546).
pub fn build_reply(invite: &Invite, response: RsvpResponse, attendee: &str) -> Result<String> {
    if !attendee.to_ascii_lowercase().starts_with("mailto:") {
        return Err(Error::InvalidAttendee(attendee.into()));
    }

    let mut event = Event::new();
    event.uid(&invite.uid);
    if let Some(s) = invite.summary.as_deref() {
        event.summary(s);
    }
    if let Some(s) = invite.start.as_deref() {
        event.append_property(Property::new("DTSTART", s).done());
    }
    if let Some(s) = invite.end.as_deref() {
        event.append_property(Property::new("DTEND", s).done());
    }
    if let Some(o) = invite.organizer.as_deref() {
        event.append_property(Property::new("ORGANIZER", o).done());
    }
    event.append_property(
        Property::new("ATTENDEE", attendee)
            .add_parameter("PARTSTAT", response.partstat())
            .add_parameter("RSVP", "TRUE")
            .done(),
    );

    let mut cal = Calendar::new();
    cal.append_property(Property::new("METHOD", "REPLY").done());
    cal.push(event);
    Ok(cal.to_string())
}

fn rand_uid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//Example//EN\r\n\
        METHOD:REQUEST\r\n\
        BEGIN:VEVENT\r\n\
        UID:abcd-1234@example.com\r\n\
        SUMMARY:Project sync\r\n\
        DTSTART:20260601T140000Z\r\n\
        DTEND:20260601T150000Z\r\n\
        ORGANIZER:mailto:boss@example.com\r\n\
        ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n\
        END:VEVENT\r\n\
        END:VCALENDAR\r\n";

    #[test]
    fn parses_invite() {
        let inv = parse_ics(SAMPLE).unwrap();
        assert_eq!(inv.uid, "abcd-1234@example.com");
        assert_eq!(inv.summary.as_deref(), Some("Project sync"));
        assert_eq!(inv.method.as_deref(), Some("REQUEST"));
        assert!(inv.attendees.iter().any(|a| a.contains("me@example.com")));
    }

    #[test]
    fn builds_reply() {
        let inv = parse_ics(SAMPLE).unwrap();
        let reply = build_reply(&inv, RsvpResponse::Accept, "mailto:me@example.com").unwrap();
        assert!(reply.contains("METHOD:REPLY"));
        assert!(reply.contains("PARTSTAT=ACCEPTED"));
        assert!(reply.contains("UID:abcd-1234@example.com"));
    }
}
