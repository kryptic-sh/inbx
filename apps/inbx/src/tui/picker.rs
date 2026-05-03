use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hjkl_picker::{PickerAction, PickerLogic, RequeryMode};
use inbx_config::Account;
use inbx_store::{FolderRow, MessageRow};

/// Generic stashed-result source. Items are populated up-front; `select`
/// writes the chosen payload into `last_picked` and returns `PickerAction::None`.
pub(super) struct StashedSource<T: Clone + Send + 'static> {
    title: &'static str,
    items: Vec<(String, T)>,
    pub last_picked: Arc<Mutex<Option<T>>>,
}

impl<T: Clone + Send + 'static> StashedSource<T> {
    pub fn new(title: &'static str, items: Vec<(String, T)>) -> Self {
        Self {
            title,
            items,
            last_picked: Arc::new(Mutex::new(None)),
        }
    }
}

impl<T: Clone + Send + 'static> PickerLogic for StashedSource<T> {
    fn title(&self) -> &str {
        self.title
    }

    fn item_count(&self) -> usize {
        self.items.len()
    }

    fn label(&self, idx: usize) -> String {
        self.items[idx].0.clone()
    }

    fn match_text(&self, idx: usize) -> String {
        self.items[idx].0.clone()
    }

    fn select(&self, idx: usize) -> PickerAction {
        if let Some(item) = self.items.get(idx)
            && let Ok(mut slot) = self.last_picked.lock()
        {
            *slot = Some(item.1.clone());
        }
        PickerAction::None
    }

    fn requery_mode(&self) -> RequeryMode {
        RequeryMode::FilterInMemory
    }

    fn has_preview(&self) -> bool {
        false
    }

    fn enumerate(
        &mut self,
        _query: Option<&str>,
        _cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<JoinHandle<()>> {
        None
    }
}

/// Open a folder picker. Returns the picker and the shared slot.
pub(super) fn folder_picker(
    folders: Vec<FolderRow>,
) -> (hjkl_picker::Picker, Arc<Mutex<Option<String>>>) {
    let items: Vec<(String, String)> = folders
        .into_iter()
        .map(|f| (f.name.clone(), f.name))
        .collect();
    let source = StashedSource::new("folders", items);
    let slot = Arc::clone(&source.last_picked);
    (hjkl_picker::Picker::new(Box::new(source)), slot)
}

/// Open an account switcher picker. Returns the picker and the shared slot.
pub(super) fn account_picker(
    accts: &[Account],
) -> (hjkl_picker::Picker, Arc<Mutex<Option<String>>>) {
    let items: Vec<(String, String)> = accts
        .iter()
        .map(|a| (format!("{}  <{}>", a.name, a.email), a.name.clone()))
        .collect();
    let source = StashedSource::new("accounts", items);
    let slot = Arc::clone(&source.last_picked);
    (hjkl_picker::Picker::new(Box::new(source)), slot)
}

/// Open a message-jump picker. Returns the picker and the shared slot.
pub(super) fn message_picker(
    msgs: Vec<MessageRow>,
) -> (hjkl_picker::Picker, Arc<Mutex<Option<i64>>>) {
    let items: Vec<(String, i64)> = msgs
        .iter()
        .map(|m| {
            let from = m.from_addr.as_deref().unwrap_or("(no from)");
            let subj = m.subject.as_deref().unwrap_or("(no subject)");
            (format!("{from} \u{2014} {subj}"), m.uid)
        })
        .collect();
    let source = StashedSource::new("messages", items);
    let slot = Arc::clone(&source.last_picked);
    (hjkl_picker::Picker::new(Box::new(source)), slot)
}

/// Open an attachment-save picker. Returns the picker and the shared slot.
/// `parts` is `(filename, bytes)` pairs.
pub(super) fn attachment_picker(
    parts: &[(String, Vec<u8>)],
) -> (hjkl_picker::Picker, Arc<Mutex<Option<usize>>>) {
    let items: Vec<(String, usize)> = parts
        .iter()
        .enumerate()
        .map(|(i, (name, data))| (format!("{name}  ({} bytes)", data.len()), i))
        .collect();
    let source = StashedSource::new("attachments", items);
    let slot = Arc::clone(&source.last_picked);
    (hjkl_picker::Picker::new(Box::new(source)), slot)
}

/// Extract attachments from a raw RFC 5322 message. Returns `(filename, bytes)` pairs.
pub(super) fn extract_attachments(raw: &[u8]) -> Vec<(String, Vec<u8>)> {
    use mail_parser::{MessageParser, MimeHeaders, PartType};

    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, part) in parsed.parts.iter().enumerate() {
        let disposition = part
            .content_disposition()
            .map(|cd| cd.ctype().to_ascii_lowercase());
        let is_attachment =
            disposition.as_deref() == Some("attachment") || part.attachment_name().is_some();
        if !is_attachment {
            continue;
        }
        let name = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("part-{i}"));
        let bytes: Vec<u8> = match &part.body {
            PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
            PartType::Text(t) => t.as_bytes().to_vec(),
            _ => continue,
        };
        out.push((name, bytes));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stashed_source_select_populates_slot() {
        let items = vec![
            ("alpha".to_string(), "val_alpha".to_string()),
            ("beta".to_string(), "val_beta".to_string()),
        ];
        let source = StashedSource::new("test", items);
        let slot = Arc::clone(&source.last_picked);

        // Nothing yet.
        assert!(slot.lock().unwrap().is_none());

        // Select first item.
        let action = source.select(0);
        assert!(matches!(action, PickerAction::None));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("val_alpha"));

        // Select second item overwrites.
        source.select(1);
        assert_eq!(slot.lock().unwrap().as_deref(), Some("val_beta"));
    }

    #[test]
    fn stashed_source_item_count_and_labels() {
        let source: StashedSource<i64> =
            StashedSource::new("nums", vec![("one".to_string(), 1), ("two".to_string(), 2)]);
        assert_eq!(source.item_count(), 2);
        assert_eq!(source.label(0), "one");
        assert_eq!(source.match_text(1), "two");
        assert_eq!(source.title(), "nums");
        assert!(!source.has_preview());
    }
}
