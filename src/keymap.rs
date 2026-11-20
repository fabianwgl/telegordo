//! Key strings like `"ctrl+shift+e"` → matchers, and per-pane action maps.
//!
//! Defaults follow discordo. Terminals without the kitty keyboard protocol
//! cannot tell `ctrl+i` from Tab, `ctrl+h` from Backspace or `shift+enter`
//! from Enter, so those bindings get `alt+` fallbacks there.

use std::collections::BTreeMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// One or more key strings from the config.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KeyList(pub Vec<String>);

impl<'de> Deserialize<'de> for KeyList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::One(s) => KeyList(vec![s]),
            Raw::Many(v) => KeyList(v),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeySpec {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // "+" alone, or a trailing "+" like "ctrl++", means the plus key.
        let (mod_part, key_part) = if s == "+" {
            ("", "+")
        } else if let Some(stripped) = s.strip_suffix("++") {
            (stripped, "+")
        } else {
            match s.rfind('+') {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => ("", s),
            }
        };
        let mut mods = KeyModifiers::NONE;
        for m in mod_part.split('+').filter(|m| !m.is_empty()) {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" | "opt" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "cmd" => KeyModifiers::SUPER,
                _ => return None,
            };
        }
        let lower = key_part.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" | "pageup" => KeyCode::PageUp,
            "pgdn" | "pagedown" => KeyCode::PageDown,
            "space" => KeyCode::Char(' '),
            f if f.len() >= 2
                && f.starts_with('f')
                && f[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                KeyCode::F(f[1..].parse().ok()?)
            }
            _ => {
                let mut chars = key_part.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // Modified letters are matched case-insensitively ("ctrl+K" == "ctrl+k"),
                // bare letters are case-sensitive ("G" is shift+g).
                if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    KeyCode::Char(c.to_ascii_lowercase())
                } else {
                    KeyCode::Char(c)
                }
            }
        };
        // "shift+tab" is what terminals send as BackTab.
        if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
            return Some(Self {
                code: KeyCode::BackTab,
                mods: mods - KeyModifiers::SHIFT,
            });
        }
        Some(Self { code, mods })
    }

    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        match (self.code, ev.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => {
                let want = self.mods & relevant;
                let got = ev.modifiers & relevant;
                if want != got {
                    return false;
                }
                if want.is_empty() {
                    // Shift is implied by the character itself.
                    if self.mods.contains(KeyModifiers::SHIFT) && a.is_ascii_alphabetic() {
                        a.to_ascii_uppercase() == b
                    } else {
                        a == b
                    }
                } else {
                    a.eq_ignore_ascii_case(&b)
                }
            }
            (KeyCode::BackTab, KeyCode::BackTab) => {
                (self.mods & relevant) == (ev.modifiers & relevant)
            }
            (a, b) => a == b && self.mods == ev.modifiers,
        }
    }
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl+")?;
        }
        if self.mods.contains(KeyModifiers::ALT) {
            f.write_str("alt+")?;
        }
        if self.mods.contains(KeyModifiers::SUPER) {
            f.write_str("super+")?;
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            f.write_str("shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::BackTab => f.write_str("shift+tab"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pgup"),
            KeyCode::PageDown => f.write_str("pgdn"),
            KeyCode::F(n) => write!(f, "f{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

macro_rules! actions {
    ($name:ident { $($variant:ident = $key:literal => [$($default:literal),*] / [$($legacy:literal),*] : $help:literal),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),* }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            pub fn name(self) -> &'static str {
                match self { $($name::$variant => $key),* }
            }

            pub fn help(self) -> &'static str {
                match self { $($name::$variant => $help),* }
            }

            fn defaults(self, enhanced: bool) -> &'static [&'static str] {
                match self {
                    $($name::$variant => {
                        let legacy: &'static [&'static str] = &[$($legacy),*];
                        if !enhanced && !legacy.is_empty() { legacy } else { &[$($default),*] }
                    }),*
                }
            }
        }
    };
}

// `[enhanced keys] / [legacy-terminal keys]`. Empty legacy list = same as enhanced.
actions!(GlobalAction {
    FocusChats = "focus_chats" => ["ctrl+g"] / [] : "chats",
    FocusMessages = "focus_messages" => ["ctrl+t"] / [] : "messages",
    FocusComposer = "focus_composer" => ["ctrl+i"] / ["alt+i"] : "composer",
    FocusPrevious = "focus_previous" => ["ctrl+h"] / ["alt+h"] : "focus prev",
    FocusNext = "focus_next" => ["ctrl+l"] / ["alt+l"] : "focus next",
    ToggleSidebar = "toggle_sidebar" => ["alt+g"] / [] : "toggle sidebar",
    Picker = "picker" => ["ctrl+k"] / [] : "picker",
    ToggleHelp = "toggle_help" => ["alt+."] / [] : "help",
    Suspend = "suspend" => ["ctrl+z"] / [] : "suspend",
    Logout = "logout" => ["ctrl+d"] / [] : "logout",
    Quit = "quit" => ["ctrl+c"] / [] : "quit",
});

actions!(TreeAction {
    Up = "up" => ["k", "up"] / [] : "up",
    Down = "down" => ["j", "down"] / [] : "down",
    Top = "top" => ["g", "home"] / [] : "top",
    Bottom = "bottom" => ["G", "end"] / [] : "bottom",
    PageUp = "page_up" => ["pgup"] / [] : "page up",
    PageDown = "page_down" => ["pgdn"] / [] : "page down",
    Select = "select" => ["enter", "l", "right"] / [] : "open/expand",
    CollapseParent = "collapse_parent" => ["-", "h", "left"] / [] : "collapse parent",
    CollapseAll = "collapse_all" => ["_"] / [] : "collapse all",
    MoveToParent = "move_to_parent" => ["p"] / [] : "parent",
    NextUnread = "next_unread" => ["J"] / [] : "next unread",
    PrevUnread = "prev_unread" => ["K"] / [] : "prev unread",
    YankId = "yank_id" => ["i"] / [] : "yank id",
});

actions!(MessagesAction {
    Up = "up" => ["k", "up"] / [] : "up",
    Down = "down" => ["j", "down"] / [] : "down",
    Top = "top" => ["g", "home"] / [] : "top",
    Bottom = "bottom" => ["G", "end"] / [] : "bottom",
    ScrollUp = "scroll_up" => ["K", "pgup"] / [] : "scroll up",
    ScrollDown = "scroll_down" => ["J", "pgdn"] / [] : "scroll down",
    SelectReply = "select_reply" => ["s"] / [] : "go to replied",
    Reply = "reply" => ["R", "r"] / [] : "reply",
    Edit = "edit" => ["e"] / [] : "edit",
    Delete = "delete" => ["d"] / [] : "delete",
    Open = "open" => ["o"] / [] : "open attachment",
    Download = "download" => ["D"] / [] : "download",
    YankText = "yank_text" => ["y"] / [] : "yank text",
    YankUrl = "yank_url" => ["u"] / [] : "yank url",
    YankId = "yank_id" => ["i"] / [] : "yank id",
    Cancel = "cancel" => ["esc"] / [] : "unselect",
});

actions!(ComposerAction {
    Send = "send" => ["enter"] / [] : "send",
    Newline = "newline" => ["shift+enter", "alt+enter"] / ["alt+enter"] : "newline",
    Cancel = "cancel" => ["esc"] / [] : "cancel",
    EditLast = "edit_last" => ["up"] / [] : "edit last",
    OpenEditor = "open_editor" => ["ctrl+e"] / [] : "$EDITOR",
});

actions!(PickerAction {
    Up = "up" => ["ctrl+p", "up"] / [] : "up",
    Down = "down" => ["ctrl+n", "down"] / [] : "down",
    Select = "select" => ["enter"] / [] : "select",
    Cancel = "cancel" => ["esc"] / [] : "cancel",
});

/// Bindings for one pane.
#[derive(Debug, Clone)]
pub struct Bindings<A> {
    pub list: Vec<(A, Vec<KeySpec>)>,
}

impl<A: Copy + PartialEq> Bindings<A> {
    pub fn lookup(&self, ev: &KeyEvent) -> Option<A> {
        self.list
            .iter()
            .find(|(_, keys)| keys.iter().any(|k| k.matches(ev)))
            .map(|(a, _)| *a)
    }

    pub fn keys(&self, action: A) -> &[KeySpec] {
        self.list
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, k)| k.as_slice())
            .unwrap_or(&[])
    }

    /// First key, for the help bar.
    pub fn label(&self, action: A) -> String {
        self.keys(action)
            .first()
            .map(|k| k.to_string())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Keymap {
    pub enhanced: bool,
    pub global: Bindings<GlobalAction>,
    pub tree: Bindings<TreeAction>,
    pub messages: Bindings<MessagesAction>,
    pub composer: Bindings<ComposerAction>,
    pub picker: Bindings<PickerAction>,
}

macro_rules! build_bindings {
    ($ty:ident, $overrides:expr, $enhanced:expr, $warnings:expr, $pane:literal) => {{
        let overrides: &BTreeMap<String, KeyList> = $overrides;
        for name in overrides.keys() {
            if !$ty::ALL.iter().any(|a| a.name() == name) {
                $warnings.push(format!("unknown action keys.{}.{}", $pane, name));
            }
        }
        let list = $ty::ALL
            .iter()
            .map(|a| {
                let strings: Vec<String> = match overrides.get(a.name()) {
                    Some(KeyList(v)) => v.clone(),
                    None => a
                        .defaults($enhanced)
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                };
                let keys = strings
                    .iter()
                    .filter_map(|s| {
                        let k = KeySpec::parse(s);
                        if k.is_none() {
                            $warnings.push(format!(
                                "bad key {:?} for keys.{}.{}",
                                s,
                                $pane,
                                a.name()
                            ));
                        }
                        k
                    })
                    .collect();
                (*a, keys)
            })
            .collect();
        Bindings { list }
    }};
}

impl Keymap {
    /// Returns the keymap and any config warnings.
    pub fn build(cfg: &crate::config::KeysConfig, enhanced: bool) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let km = Keymap {
            enhanced,
            global: build_bindings!(GlobalAction, &cfg.global, enhanced, warnings, "global"),
            tree: build_bindings!(TreeAction, &cfg.tree, enhanced, warnings, "tree"),
            messages: build_bindings!(
                MessagesAction,
                &cfg.messages,
                enhanced,
                warnings,
                "messages"
            ),
            composer: build_bindings!(
                ComposerAction,
                &cfg.composer,
                enhanced,
                warnings,
                "composer"
            ),
            picker: build_bindings!(PickerAction, &cfg.picker, enhanced, warnings, "picker"),
        };
        (km, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_modifiers_and_named_keys() {
        let k = KeySpec::parse("ctrl+shift+e").unwrap();
        assert_eq!(k.code, KeyCode::Char('e'));
        assert_eq!(k.mods, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(KeySpec::parse("pgdn").unwrap().code, KeyCode::PageDown);
        assert_eq!(KeySpec::parse("alt+.").unwrap().code, KeyCode::Char('.'));
        assert_eq!(KeySpec::parse("f5").unwrap().code, KeyCode::F(5));
        assert_eq!(KeySpec::parse("shift+tab").unwrap().code, KeyCode::BackTab);
        assert_eq!(KeySpec::parse("ctrl++").unwrap().code, KeyCode::Char('+'));
        assert!(KeySpec::parse("hyper+x").is_none());
        assert!(KeySpec::parse("ab").is_none());
    }

    #[test]
    fn bare_letters_are_case_sensitive() {
        let g = KeySpec::parse("g").unwrap();
        let big_g = KeySpec::parse("G").unwrap();
        assert!(g.matches(&ev(KeyCode::Char('g'), KeyModifiers::NONE)));
        assert!(!g.matches(&ev(KeyCode::Char('G'), KeyModifiers::SHIFT)));
        assert!(big_g.matches(&ev(KeyCode::Char('G'), KeyModifiers::SHIFT)));
        assert!(big_g.matches(&ev(KeyCode::Char('G'), KeyModifiers::NONE)));
    }

    #[test]
    fn ctrl_letters_ignore_case_and_shift_enter_is_distinct() {
        let k = KeySpec::parse("ctrl+k").unwrap();
        assert!(k.matches(&ev(KeyCode::Char('k'), KeyModifiers::CONTROL)));
        assert!(!k.matches(&ev(KeyCode::Char('k'), KeyModifiers::NONE)));
        let se = KeySpec::parse("shift+enter").unwrap();
        assert!(se.matches(&ev(KeyCode::Enter, KeyModifiers::SHIFT)));
        assert!(!se.matches(&ev(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn legacy_terminals_get_alt_fallbacks() {
        let cfg = crate::config::KeysConfig::default();
        let (enhanced, _) = Keymap::build(&cfg, true);
        let (legacy, _) = Keymap::build(&cfg, false);
        assert_eq!(enhanced.global.label(GlobalAction::FocusComposer), "ctrl+i");
        assert_eq!(legacy.global.label(GlobalAction::FocusComposer), "alt+i");
        // Backspace must never move focus in a legacy terminal.
        let bs = ev(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(legacy.global.lookup(&bs), None);
    }

    #[test]
    fn overrides_replace_defaults_and_warn_on_unknown() {
        let mut cfg = crate::config::KeysConfig::default();
        cfg.global
            .insert("quit".into(), KeyList(vec!["ctrl+q".into()]));
        cfg.global.insert("nope".into(), KeyList(vec!["x".into()]));
        let (km, warnings) = Keymap::build(&cfg, true);
        assert_eq!(km.global.label(GlobalAction::Quit), "ctrl+q");
        assert!(
            km.global
                .lookup(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(warnings.len(), 1);
    }
}
