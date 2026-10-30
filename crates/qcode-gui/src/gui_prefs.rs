//! GUI-only preferences (`<data-dir>/gui.json`).
//!
//! These are deliberately separate from `config.toml`: theme/font choices are
//! not kernel configuration and must not trigger a kernel restart.

use std::path::{Path, PathBuf};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, px};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Default base UI font size in px (see `GuiPreferences::default`).
pub const DEFAULT_FONT_SIZE: u32 = 14;

/// Smallest accepted base UI font size (px); the zoom-out shortcut clamps here.
pub const FONT_SIZE_MIN: u32 = 10;

/// Largest accepted base UI font size (px); the zoom-in shortcut clamps here.
pub const FONT_SIZE_MAX: u32 = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPreferences {
    pub theme: String,
    pub font_size: u32,
    pub code_font: String,
    pub density: String,
    /// Most-recent first; absolute paths. Capped to 10.
    pub recent_workspaces: Vec<String>,
    /// Session to reopen on the next launch, when it still exists.
    pub last_session: Option<String>,
    /// Composer keyboard: `enter` (Enter sends, the shipped default) or
    /// `cmd_enter` (Cmd/Ctrl+Enter sends, Enter inserts a newline).
    pub send_mode: String,
    pub window_geometry: Option<WindowGeometry>,
}

impl Default for GuiPreferences {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            font_size: DEFAULT_FONT_SIZE,
            // JetBrains Mono is the font repo's preferred code face
            // (D:\workspace\fonts); SF Mono is not part of that set.
            code_font: "JetBrains Mono".to_string(),
            density: "comfortable".to_string(),
            recent_workspaces: Vec::new(),
            last_session: None,
            send_mode: "enter".to_string(),
            window_geometry: None,
        }
    }
}

impl GuiPreferences {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("gui.json")
    }

    /// Load preferences; a missing or malformed file falls back to defaults.
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&text)
            .map(Self::normalized)
            .unwrap_or_default()
    }

    /// Atomically persist preferences. UI preferences are not secrets.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let temp = data_dir.join(format!(".gui.json.tmp-{}", std::process::id()));
        let bytes =
            serde_json::to_vec_pretty(&self.clone().normalized()).map_err(std::io::Error::other)?;
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, &path)
    }

    /// Record `workspace` at the front of the recent list.
    pub fn record_workspace(&mut self, workspace: &Path) {
        let path = workspace.display().to_string();
        self.recent_workspaces.retain(|known| known != &path);
        self.recent_workspaces.insert(0, path);
        self.recent_workspaces.truncate(10);
    }

    /// Recent workspaces in most-recent-first order.
    pub fn recent_workspaces(&self) -> impl Iterator<Item = &str> {
        self.recent_workspaces.iter().map(String::as_str)
    }

    /// Apply the appearance to every GPUI window.
    pub fn apply(&self, cx: &mut App) {
        if let Some(mode) = match self.theme.as_str() {
            "light" => Some(ThemeMode::Light),
            "dark" => Some(ThemeMode::Dark),
            _ => None,
        } {
            Theme::change(mode, None, cx);
        }
        let font_size = px(self.font_size as f32);
        Theme::update(cx, |theme| {
            theme.font_size = font_size;
            theme.mono_font_size = font_size;
            theme.mono_font_family = self.code_font.clone().into();
        });
    }

    fn normalized(mut self) -> Self {
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".to_string();
        }
        self.font_size = self.font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        // SF Mono was the old default and is not in the project font set
        // (D:\workspace\fonts); migrate any saved value back to JetBrains Mono.
        if self.code_font.trim().is_empty() || self.code_font == "SF Mono" {
            self.code_font = "JetBrains Mono".to_string();
        }
        if !matches!(self.density.as_str(), "comfortable" | "compact") {
            self.density = "comfortable".to_string();
        }
        if !matches!(self.send_mode.as_str(), "enter" | "cmd_enter") {
            self.send_mode = "enter".to_string();
        }
        let mut seen = std::collections::BTreeSet::new();
        self.recent_workspaces
            .retain(|path| !path.trim().is_empty() && seen.insert(path.clone()));
        self.recent_workspaces.truncate(10);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_mode_defaults_to_enter_and_normalizes_unknown_values() {
        assert_eq!(GuiPreferences::default().send_mode, "enter");

        // An old gui.json without the field falls back to the default.
        let loaded: GuiPreferences = serde_json::from_str("{}").expect("empty preferences");
        assert_eq!(loaded.send_mode, "enter");

        let mut prefs = GuiPreferences::default();
        prefs.send_mode = "cmd_enter".to_string();
        assert_eq!(prefs.clone().normalized().send_mode, "cmd_enter");

        prefs.send_mode = "tab".to_string();
        assert_eq!(prefs.normalized().send_mode, "enter");
    }
}
