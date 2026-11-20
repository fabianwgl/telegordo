//! Telegram `FormattedText` → styled spans, and word wrapping of spans.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use tdlib_rs::enums::TextEntityType;
use tdlib_rs::types::FormattedText;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub struct RenderOpts {
    pub markdown: bool,
    pub mask_spoilers: bool,
    pub code_theme: String,
}

pub const CODE_STYLE: Style = Style::new().fg(Color::Rgb(220, 160, 110));
const LINK_STYLE: Style = Style::new()
    .fg(Color::Cyan)
    .add_modifier(Modifier::UNDERLINED);
const MENTION_STYLE: Style = Style::new().fg(Color::LightBlue);
const QUOTE_STYLE: Style = Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC);
const SPOILER_STYLE: Style = Style::new().fg(Color::DarkGray).bg(Color::DarkGray);

/// Converts UTF-16 entity ranges into char-index ranges.
fn utf16_to_char_ranges(text: &str, entities: &[(i32, i32)]) -> Vec<(usize, usize)> {
    // utf16 offset at the start of every char, plus one past the end.
    let mut starts = Vec::with_capacity(text.len() + 1);
    let mut pos = 0usize;
    for c in text.chars() {
        starts.push(pos);
        pos += c.len_utf16();
    }
    starts.push(pos);
    let to_char = |u: usize| starts.partition_point(|&s| s < u);
    entities
        .iter()
        .map(|&(off, len)| {
            let a = off.max(0) as usize;
            let b = a + len.max(0) as usize;
            (to_char(a), to_char(b))
        })
        .collect()
}

fn entity_style(t: &TextEntityType, opts: &RenderOpts) -> Option<Style> {
    Some(match t {
        TextEntityType::Bold => Style::new().add_modifier(Modifier::BOLD),
        TextEntityType::Italic => Style::new().add_modifier(Modifier::ITALIC),
        TextEntityType::Underline => Style::new().add_modifier(Modifier::UNDERLINED),
        TextEntityType::Strikethrough => Style::new().add_modifier(Modifier::CROSSED_OUT),
        TextEntityType::Spoiler if opts.mask_spoilers => SPOILER_STYLE,
        TextEntityType::Spoiler => Style::new().add_modifier(Modifier::DIM),
        TextEntityType::Code | TextEntityType::Pre | TextEntityType::PreCode(_) => CODE_STYLE,
        TextEntityType::Url | TextEntityType::TextUrl(_) | TextEntityType::EmailAddress => {
            LINK_STYLE
        }
        TextEntityType::Mention
        | TextEntityType::MentionName(_)
        | TextEntityType::Hashtag
        | TextEntityType::Cashtag
        | TextEntityType::BotCommand => MENTION_STYLE,
        TextEntityType::BlockQuote | TextEntityType::ExpandableBlockQuote => QUOTE_STYLE,
        _ => return None,
    })
}

/// Makes text safe to measure and draw: tabs become spaces up to the next
/// multiple of 4 columns, and other control characters except `\n` are
/// dropped (ratatui would drop them anyway, after `wrap` counted them).
/// Keeps the column across calls, so spans of one text line up.
#[derive(Debug, Default)]
struct Cleaner {
    col: usize,
}

impl Cleaner {
    const TAB: usize = 4;

    fn clean(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\n' => {
                    out.push('\n');
                    self.col = 0;
                }
                '\t' => {
                    let n = Self::TAB - self.col % Self::TAB;
                    out.extend(std::iter::repeat_n(' ', n));
                    self.col += n;
                }
                c if c.is_control() => {}
                c => {
                    out.push(c);
                    self.col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                }
            }
        }
        out
    }
}

/// Styled spans for a formatted text. `base` is applied under every entity.
pub fn formatted_spans(ft: &FormattedText, base: Style, opts: &RenderOpts) -> Vec<Span<'static>> {
    let chars: Vec<char> = ft.text.chars().collect();
    let mut cleaner = Cleaner::default();
    if !opts.markdown || ft.entities.is_empty() {
        return vec![Span::styled(cleaner.clean(&ft.text), base)];
    }
    let ranges = utf16_to_char_ranges(
        &ft.text,
        &ft.entities
            .iter()
            .map(|e| (e.offset, e.length))
            .collect::<Vec<_>>(),
    );

    let mut styles = vec![base; chars.len()];
    let mut spoiler = vec![false; chars.len()];
    // Code blocks with a language are highlighted separately.
    let mut code_blocks: Vec<(usize, usize, String)> = Vec::new();
    for (e, &(a, b)) in ft.entities.iter().zip(&ranges) {
        let b = b.min(chars.len());
        if a >= b {
            continue;
        }
        if let TextEntityType::PreCode(p) = &e.r#type
            && !p.language.is_empty()
        {
            code_blocks.push((a, b, p.language.clone()));
            continue;
        }
        if matches!(e.r#type, TextEntityType::Spoiler) && opts.mask_spoilers {
            spoiler[a..b].iter_mut().for_each(|s| *s = true);
        }
        if let Some(st) = entity_style(&e.r#type, opts) {
            for s in &mut styles[a..b] {
                *s = s.patch(st);
            }
        }
    }

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    code_blocks.sort_by_key(|c| c.0);
    let mut blocks = code_blocks.into_iter().peekable();
    while i < chars.len() {
        if let Some((a, b, lang)) = blocks.peek().cloned()
            && a == i
        {
            let code: String = chars[a..b].iter().collect();
            out.extend(highlight_with(&code, &lang, &opts.code_theme, &mut cleaner));
            blocks.next();
            i = b;
            continue;
        }
        let stop = blocks.peek().map(|c| c.0).unwrap_or(chars.len()).max(i + 1);
        let mut j = i;
        let mut buf = String::new();
        while j < stop && j < chars.len() && styles[j] == styles[i] && spoiler[j] == spoiler[i] {
            let c = chars[j];
            buf.push(if spoiler[j] && !matches!(c, '\n' | ' ' | '\t') {
                '░'
            } else {
                c
            });
            j += 1;
        }
        out.push(Span::styled(cleaner.clean(&buf), styles[i]));
        i = j;
    }
    out
}

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme(name: &str) -> &'static Theme {
    static T: OnceLock<ThemeSet> = OnceLock::new();
    let set = T.get_or_init(ThemeSet::load_defaults);
    set.themes
        .get(name)
        .or_else(|| set.themes.get("base16-ocean.dark"))
        .or_else(|| set.themes.values().next())
        .expect("syntect ships default themes")
}

/// Highlights a code block; falls back to the plain code style.
#[cfg(test)]
fn highlight(code: &str, lang: &str, theme_name: &str) -> Vec<Span<'static>> {
    highlight_with(code, lang, theme_name, &mut Cleaner::default())
}

fn highlight_with(
    code: &str,
    lang: &str,
    theme_name: &str,
    cleaner: &mut Cleaner,
) -> Vec<Span<'static>> {
    let ss = syntaxes();
    let Some(syntax) = ss.find_syntax_by_token(lang) else {
        return vec![Span::styled(cleaner.clean(code), CODE_STYLE)];
    };
    let mut h = HighlightLines::new(syntax, theme(theme_name));
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(code) {
        match h.highlight_line(line, ss) {
            Ok(regions) => {
                for (st, piece) in regions {
                    let c = st.foreground;
                    out.push(Span::styled(
                        cleaner.clean(piece),
                        Style::new().fg(Color::Rgb(c.r, c.g, c.b)),
                    ));
                }
            }
            Err(_) => out.push(Span::styled(cleaner.clean(line), CODE_STYLE)),
        }
    }
    out
}

/// URLs in a text (explicit links and bare URLs), in the order they appear.
pub fn urls(ft: &FormattedText) -> Vec<String> {
    let ranges = utf16_to_char_ranges(
        &ft.text,
        &ft.entities
            .iter()
            .map(|e| (e.offset, e.length))
            .collect::<Vec<_>>(),
    );
    let chars: Vec<char> = ft.text.chars().collect();
    let mut out = Vec::new();
    for (e, &(a, b)) in ft.entities.iter().zip(&ranges) {
        match &e.r#type {
            TextEntityType::TextUrl(u) => out.push(u.url.clone()),
            TextEntityType::Url => out.push(
                chars[a.min(chars.len())..b.min(chars.len())]
                    .iter()
                    .collect(),
            ),
            _ => {}
        }
    }
    out
}

fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Word-wraps spans to `width` columns. `\n` forces a break.
pub fn wrap(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // The current line began with a soft wrap (not a `\n`).
    let mut soft = false;

    let push_piece = |cur: &mut Vec<Span<'static>>, text: &str, style: Style| {
        if let Some(last) = cur.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
            return;
        }
        cur.push(Span::styled(text.to_string(), style));
    };

    for span in spans {
        let style = span.style;
        for (k, segment) in span.content.split('\n').enumerate() {
            if k > 0 {
                lines.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
                soft = false;
            }
            // Split into alternating runs of spaces and non-spaces.
            let mut rest = segment;
            while !rest.is_empty() {
                let is_space = rest.starts_with(' ');
                let end = rest
                    .char_indices()
                    .find(|(_, c)| (*c == ' ') != is_space)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let token = &rest[..end];
                rest = &rest[end..];
                let w = str_width(token);
                if is_space {
                    if soft && cur_w == 0 && cur.is_empty() {
                        continue; // no leading spaces after a soft wrap; indentation stays
                    }
                    let fit = w.min(width.saturating_sub(cur_w));
                    if fit > 0 {
                        push_piece(&mut cur, &token[..fit], style);
                        cur_w += fit;
                    }
                    continue;
                }
                if cur_w + w > width && cur_w > 0 {
                    trim_trailing_spaces(&mut cur);
                    lines.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                    soft = true;
                }
                if w <= width {
                    push_piece(&mut cur, token, style);
                    cur_w += w;
                    continue;
                }
                // A word longer than the line: hard split.
                let mut chunk = String::new();
                let mut chunk_w = 0;
                for c in token.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if cur_w + chunk_w + cw > width && (cur_w + chunk_w) > 0 {
                        push_piece(&mut cur, &chunk, style);
                        lines.push(Line::from(std::mem::take(&mut cur)));
                        chunk.clear();
                        chunk_w = 0;
                        cur_w = 0;
                        soft = true;
                    }
                    chunk.push(c);
                    chunk_w += cw;
                }
                if !chunk.is_empty() {
                    push_piece(&mut cur, &chunk, style);
                    cur_w += chunk_w;
                }
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

fn trim_trailing_spaces(cur: &mut Vec<Span<'static>>) {
    while let Some(last) = cur.last_mut() {
        let trimmed = last.content.trim_end_matches(' ').len();
        if trimmed == 0 {
            cur.pop();
        } else {
            last.content.to_mut().truncate(trimmed);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::types::TextEntity;

    fn opts() -> RenderOpts {
        RenderOpts {
            markdown: true,
            mask_spoilers: true,
            code_theme: "base16-ocean.dark".into(),
        }
    }

    fn ft(text: &str, entities: Vec<(i32, i32, TextEntityType)>) -> FormattedText {
        FormattedText {
            text: text.into(),
            entities: entities
                .into_iter()
                .map(|(offset, length, r#type)| TextEntity {
                    offset,
                    length,
                    r#type,
                })
                .collect(),
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn utf16_offsets_after_emoji() {
        // "😀" is 2 UTF-16 units, so "bold" starts at offset 3.
        let t = ft("😀 bold x", vec![(3, 4, TextEntityType::Bold)]);
        let spans = formatted_spans(&t, Style::new(), &opts());
        let bold: Vec<_> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(bold.len(), 1);
        assert_eq!(bold[0].content, "bold");
    }

    #[test]
    fn overlapping_entities_combine() {
        let t = ft(
            "abcdef",
            vec![(0, 4, TextEntityType::Bold), (2, 4, TextEntityType::Italic)],
        );
        let spans = formatted_spans(&t, Style::new(), &opts());
        let texts: Vec<_> = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
        assert!(
            spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn spoilers_are_masked() {
        let t = ft("a secret b", vec![(2, 6, TextEntityType::Spoiler)]);
        let spans = formatted_spans(&t, Style::new(), &opts());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a ░░░░░░ b");
    }

    #[test]
    fn markdown_off_is_plain() {
        let t = ft("abc", vec![(0, 3, TextEntityType::Bold)]);
        let mut o = opts();
        o.markdown = false;
        let spans = formatted_spans(&t, Style::new(), &o);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn urls_from_entities() {
        let t = ft(
            "see example.com and docs",
            vec![
                (4, 11, TextEntityType::Url),
                (
                    20,
                    4,
                    TextEntityType::TextUrl(tdlib_rs::types::TextEntityTypeTextUrl {
                        url: "https://d.ev".into(),
                    }),
                ),
            ],
        );
        assert_eq!(urls(&t), vec!["example.com", "https://d.ev"]);
    }

    #[test]
    fn wraps_words_and_newlines() {
        let spans = vec![Span::raw("hello world foo\nbar")];
        assert_eq!(plain(&wrap(&spans, 11)), vec!["hello world", "foo", "bar"]);
        assert_eq!(
            plain(&wrap(&spans, 5)),
            vec!["hello", "world", "foo", "bar"]
        );
    }

    #[test]
    fn newlines_keep_indentation() {
        let want = vec!["fn f() {", "    x();", "}"];
        let spans = vec![Span::raw("fn f() {\n    x();\n}")];
        assert_eq!(plain(&wrap(&spans, 20)), want);
        let code = highlight("fn f() {\n    x();\n}", "rust", "base16-ocean.dark");
        assert_eq!(plain(&wrap(&code, 20)), want);
        // A soft wrap still never starts a line with spaces.
        let spans = vec![Span::raw("aaaa    bbbb")];
        assert_eq!(plain(&wrap(&spans, 6)), vec!["aaaa", "bbbb"]);
    }

    #[test]
    fn hard_splits_long_words_and_counts_wide_chars() {
        let spans = vec![Span::raw("abcdefghij")];
        assert_eq!(plain(&wrap(&spans, 4)), vec!["abcd", "efgh", "ij"]);
        // CJK chars are 2 columns wide.
        let spans = vec![Span::raw("日本語")];
        assert_eq!(plain(&wrap(&spans, 4)), vec!["日本", "語"]);
    }

    #[test]
    fn tabs_become_spaces_and_wrap_agrees() {
        let t = ft("\tx", vec![]);
        let spans = formatted_spans(&t, Style::new(), &opts());
        assert_eq!(plain(&wrap(&spans, 20)), vec!["    x"]);
        // Tab stops: "ab" + tab reaches column 4.
        let t = ft("ab\tc\n\tif x {", vec![(0, 2, TextEntityType::Bold)]);
        let spans = formatted_spans(&t, Style::new(), &opts());
        assert_eq!(plain(&wrap(&spans, 20)), vec!["ab  c", "    if x {"]);
        // Widths match what is drawn: no wrapped line is wider than the pane.
        for l in wrap(&spans, 5) {
            assert!(l.width() <= 5, "{l:?}");
        }
        // Other control characters are dropped.
        let t = ft("a\u{7}b", vec![]);
        assert_eq!(formatted_spans(&t, Style::new(), &opts())[0].content, "ab");
    }

    #[test]
    fn tabs_in_highlighted_code_keep_indentation() {
        let spans = highlight("fn x() {\n\treturn;\n}\n", "rust", "base16-ocean.dark");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\n    return;"), "{text:?}");
    }

    #[test]
    fn empty_text_is_one_line() {
        assert_eq!(wrap(&[Span::raw("")], 10).len(), 1);
    }
}
