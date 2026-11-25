//! GUI-only preferences (`<data-dir>/gui.json`).
//!
//! These are deliberately separate from `config.toml`: theme/font choices are
//! not kernel configuration and must not trigger a kernel restart.

use std::path::{Path, PathBuf};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Pixels, px};
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
    /// Whether the sidebar is expanded. `false` is the icon rail (ADR-0063 §1).
    pub sidebar_expanded: bool,
    /// 我的任务段是否展开（与定时任务段独立折叠）。
    pub tasks_section_open: bool,
    /// 定时任务段是否展开（与我的任务段独立折叠）。
    pub schedule_section_open: bool,
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
            sidebar_expanded: true,
            tasks_section_open: true,
            schedule_section_open: true,
        }
    }
}

/// Whether `family` resolves to an effectively monospaced face.
///
/// GPUI does not fail when a family is unavailable: it resolves to the first
/// entry of its fallback stack instead (`.ZedMono` → `.ZedSans` → Helvetica →
/// Segoe UI → …). On Windows the default code font, JetBrains Mono, is usually
/// not installed, so the terminal's Latin face silently lands on Segoe UI — a
/// *proportional* font. A terminal grid is built on one cell width (`advance`),
/// so a proportional face makes every cell wider than its glyphs: differently
/// coloured runs and the cursor land far to the right of the text, and the
/// shell is told fewer columns than the view can hold, wrapping lines early.
/// Measuring 'M' against 'i' catches it: a mono face reports the same advance
/// for both, a proportional face differs by a wide margin.
fn is_monospace(text_system: &gpui_kit::TextSystem, family: &str, size: Pixels) -> bool {
    let font = gpui_kit::Font {
        family: family.into(),
        features: Default::default(),
        fallbacks: None,
        weight: gpui_kit::FontWeight::NORMAL,
        style: gpui_kit::FontStyle::Normal,
    };
    let id = text_system.resolve_font(&font);
    let (Ok(m), Ok(i)) = (
        text_system.advance(id, size, 'M'),
        text_system.advance(id, size, 'i'),
    ) else {
        return false;
    };
    let m = m.width.as_f32();
    let i = i.width.as_f32();
    m > 0.0 && (m - i).abs() / m <= 0.1
}

/// The monospace family every code surface draws with, guaranteed monospaced.
///
/// `code_font` is the user's choice and wins when it actually resolves to a
/// monospaced face. When it does not — an unavailable font falls back to a
/// proportional one through GPUI — the first platform mono face that measures
/// monospaced replaces it, for the whole window (terminal, transcript and any
/// other code surface share `theme.mono_font_family`).
fn resolve_mono_family(code_font: &str, size: Pixels, cx: &App) -> gpui_kit::SharedString {
    let text_system = cx.text_system();
    if is_monospace(text_system, code_font, size) {
        return code_font.into();
    }
    const MONO_FALLBACKS: &[&str] = &[
        // Windows
        "Cascadia Mono",
        "Consolas",
        "Courier New",
        // macOS
        "Menlo",
        "Monaco",
        // Linux
        "DejaVu Sans Mono",
        "Liberation Mono",
        "monospace",
    ];
    MONO_FALLBACKS
        .iter()
        .copied()
        .find(|candidate| is_monospace(text_system, candidate, size))
        .map(Into::into)
        .unwrap_or_else(|| code_font.into())
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

    /// Read-modify-write one preference, keeping every field this writer does
    /// not own at its on-disk value.
    ///
    /// Several windows persist into the same `gui.json`: the main window owns
    /// `last_session` and the window geometry, the settings window owns the
    /// appearance fields. Saving a whole in-memory copy from one of them
    /// resurrects whatever the others had changed in the meantime — most
    /// visibly `last_session`, which then reopens an older task (or none) on
    /// the next launch.
    pub fn update(data_dir: &Path, apply: impl FnOnce(&mut Self)) -> std::io::Result<()> {
        let mut prefs = Self::load(data_dir);
        apply(&mut prefs);
        prefs.save(data_dir)
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
        // `code_font` 缺失时 GPUI 会静默回退到比例字体（Windows 上通常是
        // Segoe UI），终端的格子几何建立在等宽 advance 上，比例字体一进来
        // 整个网格就错位。这里把 mono 族换成**实测等宽**的字体；用户显式
        // 选择的字体解析后确实等宽时保持原样。
        let mono_family = resolve_mono_family(&self.code_font, font_size, cx);
        Theme::update(cx, |theme| {
            theme.font_size = font_size;
            theme.mono_font_size = font_size;
            theme.mono_font_family = mono_family;
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

        let prefs = GuiPreferences {
            send_mode: "cmd_enter".to_string(),
            ..GuiPreferences::default()
        };
        assert_eq!(prefs.normalized().send_mode, "cmd_enter");

        let prefs = GuiPreferences {
            send_mode: "tab".to_string(),
            ..GuiPreferences::default()
        };
        assert_eq!(prefs.normalized().send_mode, "enter");
    }
}
