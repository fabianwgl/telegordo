//! Message composer: multi-line textarea with reply/edit modes.

use std::time::Instant;

use ratatui_textarea::{TextArea, WrapMode};
use tdlib_rs::enums::TextEntityType;
use tdlib_rs::types::FormattedText;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Reply(i64),
    Edit(i64),
}

/// The message being edited, as it was before the edit.
#[derive(Debug, Clone, PartialEq)]
pub struct EditOrigin {
    pub original: FormattedText,
    /// What the composer started with; sending it unchanged is a no-op.
    pub initial: String,
    /// Formatting the composer text cannot carry, e.g. "underline".
    pub lossy: Vec<&'static str>,
    /// The user was already warned about `lossy` once.
    pub warned: bool,
}

/// Everything needed to put a failed send back into the composer.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub text: String,
    pub mode: Mode,
    pub edit: Option<EditOrigin>,
}

pub struct Composer {
    pub ta: TextArea<'static>,
    pub mode: Mode,
    /// Set while `mode` is `Edit` and the original text is known.
    pub edit: Option<EditOrigin>,
    pub last_typing_sent: Option<Instant>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            ta: new_area(),
            mode: Mode::Normal,
            edit: None,
            last_typing_sent: None,
        }
    }
}

/// User-visible name of formatting that has to be set explicitly, or None
/// for entities Telegram detects by itself (links, @mentions, hashtags…).
pub fn formatting_name(t: &TextEntityType) -> Option<&'static str> {
    Some(match t {
        TextEntityType::Bold => "bold",
        TextEntityType::Italic => "italic",
        TextEntityType::Underline => "underline",
        TextEntityType::Strikethrough => "strikethrough",
        TextEntityType::Spoiler => "spoilers",
        TextEntityType::Code | TextEntityType::Pre | TextEntityType::PreCode(_) => "code",
        TextEntityType::TextUrl(_) => "links",
        TextEntityType::MentionName(_) => "mentions",
        TextEntityType::CustomEmoji(_) => "custom emoji",
        TextEntityType::BlockQuote | TextEntityType::ExpandableBlockQuote => "quotes",
        _ => return None,
    })
}

/// Names of the formatting in `ft`, deduplicated, in first-seen order.
pub fn formatting_names(ft: &FormattedText) -> Vec<&'static str> {
    let mut out = Vec::new();
    for e in &ft.entities {
        if let Some(n) = formatting_name(&e.r#type)
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

fn new_area() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_wrap_mode(WrapMode::WordOrGlyph);
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta.set_placeholder_text("Message…");
    ta
}

impl Composer {
    pub fn text(&self) -> String {
        self.ta.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.ta.is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        let mut ta = new_area();
        ta.insert_str(text);
        self.ta = ta;
    }

    pub fn clear(&mut self) {
        self.ta = new_area();
        self.mode = Mode::Normal;
        self.edit = None;
    }

    pub fn draft(&self) -> Draft {
        Draft {
            text: self.text(),
            mode: self.mode,
            edit: self.edit.clone(),
        }
    }

    pub fn restore(&mut self, d: Draft) {
        self.set_text(&d.text);
        self.mode = d.mode;
        self.edit = d.edit;
    }

    /// Rows the composer wants, borders included: 1..=10 lines of text.
    pub fn height(&self, width: u16) -> u16 {
        let inner = width.saturating_sub(2).max(1) as usize;
        let rows: usize = self
            .ta
            .lines()
            .iter()
            .map(|l| {
                unicode_width::UnicodeWidthStr::width(l.as_str())
                    .div_ceil(inner)
                    .max(1)
            })
            .sum();
        rows.clamp(1, 10) as u16 + 2
    }

    /// True when a typing action should be sent now (at most every 5 s).
    pub fn should_send_typing(&mut self) -> bool {
        let now = Instant::now();
        match self.last_typing_sent {
            Some(t) if now.duration_since(t).as_secs() < 5 => false,
            _ => {
                self.last_typing_sent = Some(now);
                true
            }
        }
    }
}
