//! Drawing. Layout copies discordo: `row(tree, column(messages, composer))`,
//! with a full-width help bar at the bottom.

pub mod chats_tree;
pub mod composer;
pub mod image;
pub mod messages;
pub mod picker;
pub mod render_text;

use std::str::FromStr;

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui_image::Image;

use crate::app::{App, Auth, Focus, Modal};
use crate::keymap::{ComposerAction, GlobalAction, MessagesAction, TreeAction};
use crate::ui::composer::Mode;

pub fn parse_color(s: &str) -> Color {
    Color::from_str(s).unwrap_or(Color::Reset)
}

fn border_type(name: &str) -> Option<BorderType> {
    Some(match name {
        "plain" => BorderType::Plain,
        "thick" => BorderType::Thick,
        "double" => BorderType::Double,
        "hidden" => return None,
        _ => BorderType::Rounded,
    })
}

fn pane<'a>(app: &App, title: impl Into<Line<'a>>, active: bool) -> Block<'a> {
    let t = &app.cfg.theme;
    let color = parse_color(if active {
        &t.active_border_color
    } else {
        &t.inactive_border_color
    });
    let block = Block::default()
        .title(title)
        .title_style(Style::new().fg(parse_color(&t.title_color)))
        .border_style(Style::new().fg(color));
    match border_type(&t.border) {
        Some(bt) => block.borders(Borders::ALL).border_type(bt),
        None => block.borders(Borders::ALL).border_set(border::EMPTY),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if !matches!(app.auth, Auth::Ready) {
        draw_login(f, app, area);
        return;
    }
    let [main, help] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let right = if app.sidebar {
        let w = app.cfg.sidebar.width_percent.clamp(10, 60);
        let [tree, right] =
            Layout::horizontal([Constraint::Percentage(w), Constraint::Percentage(100 - w)])
                .areas(main);
        draw_tree(f, app, tree);
        right
    } else {
        app.rects.tree = Rect::default();
        main
    };

    let composer_h = if app.current.is_some() {
        app.composer.height(right.width)
    } else {
        0
    };
    let [msgs, comp] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(composer_h)]).areas(right);
    draw_messages(f, app, msgs);
    if app.current.is_some() {
        draw_composer(f, app, comp);
    } else {
        app.rects.composer = Rect::default();
    }
    draw_help_bar(f, app, help);

    match &app.modal {
        Modal::None => {}
        Modal::Picker(_) => draw_picker(f, app, area),
        Modal::Help => draw_full_help(f, app, area),
        Modal::ConfirmDelete { .. } => {
            let r = centered(area, 40, 5);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new("Delete this message? y / n")
                    .alignment(Alignment::Center)
                    .block(pane(app, " Confirm ", true)),
                r,
            );
        }
    }
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let active = app.focus == Focus::Tree;
    let block = pane(app, " Chats ", active);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.rects.tree = area;
    app.rects.tree_inner = inner;

    let rows = chats_tree::rows(&app.store, &app.tree);
    if rows.is_empty() {
        f.render_widget(Paragraph::new("loading chats…").dark_gray(), inner);
        return;
    }
    if app.tree.selected.is_none() {
        app.tree.selected = rows.first().map(|r| r.id);
    }
    let sel = chats_tree::index_of(&rows, app.tree.selected);
    let h = inner.height as usize;
    if sel < app.tree.offset {
        app.tree.offset = sel;
    } else if h > 0 && sel >= app.tree.offset + h {
        app.tree.offset = sel + 1 - h;
    }
    app.tree.offset = app.tree.offset.min(rows.len().saturating_sub(h.max(1)));

    let sel_bg = parse_color(&app.cfg.theme.selected_bg);
    let unread_color = parse_color(&app.cfg.theme.unread_color);
    let width = inner.width as usize;
    let mut lines = Vec::new();
    for (i, r) in rows.iter().enumerate().skip(app.tree.offset).take(h) {
        let marker = if r.expandable {
            if r.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };
        let indent = "  ".repeat(r.depth);
        let badge = match (r.mentions, r.unread) {
            (m, _) if m > 0 => format!(" @{m}"),
            (_, u) if u > 0 => format!(" {u}"),
            _ => String::new(),
        };
        let is_current = app.current.is_some_and(|v| Some(v) == r.id.view());
        let mut name_style = Style::new();
        if r.muted {
            name_style = name_style.fg(Color::DarkGray);
        } else if r.unread > 0 {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        if is_current {
            name_style = name_style
                .fg(parse_color(&app.cfg.theme.active_border_color))
                .add_modifier(Modifier::BOLD);
        }
        if matches!(r.id, chats_tree::NodeId::List(_)) {
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        let prefix_w = indent.len() + 2;
        let avail =
            width.saturating_sub(prefix_w + unicode_width::UnicodeWidthStr::width(badge.as_str()));
        let label = fit(&r.label, avail);
        let pad = avail.saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()));
        let mut line = Line::from(vec![
            Span::raw(indent),
            Span::styled(marker, Style::new().fg(Color::DarkGray)),
            Span::styled(label, name_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(
                badge,
                Style::new().fg(if r.muted {
                    Color::DarkGray
                } else {
                    unread_color
                }),
            ),
        ]);
        if i == sel {
            line = line.style(Style::new().bg(sel_bg));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn fit(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0;
    let total = unicode_width::UnicodeWidthStr::width(s);
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width || (total > width && w + cw + 1 > width) {
            if total > width && width > 0 {
                out.push('…');
            }
            return out;
        }
        out.push(c);
        w += cw;
    }
    out
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let active = app.focus == Focus::Messages;
    let title = match app.current {
        Some(v) => {
            let mut t = app.store.chat_title(v.chat_id);
            if let Some(topic) = v.topic {
                t = format!(
                    "{t} › {}",
                    app.store.topic_name(v.chat_id, topic).unwrap_or("topic")
                );
            }
            let typing = if app.cfg.typing_indicator.receive {
                app.store.typing_in(v)
            } else {
                vec![]
            };
            let mut spans = vec![Span::raw(format!(" {t} "))];
            if !typing.is_empty() {
                let who = if typing.len() > 2 {
                    format!("{} people", typing.len())
                } else {
                    typing.join(", ")
                };
                let verb = if typing.len() == 1 { "is" } else { "are" };
                spans.push(Span::styled(
                    format!("· {who} {verb} typing… "),
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            Line::from(spans)
        }
        None => Line::raw(" Messages "),
    };
    let mut block = pane(app, title, active);
    if let Some(c) = app.connection {
        block = block.title_bottom(
            Line::styled(format!(" {c} "), Style::new().fg(Color::Yellow)).right_aligned(),
        );
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.rects.messages = area;
    app.rects.messages_inner = inner;

    let Some(view) = app.current else {
        let hint = format!(
            "Pick a chat with {} or {}.",
            app.keys.tree.label(TreeAction::Select),
            app.keys.global.label(GlobalAction::Picker)
        );
        f.render_widget(
            Paragraph::new(hint)
                .dark_gray()
                .alignment(Alignment::Center),
            centered(inner, inner.width, 1),
        );
        return;
    };

    let msgs = app.store.view_messages(view);
    let layout = messages::layout(
        &msgs,
        &app.store,
        &app.cfg,
        &app.images,
        inner.width,
        app.msgs.selected,
    );
    let h = inner.height as usize;
    let top = app.msgs.window(&layout, h);
    app.rects.messages_top = top;
    let st = app.store.history.get(&view).copied().unwrap_or_default();

    let visible: Vec<Line> = layout.lines[top..(top + h).min(layout.lines.len())].to_vec();
    // Anchor short histories to the bottom, like a chat.
    let pad = h.saturating_sub(visible.len());
    app.rects.messages_pad = pad as u16;
    let text_area = Rect {
        y: inner.y + pad as u16,
        height: inner.height - pad as u16,
        ..inner
    };
    if layout.lines.is_empty() {
        let msg = if st.loading {
            "loading…"
        } else {
            "no messages"
        };
        f.render_widget(
            Paragraph::new(msg).dark_gray().alignment(Alignment::Center),
            centered(inner, inner.width, 1),
        );
    } else {
        f.render_widget(Paragraph::new(visible), text_area);
    }
    if top == 0 && st.loading && pad == 0 {
        f.render_widget(
            Paragraph::new("loading older…")
                .dark_gray()
                .alignment(Alignment::Center),
            Rect { height: 1, ..inner },
        );
    }

    for slot in &layout.images {
        let w = slot.wanted;
        // Only draw images that are fully visible.
        if slot.line < top || slot.line + w.rows as usize > top + h {
            continue;
        }
        app.wanted.push(w);
        if let Some(proto) = app.images.protocol(w.file_id) {
            let y = text_area.y + (slot.line - top) as u16;
            let r = Rect {
                x: inner.x,
                y,
                width: w.cols.min(inner.width),
                height: w.rows,
            };
            f.render_widget(Clear, r);
            f.render_widget(Image::new(proto), r);
        }
    }
    app.last_ranges = layout.ranges;
}

fn draw_composer(f: &mut Frame, app: &mut App, area: Rect) {
    let active = app.focus == Focus::Composer;
    let title = match app.composer.mode {
        Mode::Normal => " Message ".to_string(),
        Mode::Reply(id) => {
            let who = app
                .current
                .and_then(|v| app.store.message(v.chat_id, id))
                .map(|m| app.store.sender_name(&m.sender_id))
                .unwrap_or_else(|| "message".into());
            format!(" Replying to {who} ")
        }
        Mode::Edit(_) => " Editing ".to_string(),
    };
    let block = pane(app, title, active);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.rects.composer = area;
    let cursor = if active {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    app.composer.ta.set_cursor_style(cursor);
    f.render_widget(&app.composer.ta, inner);
}

fn draw_help_bar(f: &mut Frame, app: &App, area: Rect) {
    if let Some((s, _)) = &app.status {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" {s}"),
                Style::new().fg(Color::Yellow),
            )),
            area,
        );
        return;
    }
    let k = &app.keys;
    let mut hints: Vec<(String, &str)> = vec![
        (k.global.label(GlobalAction::FocusChats), "chats"),
        (k.global.label(GlobalAction::FocusMessages), "messages"),
        (k.global.label(GlobalAction::FocusComposer), "composer"),
        (k.global.label(GlobalAction::Picker), "picker"),
    ];
    match app.focus {
        Focus::Tree => hints.extend([
            (k.tree.label(TreeAction::Select), "open"),
            (k.tree.label(TreeAction::NextUnread), "next unread"),
        ]),
        Focus::Messages => hints.extend([
            (k.messages.label(MessagesAction::Reply), "reply"),
            (k.messages.label(MessagesAction::Edit), "edit"),
            (k.messages.label(MessagesAction::Delete), "delete"),
            (k.messages.label(MessagesAction::Open), "open"),
        ]),
        Focus::Composer => hints.extend([
            (k.composer.label(ComposerAction::Send), "send"),
            (k.composer.label(ComposerAction::Newline), "newline"),
            (k.composer.label(ComposerAction::OpenEditor), "editor"),
        ]),
    }
    hints.push((k.global.label(GlobalAction::ToggleHelp), "help"));
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, what)) in hints.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            key,
            Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {what}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_full_help(f: &mut Frame, app: &App, area: Rect) {
    let k = &app.keys;
    let mut lines: Vec<Line> = Vec::new();
    fn section<A: Copy + PartialEq>(
        lines: &mut Vec<Line<'static>>,
        title: &str,
        all: &[A],
        b: &crate::keymap::Bindings<A>,
        help: fn(A) -> &'static str,
    ) {
        lines.push(Line::styled(
            title.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        for &a in all {
            let keys: Vec<String> = b.keys(a).iter().map(|k| k.to_string()).collect();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<22}", keys.join(", ")),
                    Style::new().fg(Color::Gray),
                ),
                Span::styled(help(a).to_string(), Style::new().fg(Color::DarkGray)),
            ]));
        }
        lines.push(Line::default());
    }
    section(
        &mut lines,
        "Global",
        GlobalAction::ALL,
        &k.global,
        GlobalAction::help,
    );
    section(
        &mut lines,
        "Chats",
        TreeAction::ALL,
        &k.tree,
        TreeAction::help,
    );
    section(
        &mut lines,
        "Messages",
        MessagesAction::ALL,
        &k.messages,
        MessagesAction::help,
    );
    section(
        &mut lines,
        "Composer",
        ComposerAction::ALL,
        &k.composer,
        ComposerAction::help,
    );
    if !k.enhanced {
        lines.push(Line::styled(
            "Terminal has no kitty keyboard protocol: using alt+ fallbacks.",
            Style::new().fg(Color::Yellow),
        ));
    }
    let r = centered(
        area,
        60.min(area.width),
        (lines.len() as u16 + 2).min(area.height),
    );
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(pane(app, " Keys (any key closes) ", true)),
        r,
    );
}

fn draw_picker(f: &mut Frame, app: &App, area: Rect) {
    let Modal::Picker(p) = &app.modal else { return };
    let r = centered(area, 80.min(area.width), 25.min(area.height));
    f.render_widget(Clear, r);
    let block = pane(app, " Go to chat ", true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [input, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::new().fg(Color::Green)),
            Span::raw(&p.query),
            Span::raw("▏"),
        ])),
        input,
    );
    let h = list.height as usize;
    let start = p.selected.saturating_sub(h.saturating_sub(1));
    let sel_bg = parse_color(&app.cfg.theme.selected_bg);
    let lines: Vec<Line> = p
        .matches
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, &idx)| {
            let l = Line::raw(format!("  {}", p.items[idx].label));
            if i == p.selected {
                l.style(Style::new().bg(sel_bg))
            } else {
                l
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list);
}

fn draw_login(f: &mut Frame, app: &mut App, area: Rect) {
    let (title, prompt, show_input) = match &app.auth {
        Auth::Starting => (" telegordo ", "Starting TDLib…".to_string(), false),
        Auth::Phone => (
            " Log in ",
            "Phone number in international format, e.g. +351912345678".into(),
            true,
        ),
        Auth::Code(how) => (" Log in ", how.clone(), true),
        Auth::Password(hint) => (
            " Two-step verification ",
            if hint.is_empty() {
                "Password".into()
            } else {
                format!("Password (hint: {hint})")
            },
            true,
        ),
        Auth::Email => (" Log in ", "Email address".into(), true),
        Auth::EmailCode => (" Log in ", "Code sent to your email".into(), true),
        Auth::Registration => (
            " Log in ",
            "This number has no Telegram account. Sign up in an official app first.".into(),
            false,
        ),
        Auth::Qr(_) => (" Log in with QR ", String::new(), false),
        Auth::Closing => (" telegordo ", "Closing…".into(), false),
        Auth::Ready => unreachable!(),
    };

    if let Auth::Qr(link) = &app.auth {
        let qr = QrCode::new(link.as_bytes())
            .map(|c| c.render::<Dense1x2>().quiet_zone(true).build())
            .unwrap_or_else(|_| "could not render QR".into());
        let mut lines: Vec<Line> = qr.lines().map(|l| Line::raw(l.to_string())).collect();
        lines.push(Line::default());
        lines.push(Line::raw(
            "Telegram app → Settings → Devices → Link Desktop Device",
        ));
        lines.push(Line::styled(
            "ctrl+c: quit · restart telegordo to log in by phone instead",
            Style::new().fg(Color::DarkGray),
        ));
        let w = lines.iter().map(|l| l.width()).max().unwrap_or(40) as u16 + 4;
        let r = centered(
            area,
            w.min(area.width),
            (lines.len() as u16 + 2).min(area.height),
        );
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .block(pane(app, title, true)),
            r,
        );
        return;
    }

    let r = centered(area, 64.min(area.width), 9);
    f.render_widget(Clear, r);
    let block = pane(app, title, true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [p, input, err, hint] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(prompt).wrap(Wrap { trim: true }), p);
    if show_input {
        app.login_input
            .set_cursor_style(Style::new().add_modifier(Modifier::REVERSED));
        let [arrow, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(input);
        f.render_widget(Span::styled("> ", Style::new().fg(Color::Green)), arrow);
        f.render_widget(&app.login_input, field);
    }
    if let Some(e) = &app.login_error {
        f.render_widget(
            Paragraph::new(e.as_str()).red().wrap(Wrap { trim: true }),
            err,
        );
    } else if app.login_busy {
        f.render_widget(Paragraph::new("…").dark_gray(), err);
    }
    let h = match app.auth {
        Auth::Phone => "enter: next · tab: log in with QR code · ctrl+c: quit",
        _ if show_input => "enter: next · ctrl+c: quit",
        _ => "ctrl+c: quit",
    };
    f.render_widget(
        Paragraph::new(Text::styled(h, Style::new().fg(Color::DarkGray))),
        hint,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(r);
    r
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::App;
    use crate::testutil;
    use crate::tg::View;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    pub fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        screen(&term)
    }

    #[tokio::test]
    async fn main_layout_matches_discordo() {
        let (mut app, _rx) = testutil::app(crate::ui::image::Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.focus = Focus::Composer;
        let s = render(&mut app, 90, 20);
        println!("{s}");

        let lines: Vec<&str> = s.lines().collect();
        // Tree runs full height on the left: its top and bottom corners share columns.
        assert!(lines[0].starts_with("╭ Chats"));
        assert!(lines[18].starts_with("╰"));
        // Composer sits under the messages pane only, to the right of the tree.
        let composer_row = lines
            .iter()
            .position(|l| l.contains("╭ Message"))
            .expect("composer");
        assert!(lines[composer_row].find("╭ Message").unwrap() > 10);
        assert!(
            lines[composer_row].starts_with("│"),
            "tree border continues beside the composer"
        );
        // Content.
        assert!(s.contains("All chats"));
        assert!(s.contains("Family") && s.contains(" 3"));
        assert!(s.contains("╭ ↪ Alice: Hello there"));
        // Grouped: Alice's header appears once for two messages 30 s apart.
        assert_eq!(lines.iter().filter(|l| l.contains("││Alice  ")).count(), 1);
        // Full-width help bar on the last row.
        assert!(lines[19].contains("ctrl+g") && lines[19].contains("send"));
    }
}
