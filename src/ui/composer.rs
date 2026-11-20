//! Message composer: multi-line textarea with reply/edit modes.

use std::time::Instant;

use ratatui_textarea::{TextArea, WrapMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Reply(i64),
    Edit(i64),
}

pub struct Composer {
    pub ta: TextArea<'static>,
    pub mode: Mode,
    pub last_typing_sent: Option<Instant>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            ta: new_area(),
            mode: Mode::Normal,
            last_typing_sent: None,
        }
    }
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
