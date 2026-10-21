//! GUI-only preferences (`<data-dir>/gui.json`).
//!
//! These are deliberately separate from `config.toml`: theme/font choices are
//! not kernel configuration and must not trigger a kernel restart.

use std::path::{Path, PathBuf};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, px};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPreferences {
    pub theme: String,
    pub font_size: u32,
    pub code_font: String,
    pub density: String,
}

impl Default for GuiPreferences {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            font_size: 14,
            code_font: "SF Mono".to_string(),
            density: "comfortable".to_string(),
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
        self.font_size = self.font_size.clamp(10, 32);
        if self.code_font.trim().is_empty() {
            self.code_font = "SF Mono".to_string();
        }
        if !matches!(self.density.as_str(), "comfortable" | "compact") {
            self.density = "comfortable".to_string();
        }
        self
    }
}
