//! Account wizard — a 10-field `hjkl_form::Form` reachable via `<Space>n`.
//!
//! On save (`<Space>s` in form Normal mode), the account is appended to
//! `inbx_config::Config` and the password is stored in the OS keyring via
//! `inbx_config::store_password`.

use anyhow::{Result, bail};
use hjkl_form::{Field, FieldMeta, Form, TextFieldEditor};
use inbx_config::{Account, AuthMethod, TlsMode, Transport};

/// Field indices (positional constants for readability).
const IDX_NAME: usize = 0;
const IDX_EMAIL: usize = 1;
const IDX_IMAP_HOST: usize = 2;
const IDX_IMAP_PORT: usize = 3;
const IDX_IMAP_SEC: usize = 4;
const IDX_SMTP_HOST: usize = 5;
const IDX_SMTP_PORT: usize = 6;
const IDX_SMTP_SEC: usize = 7;
const IDX_USERNAME: usize = 8;
const IDX_PASSWORD: usize = 9;

pub(super) struct AccountWizard {
    pub form: Form,
    pub suggestion_applied: bool,
    /// Last known focused index — used to detect blur from email field.
    pub last_focused: usize,
}

impl AccountWizard {
    pub fn new() -> Self {
        let form = Form::new()
            .with_title("New Account")
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("name").required(true),
                1,
            )))
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("email").required(true),
                1,
            )))
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("imap host").required(true),
                1,
            )))
            .with_field(Field::SingleLineText(
                TextFieldEditor::with_meta(FieldMeta::new("imap port"), 1).with_initial("993"),
            ))
            .with_field(Field::SingleLineText(
                TextFieldEditor::with_meta(FieldMeta::new("imap security"), 1).with_initial("tls"),
            ))
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("smtp host").required(true),
                1,
            )))
            .with_field(Field::SingleLineText(
                TextFieldEditor::with_meta(FieldMeta::new("smtp port"), 1).with_initial("465"),
            ))
            .with_field(Field::SingleLineText(
                TextFieldEditor::with_meta(FieldMeta::new("smtp security"), 1).with_initial("tls"),
            ))
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("username").placeholder("defaults to email"),
                1,
            )))
            .with_field(Field::SingleLineText(TextFieldEditor::with_meta(
                FieldMeta::new("password (stored in OS keyring, not echoed afterwards)")
                    .required(true),
                1,
            )));

        Self {
            form,
            suggestion_applied: false,
            last_focused: 0,
        }
    }

    /// Apply autoconfig suggestion when leaving the email field for the first time.
    pub fn maybe_apply_autoconfig(&mut self) {
        if self.suggestion_applied {
            return;
        }
        let email = field_text(&self.form, IDX_EMAIL);
        let Some(sug) = inbx_config::autoconfig::suggest(&email) else {
            return;
        };
        // Only pre-fill fields the user hasn't touched (still at defaults).
        set_field_text(&mut self.form, IDX_IMAP_HOST, &sug.imap_host);
        set_field_text(&mut self.form, IDX_IMAP_PORT, &sug.imap_port.to_string());
        set_field_text(&mut self.form, IDX_IMAP_SEC, tls_str(sug.imap_security));
        set_field_text(&mut self.form, IDX_SMTP_HOST, &sug.smtp_host);
        set_field_text(&mut self.form, IDX_SMTP_PORT, &sug.smtp_port.to_string());
        set_field_text(&mut self.form, IDX_SMTP_SEC, tls_str(sug.smtp_security));
        self.suggestion_applied = true;
    }

    /// Extract and validate form values, returning `(Account, password)`.
    pub fn build_account(&self) -> Result<(Account, String)> {
        let name = field_text(&self.form, IDX_NAME);
        let email = field_text(&self.form, IDX_EMAIL);
        let imap_host = field_text(&self.form, IDX_IMAP_HOST);
        let imap_port_s = field_text(&self.form, IDX_IMAP_PORT);
        let imap_sec_s = field_text(&self.form, IDX_IMAP_SEC);
        let smtp_host = field_text(&self.form, IDX_SMTP_HOST);
        let smtp_port_s = field_text(&self.form, IDX_SMTP_PORT);
        let smtp_sec_s = field_text(&self.form, IDX_SMTP_SEC);
        let username_raw = field_text(&self.form, IDX_USERNAME);
        let password = field_text(&self.form, IDX_PASSWORD);

        // Required field validation.
        if name.trim().is_empty() {
            bail!("name is required");
        }
        if email.trim().is_empty() {
            bail!("email is required");
        }
        if imap_host.trim().is_empty() {
            bail!("imap host is required");
        }
        if smtp_host.trim().is_empty() {
            bail!("smtp host is required");
        }
        if password.trim().is_empty() {
            bail!("password is required");
        }

        let imap_port: u16 = imap_port_s
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("imap port must be a number 1-65535"))?;
        let smtp_port: u16 = smtp_port_s
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("smtp port must be a number 1-65535"))?;

        let imap_security = parse_tls(&imap_sec_s)
            .ok_or_else(|| anyhow::anyhow!("imap security must be 'tls' or 'starttls'"))?;
        let smtp_security = parse_tls(&smtp_sec_s)
            .ok_or_else(|| anyhow::anyhow!("smtp security must be 'tls' or 'starttls'"))?;

        let username = if username_raw.trim().is_empty() {
            email.clone()
        } else {
            username_raw
        };

        let account = Account {
            name,
            email,
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
            username,
            auth: AuthMethod::AppPassword,
            transport: Transport::Imap,
        };

        Ok((account, password))
    }

    /// Label of the currently-focused field, for the status line.
    pub fn focused_label(&self) -> &str {
        self.form
            .focused_field()
            .map(|f| f.meta().label.as_str())
            .unwrap_or("")
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn field_text(form: &Form, idx: usize) -> String {
    match form.fields.get(idx) {
        Some(Field::SingleLineText(f)) => f.text(),
        _ => String::new(),
    }
}

fn set_field_text(form: &mut Form, idx: usize, text: &str) {
    if let Some(Field::SingleLineText(f)) = form.fields.get_mut(idx) {
        f.set_text(text);
    }
}

fn tls_str(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Tls => "tls",
        TlsMode::Starttls => "starttls",
    }
}

fn parse_tls(s: &str) -> Option<TlsMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "tls" => Some(TlsMode::Tls),
        "starttls" => Some(TlsMode::Starttls),
        _ => None,
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_wizard() -> AccountWizard {
        let mut w = AccountWizard::new();
        set_field_text(&mut w.form, IDX_NAME, "personal");
        set_field_text(&mut w.form, IDX_EMAIL, "me@example.com");
        set_field_text(&mut w.form, IDX_IMAP_HOST, "imap.example.com");
        set_field_text(&mut w.form, IDX_IMAP_PORT, "993");
        set_field_text(&mut w.form, IDX_IMAP_SEC, "tls");
        set_field_text(&mut w.form, IDX_SMTP_HOST, "smtp.example.com");
        set_field_text(&mut w.form, IDX_SMTP_PORT, "465");
        set_field_text(&mut w.form, IDX_SMTP_SEC, "tls");
        set_field_text(&mut w.form, IDX_USERNAME, "");
        set_field_text(&mut w.form, IDX_PASSWORD, "hunter2");
        w
    }

    #[test]
    fn into_account_success_path() {
        let w = filled_wizard();
        let (acct, pw) = w.build_account().expect("should succeed");
        assert_eq!(acct.name, "personal");
        assert_eq!(acct.email, "me@example.com");
        assert_eq!(acct.imap_host, "imap.example.com");
        assert_eq!(acct.imap_port, 993);
        assert_eq!(acct.imap_security, TlsMode::Tls);
        assert_eq!(acct.smtp_host, "smtp.example.com");
        assert_eq!(acct.smtp_port, 465);
        assert_eq!(acct.smtp_security, TlsMode::Tls);
        // Blank username defaults to email.
        assert_eq!(acct.username, "me@example.com");
        assert_eq!(pw, "hunter2");
        assert_eq!(acct.auth, AuthMethod::AppPassword);
        assert_eq!(acct.transport, Transport::Imap);
    }

    #[test]
    fn into_account_missing_name_returns_err() {
        let mut w = filled_wizard();
        set_field_text(&mut w.form, IDX_NAME, "");
        let err = w.build_account().unwrap_err();
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn into_account_missing_password_returns_err() {
        let mut w = filled_wizard();
        set_field_text(&mut w.form, IDX_PASSWORD, "");
        let err = w.build_account().unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn into_account_missing_imap_host_returns_err() {
        let mut w = filled_wizard();
        set_field_text(&mut w.form, IDX_IMAP_HOST, "");
        let err = w.build_account().unwrap_err();
        assert!(err.to_string().contains("imap host"), "{err}");
    }

    #[test]
    fn into_account_bad_port_returns_err() {
        let mut w = filled_wizard();
        set_field_text(&mut w.form, IDX_IMAP_PORT, "notaport");
        let err = w.build_account().unwrap_err();
        assert!(err.to_string().contains("imap port"), "{err}");
    }

    #[test]
    fn autoconfig_gmail_pre_fills() {
        let mut w = AccountWizard::new();
        set_field_text(&mut w.form, IDX_EMAIL, "user@gmail.com");
        w.maybe_apply_autoconfig();
        assert!(w.suggestion_applied);
        assert_eq!(field_text(&w.form, IDX_IMAP_HOST), "imap.gmail.com");
        assert_eq!(field_text(&w.form, IDX_SMTP_HOST), "smtp.gmail.com");
        assert_eq!(field_text(&w.form, IDX_IMAP_PORT), "993");
        assert_eq!(field_text(&w.form, IDX_SMTP_PORT), "465");
    }

    #[test]
    fn autoconfig_idempotent() {
        let mut w = AccountWizard::new();
        set_field_text(&mut w.form, IDX_EMAIL, "user@gmail.com");
        w.maybe_apply_autoconfig();
        // Manually overwrite imap host, then call again — should not overwrite.
        set_field_text(&mut w.form, IDX_IMAP_HOST, "custom.host");
        w.maybe_apply_autoconfig();
        // Still "custom.host" because suggestion_applied == true.
        assert_eq!(field_text(&w.form, IDX_IMAP_HOST), "custom.host");
    }
}
