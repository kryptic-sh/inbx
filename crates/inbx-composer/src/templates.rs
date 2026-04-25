//! Per-account templates as RFC 5322 files.
//!
//! Stored at `$XDG_DATA_HOME/inbx/<account>/templates/<name>.eml`.
//! Loading a template seeds a [`Composer`] with the template's
//! Subject/To/Cc/Bcc + body, then overrides the From line via the
//! caller's [`Identity`]. Useful for canned replies and recurring
//! announcements.

use std::path::PathBuf;

use mail_parser::MessageParser;

use crate::{Composer, Field, Identity};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("parse: could not parse RFC 5322")]
    Parse,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid template name (must match [A-Za-z0-9._-]+): {0}")]
    InvalidName(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn templates_dir(account: &str) -> Result<PathBuf> {
    Ok(inbx_config::data_dir()?.join(account).join("templates"))
}

pub fn ensure_dir(account: &str) -> Result<PathBuf> {
    let dir = templates_dir(account)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(Error::InvalidName(name.into()));
    }
    Ok(())
}

pub fn save(account: &str, name: &str, raw: &[u8]) -> Result<PathBuf> {
    validate_name(name)?;
    let dir = ensure_dir(account)?;
    let path = dir.join(format!("{name}.eml"));
    std::fs::write(&path, raw)?;
    Ok(path)
}

pub fn delete(account: &str, name: &str) -> Result<()> {
    validate_name(name)?;
    let path = templates_dir(account)?.join(format!("{name}.eml"));
    if !path.exists() {
        return Err(Error::NotFound(name.into()));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

pub fn load_raw(account: &str, name: &str) -> Result<Vec<u8>> {
    validate_name(name)?;
    let path = templates_dir(account)?.join(format!("{name}.eml"));
    if !path.exists() {
        return Err(Error::NotFound(name.into()));
    }
    Ok(std::fs::read(path)?)
}

pub fn list(account: &str) -> Result<Vec<String>> {
    let dir = templates_dir(account)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str())
            && entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("eml"))
        {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Build a [`Composer`] seeded from the named template's headers + body.
pub fn from_template(identity: Identity, account: &str, name: &str) -> Result<Composer> {
    let raw = load_raw(account, name)?;
    let parsed = MessageParser::default().parse(&raw).ok_or(Error::Parse)?;
    let mut composer = Composer::new_blank(identity);

    if let Some(s) = parsed.subject() {
        composer.subject.set_content(s);
    }
    if let Some(group) = parsed.to() {
        let joined = group
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            composer.to.set_content(&joined);
        }
    }
    if let Some(group) = parsed.cc() {
        let joined = group
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            composer.cc.set_content(&joined);
        }
    }
    if let Some(group) = parsed.bcc() {
        let joined = group
            .iter()
            .filter_map(|a| a.address().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            composer.bcc.set_content(&joined);
        }
    }
    if let Some(body) = parsed.body_text(0) {
        let mut text = body.to_string();
        if let Some(sig) = composer.identity.signature_block()
            && !text.contains("-- ")
        {
            text.push('\n');
            text.push_str(&sig);
        }
        composer.body.set_content(&text);
    }
    composer.focus = Field::To;
    Ok(composer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(validate_name("ok").is_ok());
        assert!(validate_name("with-dash_under.eml").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("has space").is_err());
    }
}
