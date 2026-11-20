//! User configuration, loaded from `config.toml`.
//!
//! Every field has a default, so a missing file or a partial file both work,
//! the same way discordo treats its config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::keymap::KeyList;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub mouse: bool,
    /// `"default"` uses `$VISUAL`, then `$EDITOR`, then `vi`.
    pub editor: String,
    pub timestamps: Timestamps,
    pub date_separator: DateSeparator,
    pub markdown: Markdown,
    pub notifications: Notifications,
    pub typing_indicator: TypingIndicator,
    /// Messages fetched per history page (1..=100).
    pub messages_limit: i32,
    pub sidebar: Sidebar,
    pub images: Images,
    pub keys: KeysConfig,
    pub theme: Theme,
    /// Problems found while loading: unknown keys, bad values.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telegram: TelegramConfig::default(),
            mouse: true,
            editor: "default".into(),
            timestamps: Timestamps::default(),
            date_separator: DateSeparator::default(),
            markdown: Markdown::default(),
            notifications: Notifications::default(),
            typing_indicator: TypingIndicator::default(),
            messages_limit: 50,
            sidebar: Sidebar::default(),
            images: Images::default(),
            keys: KeysConfig::default(),
            theme: Theme::default(),
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TelegramConfig {
    /// From https://my.telegram.org. `TELEGORDO_API_ID` overrides it.
    pub api_id: Option<i32>,
    /// From https://my.telegram.org. `TELEGORDO_API_HASH` overrides it.
    pub api_hash: Option<String>,
    /// Use Telegram's test data centers.
    pub use_test_dc: bool,
    /// Keep the session database key in the OS keyring. `false` stores it in
    /// a 0600 file in the data dir (no keyring, or unsigned dev builds that
    /// trigger a Keychain prompt after every rebuild).
    pub keyring: bool,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            api_id: None,
            api_hash: None,
            use_test_dc: false,
            keyring: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Timestamps {
    pub enabled: bool,
    /// chrono `strftime` format.
    pub format: String,
}

impl Default for Timestamps {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "%-I:%M%p".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DateSeparator {
    pub enabled: bool,
    pub format: String,
}

impl Default for DateSeparator {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "%B %-d, %Y".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Markdown {
    /// Render entities and parse outgoing markdown.
    pub enabled: bool,
    pub mask_spoilers: bool,
    /// syntect theme for code blocks.
    pub theme: String,
}

impl Default for Markdown {
    fn default() -> Self {
        Self {
            enabled: true,
            mask_spoilers: true,
            theme: "base16-ocean.dark".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    /// Only notify while the terminal window is unfocused.
    pub when_unfocused: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            when_unfocused: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TypingIndicator {
    pub send: bool,
    pub receive: bool,
}

impl Default for TypingIndicator {
    fn default() -> Self {
        Self {
            send: true,
            receive: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Sidebar {
    pub width_percent: u16,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self { width_percent: 20 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Images {
    pub enabled: bool,
    /// `"auto"`, `"kitty"`, `"iterm2"`, `"sixel"` or `"halfblocks"`.
    pub protocol: String,
    pub max_width: u16,
    pub max_height: u16,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol: "auto".into(),
            max_width: 40,
            max_height: 12,
        }
    }
}

/// `[keys.<pane>] action = "key"` or `action = ["key", "key"]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct KeysConfig {
    pub global: BTreeMap<String, KeyList>,
    pub tree: BTreeMap<String, KeyList>,
    pub messages: BTreeMap<String, KeyList>,
    pub composer: BTreeMap<String, KeyList>,
    pub picker: BTreeMap<String, KeyList>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Theme {
    /// `"round"`, `"plain"`, `"thick"`, `"double"` or `"hidden"`.
    pub border: String,
    pub active_border_color: String,
    pub inactive_border_color: String,
    pub title_color: String,
    pub selected_bg: String,
    pub timestamp_color: String,
    pub unread_color: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border: "round".into(),
            active_border_color: "green".into(),
            inactive_border_color: "darkgray".into(),
            title_color: "reset".into(),
            selected_bg: "#3a3a3a".into(),
            timestamp_color: "darkgray".into(),
            unread_color: "yellow".into(),
        }
    }
}

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "telegordo").context("cannot find home directory")
}

/// Config locations, in lookup order: `$XDG_CONFIG_HOME/telegordo`,
/// `~/.config/telegordo` (also on macOS), then the platform dir
/// (macOS: `~/Library/Application Support/telegordo`).
pub fn config_candidates() -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        v.push(PathBuf::from(x).join("telegordo/config.toml"));
    }
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        v.push(home.join(".config/telegordo/config.toml"));
    }
    v.push(project_dirs()?.config_dir().join("config.toml"));
    // XDG_CONFIG_HOME is often ~/.config: drop repeats, keep the order.
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    Ok(v)
}

/// First existing candidate, else the platform default.
pub fn default_config_path() -> Result<PathBuf> {
    let c = config_candidates()?;
    Ok(c.iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| c.last().cloned().expect("non-empty")))
}

impl Config {
    /// Loads `path`. A missing file gives the defaults, unless `explicit`
    /// (given with `--config-path`), where it is an error.
    pub fn load(path: &Path, explicit: bool) -> Result<Self> {
        if !path.exists() {
            if explicit {
                anyhow::bail!("config file {} does not exist", path.display());
            }
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.messages_limit = cfg.messages_limit.clamp(1, 100);
        let raw: toml::Table = toml::from_str(text)?;
        let mut warnings = Vec::new();
        unknown_keys(&raw, &schema(), "", &mut warnings);
        warnings.extend(cfg.bad_values());
        cfg.warnings = warnings;
        Ok(cfg)
    }

    /// Values that parse as strings but mean nothing: colors, border, protocol.
    fn bad_values(&self) -> Vec<String> {
        use std::str::FromStr;
        let t = &self.theme;
        let mut out = Vec::new();
        for (name, value) in [
            ("active_border_color", &t.active_border_color),
            ("inactive_border_color", &t.inactive_border_color),
            ("title_color", &t.title_color),
            ("selected_bg", &t.selected_bg),
            ("timestamp_color", &t.timestamp_color),
            ("unread_color", &t.unread_color),
        ] {
            if ratatui::style::Color::from_str(value).is_err() {
                out.push(format!("theme.{name}: unknown color {value:?}"));
            }
        }
        if !BORDERS.contains(&t.border.as_str()) {
            out.push(format!(
                "theme.border: unknown border {:?} (use {})",
                t.border,
                BORDERS.join(", ")
            ));
        }
        let protocol = self.images.protocol.to_ascii_lowercase();
        if !PROTOCOLS.contains(&protocol.as_str()) {
            out.push(format!(
                "images.protocol: unknown protocol {:?} (use {})",
                self.images.protocol,
                PROTOCOLS.join(", ")
            ));
        }
        out
    }

    /// api_id/api_hash from env first, then config.
    pub fn api_credentials(&self) -> Option<(i32, String)> {
        let id = std::env::var("TELEGORDO_API_ID")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .or(self.telegram.api_id)?;
        let hash = std::env::var("TELEGORDO_API_HASH")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.telegram.api_hash.clone())?;
        Some((id, hash.trim().to_string()))
    }
}

pub const BORDERS: &[&str] = &["round", "rounded", "plain", "thick", "double", "hidden"];
pub const PROTOCOLS: &[&str] = &["auto", "kitty", "iterm2", "sixel", "halfblocks"];

/// Every known key, as TOML: the defaults, with optional values filled in.
fn schema() -> toml::Table {
    let mut cfg = Config::default();
    cfg.telegram.api_id = Some(0);
    cfg.telegram.api_hash = Some(String::new());
    toml::Table::try_from(cfg).expect("config serializes")
}

/// Warns about keys in `user` that `schema` does not have. The `[keys.*]`
/// tables hold action names, which the keymap checks itself.
fn unknown_keys(user: &toml::Table, schema: &toml::Table, path: &str, out: &mut Vec<String>) {
    for (k, v) in user {
        let full = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        match (schema.get(k), v) {
            (None, _) => out.push(format!("unknown key {full}")),
            (Some(toml::Value::Table(s)), toml::Value::Table(u)) if path != "keys" => {
                unknown_keys(u, s, &full, out)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_and_bad_values_warn() {
        let cfg = Config::parse(
            r##"
            mouse = false
            mosue = true
            [theme]
            border = "wavy"
            selected_bg = "#zzzzzz"
            unread_color = "yellow"
            [images]
            protocol = "png"
            [telegram]
            api_id = 1
            api_hash = "x"
            [keys.messages]
            reply = "r"
            [nope]
            x = 1
            "##,
        )
        .unwrap();
        let w = cfg.warnings.join("\n");
        assert!(w.contains("unknown key mosue"), "{w}");
        assert!(w.contains("unknown key nope"), "{w}");
        assert!(w.contains("theme.border: unknown border \"wavy\""), "{w}");
        assert!(
            w.contains("theme.selected_bg: unknown color \"#zzzzzz\""),
            "{w}"
        );
        assert!(w.contains("images.protocol"), "{w}");
        assert!(!w.contains("unread_color") && !w.contains("api_id") && !w.contains("reply"));
        assert_eq!(cfg.warnings.len(), 5, "{w}");
        assert!(
            Config::parse("").unwrap().warnings.is_empty(),
            "defaults are valid"
        );
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let p = Path::new("/nonexistent/telegordo.toml");
        assert!(Config::load(p, false).is_ok());
        let e = Config::load(p, true).unwrap_err().to_string();
        assert!(e.contains("does not exist"), "{e}");
    }

    #[test]
    fn empty_file_gives_defaults() {
        let cfg = Config::parse("").unwrap();
        assert!(cfg.mouse);
        assert_eq!(cfg.messages_limit, 50);
        assert_eq!(cfg.theme.border, "round");
        assert!(cfg.markdown.enabled);
    }

    #[test]
    fn partial_file_merges_with_defaults() {
        let cfg = Config::parse(
            r#"
            mouse = false
            messages_limit = 500
            [timestamps]
            format = "%H:%M"
            [keys.global]
            quit = ["ctrl+q", "ctrl+c"]
            "#,
        )
        .unwrap();
        assert!(!cfg.mouse);
        assert_eq!(cfg.messages_limit, 100, "clamped");
        assert_eq!(cfg.timestamps.format, "%H:%M");
        assert!(cfg.timestamps.enabled, "untouched field keeps default");
        assert_eq!(cfg.keys.global["quit"].0, vec!["ctrl+q", "ctrl+c"]);
    }
}
