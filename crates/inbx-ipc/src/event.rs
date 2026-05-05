//! IPC event types exchanged between `inbx-sync` and `inbx` TUI.

/// Events broadcast by the sync daemon over the unix socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sent on connect so the client knows the daemon version.
    Hello { version: String },
    /// Sync cycle finished for this account+folder; client should reload.
    FolderUpdated {
        account: String,
        folder: String,
        new_count: u32,
    },
    /// Periodic heartbeat (every 60s) so client can detect dead daemon.
    Heartbeat { ts_unix: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_hello() {
        let ev = Event::Hello {
            version: "0.4.0".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Event::Hello { version } if version == "0.4.0"));
    }

    #[test]
    fn round_trip_folder_updated() {
        let ev = Event::FolderUpdated {
            account: "work".into(),
            folder: "INBOX".into(),
            new_count: 3,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Event::FolderUpdated { account, folder, new_count }
                if account == "work" && folder == "INBOX" && new_count == 3)
        );
    }

    #[test]
    fn round_trip_heartbeat() {
        let ev = Event::Heartbeat {
            ts_unix: 1_700_000_000,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Event::Heartbeat { ts_unix } if ts_unix == 1_700_000_000));
    }
}
