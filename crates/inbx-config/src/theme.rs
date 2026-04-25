//! User-facing theme. Loaded from `$XDG_CONFIG_HOME/inbx/theme.toml`
//! when present; otherwise the built-in dark palette is used.
//!
//! Colors are stored as RGB tuples so this crate stays UI-agnostic;
//! both the ratatui TUI and the egui GUI map them to their own
//! native types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Border color when a pane / field is focused.
    #[serde(default = "default_focused")]
    pub focused: Rgb,
    /// Border color when a pane / field is not focused.
    #[serde(default = "default_unfocused")]
    pub unfocused: Rgb,
    /// Status bar background.
    #[serde(default = "default_status_bg")]
    pub status_bg: Rgb,
    /// Status bar foreground.
    #[serde(default = "default_status_fg")]
    pub status_fg: Rgb,
    /// Unread message accent.
    #[serde(default = "default_unread")]
    pub unread: Rgb,
    /// Generic highlight (selected row).
    #[serde(default = "default_highlight")]
    pub highlight: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            focused: default_focused(),
            unfocused: default_unfocused(),
            status_bg: default_status_bg(),
            status_fg: default_status_fg(),
            unread: default_unread(),
            highlight: default_highlight(),
        }
    }
}

fn default_focused() -> Rgb {
    Rgb::new(255, 215, 0) // gold
}
fn default_unfocused() -> Rgb {
    Rgb::new(85, 85, 85) // dark gray
}
fn default_status_bg() -> Rgb {
    Rgb::new(40, 40, 40)
}
fn default_status_fg() -> Rgb {
    Rgb::new(220, 220, 220)
}
fn default_unread() -> Rgb {
    Rgb::new(135, 175, 215)
}
fn default_highlight() -> Rgb {
    Rgb::new(75, 100, 130)
}

pub fn theme_path() -> super::Result<PathBuf> {
    Ok(super::project_dirs()?.config_dir().join("theme.toml"))
}

pub fn load_theme() -> super::Result<Theme> {
    let path = theme_path()?;
    if !path.exists() {
        return Ok(Theme::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let theme = Theme::default();
        let s = toml::to_string(&theme).unwrap();
        let parsed: Theme = toml::from_str(&s).unwrap();
        assert_eq!(parsed.focused, theme.focused);
    }

    #[test]
    fn partial_overrides() {
        let raw = r#"
focused = [255, 0, 0]
"#;
        let parsed: Theme = toml::from_str(raw).unwrap();
        assert_eq!(parsed.focused, Rgb::new(255, 0, 0));
        // Other fields fall back to defaults.
        assert_eq!(parsed.unfocused, default_unfocused());
    }
}
