//! Replies as markdown, drawn the way Claude Code draws them:
//!
//! ```text
//! Setup                                  ← # heading: bold, underlined
//!
//! Run it with cargo run, then:           ← `code` in its own color
//!
//! - one                                  ← lists keep a hanging indent
//!   wrapped onto a second row
//!
//! ▎ a quote, in italics                  ← a bar on every row
//!
//!   fn main() {}                         ← code: indented, highlighted
//!
//! ┌──────┬───────┐
//! │ Name │ Value │                       ← tables fit the width,
//! ├──────┼───────┤                         wrapping cells as needed
//! │ a    │ 1     │
//! └──────┴───────┘
//! ```
//!
//! Lines come back already wrapped to the width, since a quote's bar and a
//! list item's indent have to repeat on every row a paragraph wraps onto.

use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How far code sits in from the text around it.
const CODE_INDENT: &str = "  ";
/// Narrowest a table column gets before the table is laid out as a list.
const MIN_COLUMN: usize = 8;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults().themes.remove("base16-ocean.dark").unwrap_or_default()
});

/// `text` as lines no wider than `width`.
pub fn render(text: &str, width: u16) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new(width as usize);
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.out
}

/// What a row's prefix is made of, outermost first.
enum Container {
    Quote,
    /// A list item: its marker goes on its first row, spaces on the rest.
    Item { marker: String, pending: bool },
}

struct Table {
    aligns: Vec<Alignment>,
    /// Each row's cells, each cell its styled text.
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Whether the first row is the header.
    header: bool,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    /// The paragraph, heading or table cell being collected.
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// The next number for each open list; `None` for a bulleted one.
    lists: Vec<Option<u64>>,
    /// Each open link's URL and where its text starts in `inline`.
    links: Vec<(String, usize)>,
    /// The open code block's language and text.
    code: Option<(Option<String>, String)>,
    table: Option<Table>,
    /// A blank line goes before the next row.
    gap: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Renderer {
            width,
            out: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            links: Vec::new(),
            code: None,
            table: None,
            gap: false,
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some((_, code)) => code.push_str(&text),
                None => self.text(&text),
            },
            Event::Code(text) => {
                let style = self.style().patch(Style::new().fg(code_color()));
                self.inline.push(Span::styled(text.to_string(), style));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::InlineMath(text) | Event::DisplayMath(text) | Event::FootnoteReference(text) => self.text(&text),
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::Rule => {
                self.flush();
                self.gap = true;
                let width = self.width.saturating_sub(self.prefix_width());
                self.emit(vec![Span::styled("─".repeat(width), dim())]);
                self.gap = true;
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.styles.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                // The blank line before a quote has no bar.
                self.open_gap();
                self.containers.push(Container::Quote);
                self.styles.push(Style::new().add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.gap = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split(|c: char| c == ',' || c.is_whitespace()).next().filter(|l| !l.is_empty()).map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "- ".to_string(),
                };
                self.containers.push(Container::Item { marker, pending: true });
            }
            Tag::Table(aligns) => {
                self.flush();
                self.gap = true;
                self.table = Some(Table { aligns, rows: Vec::new(), header: false });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                    table.header = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::Emphasis => self.styles.push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.styles.push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.inline.len()));
                self.styles.push(Style::new().add_modifier(Modifier::UNDERLINED));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.styles.pop();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.code_block(lang.as_deref(), &code);
                }
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                // Only a whole list is set off from what follows; the lists
                // nested in it run on.
                if self.lists.is_empty() {
                    self.gap = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.draw_table(table);
                }
                self.gap = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                if let Some((url, from)) = self.links.pop() {
                    let text: String = self.inline[from.min(self.inline.len())..].iter().map(|s| s.content.as_ref()).collect();
                    let text = text.trim();
                    if !url.is_empty() && !url.starts_with('#') && text != url && text != url.trim_start_matches("mailto:") {
                        self.inline.push(Span::styled(format!(" ({})", url), dim()));
                    }
                }
            }
            _ => {}
        }
    }

    fn style(&self) -> Style {
        self.styles.iter().fold(Style::default(), |style, s| style.patch(*s))
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        self.inline.push(Span::styled(text.to_string(), style));
    }

    /// Lay out the paragraph collected so far, if any.
    fn flush(&mut self) {
        if self.inline.is_empty() || self.table.is_some() {
            return;
        }
        let inline = std::mem::take(&mut self.inline);
        self.emit_wrapped(&inline);
    }

    fn emit_wrapped(&mut self, spans: &[Span<'static>]) {
        let width = self.width.saturating_sub(self.prefix_width());
        for row in wrap(spans, width) {
            self.emit(row);
        }
    }

    /// The bars and indents in front of a row; `first` puts each pending
    /// list marker in.
    fn prefix(&self, first: bool) -> Vec<Span<'static>> {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => Span::styled("▎ ", dim()),
                Container::Item { marker, pending } if *pending && first => Span::raw(marker.clone()),
                Container::Item { marker, .. } => Span::raw(" ".repeat(marker.width())),
            })
            .collect()
    }

    fn prefix_width(&self) -> usize {
        self.prefix(false).iter().map(|s| s.content.width()).sum()
    }

    /// The blank line owed before the next row, if one is.
    fn open_gap(&mut self) {
        if std::mem::take(&mut self.gap) && !self.out.is_empty() {
            let prefix = self.prefix(false);
            self.out.push(Line::from(prefix));
        }
    }

    /// Put `row` out behind its prefix.
    fn emit(&mut self, row: Vec<Span<'static>>) {
        self.open_gap();
        let mut spans = self.prefix(true);
        spans.extend(row);
        self.out.push(Line::from(spans));
        for container in &mut self.containers {
            if let Container::Item { pending, .. } = container {
                *pending = false;
            }
        }
    }

    /// Code keeps its own line breaks: a line too long for the width breaks
    /// where it reaches the edge rather than at a word.
    fn code_block(&mut self, lang: Option<&str>, code: &str) {
        let width = self.width.saturating_sub(self.prefix_width() + CODE_INDENT.len());
        for line in highlight(lang, code) {
            for row in chop(line, width) {
                let mut spans = vec![Span::raw(CODE_INDENT)];
                spans.extend(row);
                self.emit(spans);
            }
        }
    }

    /// A table in box characters, its columns narrowed (widest first) and
    /// their cells wrapped to fit the width, or one block per row when even
    /// that won't fit.
    fn draw_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0).max(table.aligns.len());
        if columns == 0 {
            return;
        }
        let natural: Vec<usize> = (0..columns)
            .map(|c| table.rows.iter().filter_map(|r| r.get(c)).map(|cell| spans_width(cell)).max().unwrap_or(0).max(1))
            .collect();
        // `│ ` before each column, ` ` after, and the closing `│`.
        let chrome = 3 * columns + 1;
        let budget = self.width.saturating_sub(self.prefix_width() + chrome);
        let floors: Vec<usize> = natural.iter().map(|&w| w.min(MIN_COLUMN)).collect();
        let mut widths = natural.clone();
        if widths.iter().sum::<usize>() > budget {
            if floors.iter().sum::<usize>() > budget {
                return self.table_as_list(table);
            }
            while widths.iter().sum::<usize>() > budget {
                let widest = (0..columns).filter(|&c| widths[c] > floors[c]).max_by_key(|&c| widths[c]);
                let Some(widest) = widest else { break };
                widths[widest] -= 1;
            }
        }

        let border = |left: &str, middle: &str, right: &str| {
            let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::styled(format!("{}{}{}", left, segments.join(middle), right), dim())]
        };
        self.emit(border("┌", "┬", "┐"));
        for (i, row) in table.rows.iter().enumerate() {
            if i > 0 {
                self.emit(border("├", "┼", "┤"));
            }
            let header = table.header && i == 0;
            let cells: Vec<Vec<Vec<Span<'static>>>> = (0..columns)
                .map(|c| {
                    let cell = row.get(c).cloned().unwrap_or_default();
                    let cell = if header { bold(cell) } else { cell };
                    wrap(&cell, widths[c])
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
            for k in 0..height {
                let mut line = vec![Span::styled("│", dim())];
                for (c, cell) in cells.iter().enumerate() {
                    let content = cell.get(k).cloned().unwrap_or_default();
                    let pad = widths[c].saturating_sub(spans_width(&content));
                    let (left, right) = match table.aligns.get(c) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    line.push(Span::raw(" ".repeat(left + 1)));
                    line.extend(content);
                    line.push(Span::raw(" ".repeat(right + 1)));
                    line.push(Span::styled("│", dim()));
                }
                self.emit(line);
            }
        }
        self.emit(border("└", "┴", "┘"));
    }

    /// Each row as `Header: value` lines, for a table too wide to draw.
    fn table_as_list(&mut self, table: Table) {
        let mut rows = table.rows.into_iter();
        let headers = if table.header { rows.next().unwrap_or_default() } else { Vec::new() };
        for (i, row) in rows.enumerate() {
            if i > 0 {
                self.gap = true;
            }
            for (c, cell) in row.into_iter().enumerate() {
                let mut spans = Vec::new();
                if let Some(header) = headers.get(c) {
                    spans.extend(bold(header.clone()));
                    spans.push(Span::styled(": ", Style::new().add_modifier(Modifier::BOLD)));
                }
                spans.extend(cell);
                self.emit_wrapped(&spans);
            }
        }
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::new().add_modifier(Modifier::BOLD),
        _ => Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans.into_iter().map(|s| s.patch_style(Style::new().add_modifier(Modifier::BOLD))).collect()
}

/// Inline code: a soft blue, as Claude Code has it.
fn code_color() -> Color {
    if crate::tui::truecolor() { Color::Rgb(177, 185, 249) } else { Color::Indexed(147) }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Append `text` to `row`, joining it onto the last span when the style
/// matches.
fn push(row: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => row.push(Span::styled(text.to_string(), style)),
    }
}

/// Lay `spans` out in rows of at most `width` cells, breaking at spaces
/// (runs of them collapse to one) and at `\n`, and inside a word only when
/// it is wider than a row.
pub fn wrap(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut wrapper = Wrapper { width: width.max(1), rows: Vec::new(), row: Vec::new(), row_width: 0, space: None, word: Vec::new(), word_width: 0 };
    for span in spans {
        for ch in span.content.chars() {
            if ch == '\n' {
                wrapper.place();
                wrapper.space = None;
                wrapper.break_row();
            } else if ch.is_whitespace() {
                wrapper.place();
                if !wrapper.row.is_empty() {
                    wrapper.space = Some(span.style);
                }
            } else {
                match wrapper.word.last_mut() {
                    Some((text, style)) if *style == span.style => text.push(ch),
                    _ => wrapper.word.push((ch.to_string(), span.style)),
                }
                wrapper.word_width += ch.width().unwrap_or(0);
            }
        }
    }
    wrapper.place();
    if !wrapper.row.is_empty() {
        wrapper.break_row();
    }
    wrapper.rows
}

struct Wrapper {
    width: usize,
    rows: Vec<Vec<Span<'static>>>,
    row: Vec<Span<'static>>,
    row_width: usize,
    /// The space owed before the next word, in the style it was written in.
    space: Option<Style>,
    word: Vec<(String, Style)>,
    word_width: usize,
}

impl Wrapper {
    fn break_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.row));
        self.row_width = 0;
    }

    /// Put the word collected so far on this row, or the next.
    fn place(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let space = self.space.take();
        if !self.row.is_empty() {
            let needed = space.is_some() as usize + self.word_width;
            if self.row_width + needed <= self.width {
                if let Some(style) = space {
                    push(&mut self.row, " ", style);
                    self.row_width += 1;
                }
            } else {
                self.break_row();
            }
        }
        for (text, style) in std::mem::take(&mut self.word) {
            if self.row_width + text.width() <= self.width {
                push(&mut self.row, &text, style);
                self.row_width += text.width();
                continue;
            }
            // Wider than a row: break it where it reaches the edge.
            for ch in text.chars() {
                let w = ch.width().unwrap_or(0);
                if self.row_width + w > self.width && !self.row.is_empty() {
                    self.break_row();
                }
                push(&mut self.row, ch.encode_utf8(&mut [0; 4]), style);
                self.row_width += w;
            }
        }
        self.word_width = 0;
    }
}

/// A line of code cut into rows of at most `width` cells; an empty line
/// is still one row.
fn chop(line: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows = vec![Vec::new()];
    let mut used = 0;
    for span in line {
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width && used > 0 {
                rows.push(Vec::new());
                used = 0;
            }
            push(rows.last_mut().unwrap(), ch.encode_utf8(&mut [0; 4]), span.style);
            used += w;
        }
    }
    rows
}

/// `code`'s lines in the theme's colors, or uncolored when the language
/// isn't one syntect knows. The theme's background is left out, so code
/// sits on the terminal's own.
fn highlight(lang: Option<&str>, code: &str) -> Vec<Vec<Span<'static>>> {
    let code = code.replace('\t', "    ");
    let Some(syntax) = lang.and_then(|l| SYNTAXES.find_syntax_by_token(l)) else {
        return code.lines().map(|l| vec![Span::raw(l.to_string())]).collect();
    };
    let mut highlighter = HighlightLines::new(syntax, &THEME);
    LinesWithEndings::from(&code)
        .map(|line| match highlighter.highlight_line(line, &SYNTAXES) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| (style, text.trim_end_matches(['\n', '\r'])))
                .filter(|(_, text)| !text.is_empty())
                .map(|(style, text)| {
                    let mut s = Style::new().fg(term_color(style.foreground));
                    if style.font_style.contains(FontStyle::BOLD) {
                        s = s.add_modifier(Modifier::BOLD);
                    }
                    if style.font_style.contains(FontStyle::ITALIC) {
                        s = s.add_modifier(Modifier::ITALIC);
                    }
                    Span::styled(text.to_string(), s)
                })
                .collect(),
            Err(_) => vec![Span::raw(line.trim_end_matches(['\n', '\r']).to_string())],
        })
        .collect()
}

/// A theme color as the terminal can show it: itself in 24-bit color, else
/// the nearest of the 256.
fn term_color(c: syntect::highlighting::Color) -> Color {
    if crate::tui::truecolor() { Color::Rgb(c.r, c.g, c.b) } else { Color::Indexed(ansi256(c.r, c.g, c.b)) }
}

/// The nearest xterm-256 color: from the 6×6×6 cube or the gray ramp.
fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] - v as i32).abs()).unwrap_or(0);
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let distance = |(x, y, z): (i32, i32, i32)| (x - r as i32).pow(2) + (y - g as i32).pow(2) + (z - b as i32).pow(2);
    let cube = distance((LEVELS[ri], LEVELS[gi], LEVELS[bi]));
    let step = ((r as i32 + g as i32 + b as i32) / 3 - 8).clamp(0, 230) / 10;
    let level = 8 + 10 * step;
    if distance((level, level, level)) < cube {
        232 + step as u8
    } else {
        16 + (36 * ri + 6 * gi + bi) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn span<'a>(lines: &'a [Line<'static>], wanted: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains(wanted))
            .unwrap_or_else(|| panic!("no span with {:?} in\n{}", wanted, text(lines)))
    }

    #[test]
    fn inline_markup_becomes_style() {
        let lines = render("# Title\n\nSome **bold**, *slanted*, ~~gone~~ and `code`.", 60);
        assert_eq!(text(&lines), "Title\n\nSome bold, slanted, gone and code.");
        assert!(span(&lines, "Title").style.add_modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));
        assert!(span(&lines, "bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(span(&lines, "slanted").style.add_modifier.contains(Modifier::ITALIC));
        assert!(span(&lines, "gone").style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(span(&lines, "code").style.fg, Some(code_color()));
    }

    #[test]
    fn links_show_their_url_unless_it_is_the_text() {
        let lines = render("See [the docs](https://smartloop.ai/docs) or <https://smartloop.ai>.", 80);
        assert_eq!(text(&lines), "See the docs (https://smartloop.ai/docs) or https://smartloop.ai.");
        assert!(span(&lines, "the docs").style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn lists_wrap_under_their_text_and_nest() {
        let lines = render("Steps:\n\n- one two three four five\n  - inner\n- two\n\n1. first\n2. second\n\nAfter.", 16);
        assert_eq!(
            text(&lines),
            "Steps:\n\n- one two three\n  four five\n  - inner\n- two\n\n1. first\n2. second\n\nAfter."
        );
    }

    #[test]
    fn quotes_keep_their_bar_when_they_wrap() {
        let lines = render("Before.\n\n> a quoted line long enough to wrap\n\nAfter.", 20);
        assert_eq!(text(&lines), "Before.\n\n▎ a quoted line long\n▎ enough to wrap\n\nAfter.");
        assert!(span(&lines, "quoted").style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn code_keeps_its_indentation_and_gets_colors() {
        let source = "Run:\n\n```rust\nfn main() {\n    println!(\"a very long line that runs past the edge\");\n}\n```\nDone.";
        let lines = render(source, 40);
        let shown = text(&lines);
        assert!(shown.starts_with("Run:\n\n  fn main() {\n      println!("), "{}", shown);
        assert!(shown.ends_with("  }\n\nDone."), "{}", shown);
        assert!(lines.iter().all(|l| l.width() <= 40), "{}", shown);
        assert!(span(&lines, "fn").style.fg.is_some());
        assert!(!shown.contains("```"));
    }

    #[test]
    fn code_in_an_unknown_language_is_plain() {
        let lines = render("```nosuchlang\nx = 1\n```", 40);
        assert_eq!(text(&lines), "  x = 1");
        assert_eq!(span(&lines, "x = 1").style.fg, None);
    }

    #[test]
    fn tables_fit_the_width() {
        let source = "| Language | Typing | Notes |\n|:--|:-:|--:|\n| Rust | static | fast and safe |\n| Python | dynamic | easy |";
        let wide = text(&render(source, 80));
        assert_eq!(
            wide,
            "┌──────────┬─────────┬───────────────┐\n\
             │ Language │ Typing  │         Notes │\n\
             ├──────────┼─────────┼───────────────┤\n\
             │ Rust     │ static  │ fast and safe │\n\
             ├──────────┼─────────┼───────────────┤\n\
             │ Python   │ dynamic │          easy │\n\
             └──────────┴─────────┴───────────────┘"
        );

        let narrow = render(source, 34);
        assert!(narrow.iter().all(|l| l.width() <= 34), "{}", text(&narrow));
        assert!(text(&narrow).contains(" fast and │\n│          │         │      safe │"), "{}", text(&narrow));

        let list = text(&render(source, 20));
        assert!(list.starts_with("Language: Rust\nTyping: static\nNotes: fast and safe\n\nLanguage: Python"), "{}", list);
    }

    #[test]
    fn wide_characters_line_up_in_tables() {
        let lines = render("| 名前 | x |\n|---|---|\n| 東京 | 🎉 |", 40);
        let widths: Vec<usize> = lines.iter().map(Line::width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{}", text(&lines));
    }

    #[test]
    fn half_written_markdown_still_renders() {
        for partial in ["**bold", "```rust\nfn", "| a | b |\n|--", "> quote\n- item\n  ```", "1. [link](http"] {
            for width in [1, 5, 40] {
                render(partial, width);
            }
        }
        assert_eq!(text(&render("```rust\nfn main", 40)), "  fn main");
    }

    #[test]
    fn colors_map_to_the_256_palette() {
        assert_eq!(ansi256(0, 0, 0), 16);
        assert_eq!(ansi256(255, 255, 255), 231);
        assert_eq!(ansi256(128, 128, 128), 244);
        assert_eq!(ansi256(255, 0, 0), 196);
    }
}
