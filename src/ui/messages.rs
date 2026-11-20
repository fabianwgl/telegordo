//! Message list: lays out messages into lines, keeps scroll and selection.

use chrono::{DateTime, Local, TimeZone};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tdlib_rs::enums::{MessageContent, MessageOrigin, StickerFormat};
use tdlib_rs::types::{FormattedText, Message};
use unicode_width::UnicodeWidthStr;

use super::image::{Images, Wanted};
use super::render_text::{RenderOpts, formatted_spans, wrap};
use crate::config::Config;
use crate::tg::store::Store;

#[derive(Debug, Default)]
pub struct MessagesState {
    pub selected: Option<i64>,
    /// Lines scrolled up from the bottom. 0 = follow new messages.
    pub scroll: usize,
    /// Bring the selection into view on the next draw.
    pub reveal: bool,
    /// Set by the last draw when the top of the loaded history is visible.
    pub at_top: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ImageSlot {
    pub line: usize,
    pub wanted: Wanted,
}

#[derive(Debug, Default)]
pub struct Layout {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<ImageSlot>,
    /// Message id with its first and one-past-last line.
    pub ranges: Vec<(i64, usize, usize)>,
}

const NAME_COLORS: [Color; 7] = [
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::Rgb(255, 165, 90),
];

pub fn name_color(id: i64) -> Color {
    NAME_COLORS[(id.unsigned_abs() % NAME_COLORS.len() as u64) as usize]
}

fn local(ts: i32) -> DateTime<Local> {
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(Local::now)
}

fn duration(secs: i32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// What a message shows besides its text.
pub enum Media<'a> {
    None,
    Photo(&'a tdlib_rs::types::Photo),
    StaticSticker(&'a tdlib_rs::types::Sticker),
    Placeholder(String),
}

/// Media part and text/caption of a message. `None` text = service message.
pub fn parts<'a>(m: &'a Message, store: &Store) -> (Media<'a>, Option<FormattedText>) {
    let plain = |s: String| {
        Some(FormattedText {
            text: s,
            entities: vec![],
        })
    };
    let sender = || store.sender_name(&m.sender_id);
    match &m.content {
        MessageContent::MessageText(t) => (Media::None, Some(t.text.clone())),
        MessageContent::MessagePhoto(p) => (Media::Photo(&p.photo), Some(p.caption.clone())),
        MessageContent::MessageSticker(s) => match s.sticker.format {
            StickerFormat::Webp => (Media::StaticSticker(&s.sticker), plain(String::new())),
            _ => (
                Media::Placeholder(format!("[Sticker {}]", s.sticker.emoji)),
                plain(String::new()),
            ),
        },
        MessageContent::MessageVideo(v) => (
            Media::Placeholder(
                format!(
                    "[Video {} {}",
                    duration(v.video.duration),
                    v.video.file_name
                )
                .trim_end()
                .to_string()
                    + "]",
            ),
            Some(v.caption.clone()),
        ),
        MessageContent::MessageAnimation(a) => {
            (Media::Placeholder("[GIF]".into()), Some(a.caption.clone()))
        }
        MessageContent::MessageVoiceNote(v) => (
            Media::Placeholder(format!("[Voice {}]", duration(v.voice_note.duration))),
            Some(v.caption.clone()),
        ),
        MessageContent::MessageVideoNote(v) => (
            Media::Placeholder(format!(
                "[Video message {}]",
                duration(v.video_note.duration)
            )),
            plain(String::new()),
        ),
        MessageContent::MessageAudio(a) => {
            let title = if a.audio.title.is_empty() {
                a.audio.file_name.clone()
            } else {
                format!("{} – {}", a.audio.performer, a.audio.title)
            };
            (
                Media::Placeholder(format!("[Audio {} {}]", title, duration(a.audio.duration))),
                Some(a.caption.clone()),
            )
        }
        MessageContent::MessageDocument(d) => (
            Media::Placeholder(format!(
                "[File {} {}]",
                d.document.file_name,
                human_size(d.document.document.size)
            )),
            Some(d.caption.clone()),
        ),
        MessageContent::MessageAnimatedEmoji(e) => (Media::None, plain(e.emoji.clone())),
        MessageContent::MessageLocation(_) => (
            Media::Placeholder("[Location]".into()),
            plain(String::new()),
        ),
        MessageContent::MessageVenue(_) => {
            (Media::Placeholder("[Venue]".into()), plain(String::new()))
        }
        MessageContent::MessageContact(c) => (
            Media::Placeholder(format!(
                "[Contact {} {}]",
                c.contact.first_name, c.contact.phone_number
            )),
            plain(String::new()),
        ),
        MessageContent::MessagePoll(p) => (
            Media::Placeholder(format!("[Poll {}]", p.poll.question.text)),
            plain(String::new()),
        ),
        MessageContent::MessageDice(d) => (
            Media::Placeholder(format!("[Dice {}]", d.emoji)),
            plain(String::new()),
        ),
        MessageContent::MessageCall(_) => {
            (Media::Placeholder("[Call]".into()), plain(String::new()))
        }
        MessageContent::MessageStory(_) => {
            (Media::Placeholder("[Story]".into()), plain(String::new()))
        }
        MessageContent::MessageExpiredPhoto => (
            Media::Placeholder("[Expired photo]".into()),
            plain(String::new()),
        ),
        MessageContent::MessageExpiredVideo => (
            Media::Placeholder("[Expired video]".into()),
            plain(String::new()),
        ),
        MessageContent::MessageUnsupported => (
            Media::Placeholder("[Unsupported message]".into()),
            plain(String::new()),
        ),
        MessageContent::MessageChatAddMembers(a) => {
            let names: Vec<String> = a
                .member_user_ids
                .iter()
                .map(|u| store.user_name(*u))
                .collect();
            (
                Media::Placeholder(format!("{} added {}", sender(), names.join(", "))),
                None,
            )
        }
        MessageContent::MessageChatJoinByLink | MessageContent::MessageChatJoinByRequest => {
            (Media::Placeholder(format!("{} joined", sender())), None)
        }
        MessageContent::MessageChatDeleteMember(d) => (
            Media::Placeholder(format!(
                "{} removed {}",
                sender(),
                store.user_name(d.user_id)
            )),
            None,
        ),
        MessageContent::MessageChatChangeTitle(t) => (
            Media::Placeholder(format!("{} renamed the chat to \"{}\"", sender(), t.title)),
            None,
        ),
        MessageContent::MessagePinMessage(_) => (
            Media::Placeholder(format!("{} pinned a message", sender())),
            None,
        ),
        MessageContent::MessageForumTopicCreated(t) => (
            Media::Placeholder(format!("{} created topic \"{}\"", sender(), t.name)),
            None,
        ),
        MessageContent::MessageContactRegistered => (
            Media::Placeholder(format!("{} joined Telegram", sender())),
            None,
        ),
        MessageContent::MessageScreenshotTaken => (
            Media::Placeholder(format!("{} took a screenshot", sender())),
            None,
        ),
        _ => (Media::Placeholder("[service message]".into()), None),
    }
}

/// One-line preview, for replies and notifications.
pub fn snippet(m: &Message, store: &Store) -> String {
    let (media, text) = parts(m, store);
    let text = text.map(|t| t.text).unwrap_or_default();
    let media = match media {
        Media::None => String::new(),
        Media::Photo(_) => "[Photo]".into(),
        Media::StaticSticker(s) => format!("[Sticker {}]", s.emoji),
        Media::Placeholder(p) => p,
    };
    let s = format!("{media} {text}");
    s.trim().replace('\n', " ")
}

fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn origin_name(o: &MessageOrigin, store: &Store) -> String {
    match o {
        MessageOrigin::User(u) => store.user_name(u.sender_user_id),
        MessageOrigin::HiddenUser(h) => h.sender_name.clone(),
        MessageOrigin::Chat(c) => store.chat_title(c.sender_chat_id),
        MessageOrigin::Channel(c) => store.chat_title(c.chat_id),
    }
}

/// Lays out every message of the view for a pane `width` columns wide.
pub fn layout(
    msgs: &[&Message],
    store: &Store,
    cfg: &Config,
    images: &Images,
    width: u16,
    selected: Option<i64>,
) -> Layout {
    let width = width.max(10) as usize;
    let opts = RenderOpts {
        markdown: cfg.markdown.enabled,
        mask_spoilers: cfg.markdown.mask_spoilers,
        code_theme: cfg.markdown.theme.clone(),
    };
    let dim = Style::new().fg(Color::DarkGray);
    let ts_style = Style::new().fg(super::parse_color(&cfg.theme.timestamp_color));
    let sel_bg = super::parse_color(&cfg.theme.selected_bg);

    let mut out = Layout::default();
    let mut last_day = None;
    let mut last_sender: Option<(i64, i32)> = None;

    for m in msgs {
        let when = local(m.date);
        if cfg.date_separator.enabled && last_day != Some(when.date_naive()) {
            let label = format!(" {} ", when.format(&cfg.date_separator.format));
            let side = width.saturating_sub(label.width()) / 2;
            out.lines.push(Line::from(Span::styled(
                format!(
                    "{}{}{}",
                    "─".repeat(side),
                    label,
                    "─".repeat(width.saturating_sub(side + label.width()))
                ),
                dim,
            )));
            last_day = Some(when.date_naive());
            last_sender = None;
        }

        let start = out.lines.len();
        let sender_id = Store::sender_id(&m.sender_id);
        let (media, text) = parts(m, store);

        // Service messages: one dim line, no header.
        let Some(text) = text else {
            let line = match media {
                Media::Placeholder(p) => p,
                _ => String::new(),
            };
            for l in wrap(
                &[Span::styled(
                    format!("— {line}"),
                    dim.add_modifier(Modifier::ITALIC),
                )],
                width,
            ) {
                out.lines.push(l);
            }
            out.ranges.push((m.id, start, out.lines.len()));
            last_sender = None;
            continue;
        };

        // Group consecutive messages from one sender within 5 minutes, like Discord.
        let grouped = m.reply_to.is_none()
            && m.forward_info.is_none()
            && last_sender.is_some_and(|(s, d)| s == sender_id && m.date - d < 300);
        if !grouped {
            let mut header = vec![Span::styled(
                store.sender_name(&m.sender_id),
                Style::new()
                    .fg(name_color(sender_id))
                    .add_modifier(Modifier::BOLD),
            )];
            if cfg.timestamps.enabled {
                header.push(Span::styled(
                    format!("  {}", when.format(&cfg.timestamps.format)),
                    ts_style,
                ));
            }
            if m.edit_date != 0 {
                header.push(Span::styled("  (edited)", dim));
            }
            if m.sending_state.is_some() {
                header.push(Span::styled("  sending…", dim));
            }
            out.lines.push(Line::from(header));
        }
        last_sender = Some((sender_id, m.date));

        if let Some(fwd) = &m.forward_info {
            out.lines.push(Line::from(Span::styled(
                truncate(
                    &format!("↷ Forwarded from {}", origin_name(&fwd.origin, store)),
                    width,
                ),
                dim.add_modifier(Modifier::ITALIC),
            )));
        }
        if let Some(tdlib_rs::enums::MessageReplyTo::Message(r)) = &m.reply_to {
            let quoted = if r.chat_id == m.chat_id || r.chat_id == 0 {
                store
                    .message(m.chat_id, r.message_id)
                    .map(|q| (store.sender_name(&q.sender_id), snippet(q, store)))
            } else {
                None
            };
            let label = match quoted {
                Some((name, snip)) => format!("╭ ↪ {name}: {snip}"),
                None => match &r.origin {
                    Some(o) => format!("╭ ↪ {}", origin_name(o, store)),
                    None => "╭ ↪ reply".into(),
                },
            };
            out.lines
                .push(Line::from(Span::styled(truncate(&label, width), dim)));
        }

        match media {
            Media::None => {}
            Media::Placeholder(p) => out
                .lines
                .push(Line::from(Span::styled(p, Style::new().fg(Color::Magenta)))),
            Media::Photo(photo) => push_image(
                &mut out,
                images,
                width,
                photo_dims(photo),
                |cols| images.pick_size(photo, cols).map(|s| s.photo.id),
                "[Photo]",
            ),
            Media::StaticSticker(s) => {
                let rows = images.max_rows.min(8);
                if images.enabled() {
                    let (cols, rows) = images.cells_for(s.width, s.height, width as u16, rows);
                    let line = out.lines.len();
                    out.lines.push(Line::from(Span::styled(
                        format!("[Sticker {}]", s.emoji),
                        dim,
                    )));
                    out.lines.extend((1..rows).map(|_| Line::default()));
                    out.images.push(ImageSlot {
                        line,
                        wanted: Wanted {
                            file_id: s.sticker.id,
                            cols,
                            rows,
                        },
                    });
                } else {
                    out.lines.push(Line::from(Span::styled(
                        format!("[Sticker {}]", s.emoji),
                        Style::new().fg(Color::Magenta),
                    )));
                }
            }
        }

        if !text.text.is_empty() {
            out.lines
                .extend(wrap(&formatted_spans(&text, Style::new(), &opts), width));
        }
        if grouped
            && m.edit_date != 0
            && let Some(last) = out.lines.last_mut()
        {
            last.spans.push(Span::styled(" (edited)", dim));
        }

        let end = out.lines.len();
        if selected == Some(m.id) {
            for l in &mut out.lines[start..end] {
                l.style = l.style.bg(sel_bg);
            }
        }
        out.ranges.push((m.id, start, end));
    }
    out
}

fn photo_dims(p: &tdlib_rs::types::Photo) -> (i32, i32) {
    p.sizes
        .iter()
        .max_by_key(|s| s.width)
        .map(|s| (s.width, s.height))
        .unwrap_or((320, 240))
}

fn push_image(
    out: &mut Layout,
    images: &Images,
    width: usize,
    (pw, ph): (i32, i32),
    file_for: impl Fn(u16) -> Option<i32>,
    label: &str,
) {
    let dim = Style::new().fg(Color::DarkGray);
    if !images.enabled() {
        out.lines.push(Line::from(Span::styled(
            label.to_string(),
            Style::new().fg(Color::Magenta),
        )));
        return;
    }
    let (cols, rows) = images.cells_for(pw, ph, width as u16, images.max_rows);
    let Some(file_id) = file_for(cols) else {
        out.lines.push(Line::from(Span::styled(
            label.to_string(),
            Style::new().fg(Color::Magenta),
        )));
        return;
    };
    let line = out.lines.len();
    let first = if images.failed(file_id) {
        format!("{label} (failed to load)")
    } else {
        label.to_string()
    };
    out.lines.push(Line::from(Span::styled(first, dim)));
    out.lines.extend((1..rows).map(|_| Line::default()));
    out.images.push(ImageSlot {
        line,
        wanted: Wanted {
            file_id,
            cols,
            rows,
        },
    });
}

impl MessagesState {
    /// First visible line for a viewport of `height`, updating scroll so the
    /// selection is shown when `reveal` is set.
    pub fn window(&mut self, layout: &Layout, height: usize) -> usize {
        let total = layout.lines.len();
        let max_scroll = total.saturating_sub(height);
        if self.reveal {
            self.reveal = false;
            if let Some(sel) = self.selected
                && let Some(&(_, a, b)) = layout.ranges.iter().find(|r| r.0 == sel)
            {
                let top = total.saturating_sub(height + self.scroll);
                if a < top {
                    self.scroll = total.saturating_sub(a + height);
                } else if b > top + height {
                    self.scroll = total.saturating_sub(b);
                }
            }
        }
        self.scroll = self.scroll.min(max_scroll);
        let top = total.saturating_sub(height + self.scroll);
        self.at_top = top == 0;
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_follows_bottom_and_reveals_selection() {
        let mut layout = Layout::default();
        for i in 0..10 {
            layout.lines.push(Line::raw(format!("line {i}")));
            layout.ranges.push((i as i64, i, i + 1));
        }
        let mut st = MessagesState::default();
        assert_eq!(st.window(&layout, 4), 6, "stuck to bottom");
        assert!(!st.at_top);
        st.selected = Some(1);
        st.reveal = true;
        assert_eq!(st.window(&layout, 4), 1);
        st.scroll = 100;
        assert_eq!(st.window(&layout, 4), 0, "scroll clamps at the top");
        assert!(st.at_top);
    }

    #[test]
    fn sizes_are_human() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2_100_000), "2.1 MB");
        assert_eq!(duration(75), "1:15");
    }
}
