//! Links in the conversation, clickable two ways: docent opens one on a
//! plain click, since it has the mouse for selecting, and each is wrapped in
//! an OSC 8 hyperlink for terminals that hover and cmd-click them.
//!
//! A link rides on its text's style, so it survives wrapping and scrolling
//! with no bookkeeping: the underline color holds the URL's number in a
//! table here. That color is only a marker; `apply` takes it off every cell
//! before the frame reaches the terminal.

use std::ops::Range;
use std::sync::Mutex;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Every URL linked so far, numbered by position. It only grows, so a
/// number stays good in replies already rendered and cached.
static URLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Link text: underlined, and marked as `url`.
pub fn style(url: &str) -> Style {
    mark(Style::new().add_modifier(Modifier::UNDERLINED), url)
}

/// `style` marked as linking to `url`.
pub fn mark(style: Style, url: &str) -> Style {
    let mut urls = URLS.lock().unwrap_or_else(|e| e.into_inner());
    let id = match urls.iter().position(|u| u == url) {
        Some(id) => id,
        None => {
            urls.push(url.to_string());
            urls.len() - 1
        }
    };
    let [_, r, g, b] = (id as u32).to_be_bytes();
    style.underline_color(Color::Rgb(r, g, b))
}

/// The URL numbered `id`.
pub fn url(id: u32) -> Option<String> {
    URLS.lock().unwrap_or_else(|e| e.into_inner()).get(id as usize).cloned()
}

/// The link `style` is marked with, if any.
#[cfg(test)]
pub fn id(style: Style) -> Option<u32> {
    style.underline_color.and_then(id_of)
}

/// The link number a marker color holds.
fn id_of(color: Color) -> Option<u32> {
    match color {
        Color::Rgb(r, g, b) => Some(u32::from_be_bytes([0, r, g, b])),
        _ => None,
    }
}

/// Whether a link to `url` is one to open: the web or mail, not a page
/// anchor or a relative path.
pub fn opens(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:")
}

/// `text` in `style`, with each bare web address in it made a link.
pub fn linkify(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut from = 0;
    for at in addresses(text) {
        if from < at.start {
            spans.push(Span::styled(text[from..at.start].to_string(), style));
        }
        let url = &text[at.clone()];
        spans.push(Span::styled(url.to_string(), style.patch(self::style(url))));
        from = at.end;
    }
    if from < text.len() || spans.is_empty() {
        spans.push(Span::styled(text[from..].to_string(), style));
    }
    spans
}

/// Where the bare web addresses in `text` are.
fn addresses(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = ["https://", "http://"].iter().filter_map(|p| text[from..].find(p)).min().map(|i| from + i) {
        let tail = &text[start..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')).unwrap_or(tail.len());
        let url = trim_url(&tail[..end]);
        if url.len() > "https://".len() {
            found.push(start..start + url.len());
        }
        from = start + end;
    }
    found
}

// Links are marked in the markdown before tui-markdown lays it out, with
// characters that take no width, so a table sizes its cells for the text
// that's finally shown: a label or address opens with LABEL or ADDRESS,
// then its URL's number in BIT_0s and BIT_1s (none for a link that doesn't
// open), and closes with END.
const LABEL: char = '\u{2061}';
const ADDRESS: char = '\u{2062}';
const BIT_0: char = '\u{2063}';
const BIT_1: char = '\u{2064}';
const END: char = '\u{2060}';

/// `markdown` with its links marked for `resolve`, which gets back the URLs
/// they're numbered in. A link reads `label (address)`, or just the address
/// where the label only repeats it; a bare web address becomes one, except
/// in code, HTML and math. A `<br>` in a table cell, which can't break a
/// line, becomes a space.
pub fn mark_links(markdown: &str) -> (String, Vec<String>) {
    // The options tui-markdown parses with, so the two agree on what's code.
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_SUPERSCRIPT
        | Options::ENABLE_SUBSCRIPT
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_GFM
        | Options::ENABLE_TABLES;
    let mut urls = Vec::new();
    // Source ranges to replace, and what with.
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    // Where a bare address is left alone.
    let mut skip: Vec<Range<usize>> = Vec::new();
    let mut link: Option<OpenLink> = None;
    let mut in_cell = false;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        if let Some(OpenLink { label, text, .. }) = &mut link
            && !matches!(event, Event::End(TagEnd::Link))
        {
            *label = Some(label.as_ref().map_or(range.clone(), |l| l.start.min(range.start)..l.end.max(range.end)));
            if let Event::Text(t) | Event::Code(t) = &event {
                text.push_str(t);
            }
            continue;
        }
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some(OpenLink { range, url: dest_url.to_string(), label: None, text: String::new() });
            }
            Event::End(TagEnd::Link) => {
                let Some(OpenLink { range, url, label: Some(label), text }) = link.take() else { continue };
                skip.push(range.clone());
                let id = opens(&url).then(|| number(&mut urls, &url));
                let text = text.trim();
                let edit = if text == url || text == url.trim_start_matches("mailto:") {
                    marked(LABEL, id, &escape(text))
                } else if url.is_empty() || url.starts_with('#') {
                    marked(LABEL, id, &markdown[label])
                } else {
                    format!("{} {}", marked(LABEL, id, &markdown[label]), marked(ADDRESS, id, &format!("({})", escape(&url))))
                };
                edits.push((range, edit));
            }
            Event::Start(Tag::TableCell) => in_cell = true,
            Event::End(TagEnd::TableCell) => in_cell = false,
            Event::InlineHtml(html) if in_cell && matches!(html.to_lowercase().replace(' ', "").as_str(), "<br>" | "<br/>") => {
                edits.push((range, " ".into()));
            }
            Event::Start(Tag::CodeBlock(_) | Tag::Image { .. } | Tag::HtmlBlock | Tag::MetadataBlock(_))
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => skip.push(range),
            _ => {}
        }
    }
    for at in addresses(markdown) {
        if !skip.iter().any(|r| r.contains(&at.start)) {
            let url = &markdown[at.clone()];
            let id = number(&mut urls, url);
            edits.push((at, marked(LABEL, Some(id), &escape(url))));
        }
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(markdown.len());
    let mut from = 0;
    for (range, edit) in edits {
        if range.start < from {
            continue;
        }
        out.push_str(&markdown[from..range.start]);
        out.push_str(&edit);
        from = range.end;
    }
    out.push_str(&markdown[from..]);
    (out, urls)
}

/// A link `mark_links` is reading.
struct OpenLink {
    range: Range<usize>,
    url: String,
    /// Where its label is in the markdown, once one's been seen.
    label: Option<Range<usize>>,
    /// The label's text, without its formatting.
    text: String,
}

/// `url`'s number in `urls`, adding it if it's new.
fn number(urls: &mut Vec<String>, url: &str) -> usize {
    urls.iter().position(|u| u == url).unwrap_or_else(|| {
        urls.push(url.to_string());
        urls.len() - 1
    })
}

/// `text` between `kind`'s opening marker, with link `id`, and `END`.
fn marked(kind: char, id: Option<usize>, text: &str) -> String {
    let mut out = String::from(kind);
    if let Some(id) = id {
        out.extend(format!("{:b}", id).chars().map(|b| if b == '1' { BIT_1 } else { BIT_0 }));
    }
    out.push_str(text);
    out.push(END);
    out
}

/// `text` with what markdown would read into it escaped, so it shows as it
/// is.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\`*_[]()<>!~^$|&#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Take the markers `mark_links` left in `lines` off, styling what they
/// marked: a label underlined, an address dim, each a link to its URL in
/// `urls` if it opens. A table's cells wrap side by side, so a link still
/// open at the end of a row goes on in the same column of the next.
pub fn resolve(lines: &mut [Line<'static>], urls: &[String]) {
    // The link open in each table column (0 outside tables): its kind and
    // URL number.
    let mut open: Vec<Option<(char, Option<usize>)>> = Vec::new();
    for line in lines {
        let mut column = 0;
        let mut spans = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            if span.content == "│" {
                column += 1;
                spans.push(span);
                continue;
            }
            // A cell's padding stays plain, even with a link wrapping
            // through it.
            let padding = column > 0 && span.content.chars().all(|c| c == ' ');
            if padding || !span.content.contains([LABEL, ADDRESS, BIT_0, BIT_1, END]) && open.get(column).is_none_or(Option::is_none) {
                spans.push(span);
                continue;
            }
            if open.len() <= column {
                open.resize(column + 1, None);
            }
            let link = &mut open[column];
            let mut run = String::new();
            let mut flush = |run: &mut String, link: &Option<(char, Option<usize>)>| {
                if !run.is_empty() {
                    let style = match *link {
                        None => span.style,
                        Some((kind, id)) => {
                            let style = match kind {
                                ADDRESS => span.style.add_modifier(Modifier::DIM),
                                _ => span.style.add_modifier(Modifier::UNDERLINED),
                            };
                            match id.and_then(|id| urls.get(id)) {
                                Some(url) => mark(style, url),
                                None => style,
                            }
                        }
                    };
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.content.to_mut().push_str(run),
                        _ => spans.push(Span::styled(run.clone(), style)),
                    }
                    run.clear();
                }
            };
            for c in span.content.chars() {
                match c {
                    LABEL | ADDRESS => {
                        flush(&mut run, link);
                        *link = Some((c, None));
                    }
                    BIT_0 | BIT_1 => {
                        if let Some((_, id)) = link {
                            *id = Some(id.unwrap_or(0) * 2 + usize::from(c == BIT_1));
                        }
                    }
                    END => {
                        flush(&mut run, link);
                        *link = None;
                    }
                    c => run.push(c),
                }
            }
            flush(&mut run, link);
        }
        line.spans = spans;
    }
}

/// `url` without the punctuation ending the sentence around it: a closing
/// bracket only when it has no opening one in the URL, as in
/// `(see https://a.b/c)` but not `https://en.wikipedia.org/wiki/Rust_(language)`.
fn trim_url(mut url: &str) -> &str {
    loop {
        let unopened = |open, close| url.ends_with(close) && url.matches(close).count() > url.matches(open).count();
        if url.ends_with(['.', ',', ';', ':', '!', '?', '\'', '"']) || unopened('(', ')') || unopened('[', ']') {
            url = &url[..url.len() - 1];
        } else {
            return url;
        }
    }
}

/// Take the link markers off `area`'s cells, returning which link each cell
/// belongs to, row by row; with `hyperlinks`, each linked cell also becomes
/// an OSC 8 hyperlink.
pub fn apply(buffer: &mut Buffer, area: Rect, hyperlinks: bool) -> Vec<Vec<Option<u32>>> {
    let urls = URLS.lock().unwrap_or_else(|e| e.into_inner());
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| {
                    let cell = &mut buffer[(x, y)];
                    let id = id_of(cell.underline_color)?;
                    cell.underline_color = Color::Reset;
                    let url = urls.get(id as usize)?;
                    let width = cell.symbol().width();
                    // The second column of a wide character has nothing to
                    // wrap; the first one covers it.
                    if hyperlinks && width > 0 {
                        let symbol = format!("\x1b]8;id={};{}\x1b\\{}\x1b]8;;\x1b\\", id, url, cell.symbol());
                        cell.set_symbol(&symbol);
                        // The escapes have no width on screen, so say how
                        // much the character itself takes.
                        if let Some(width) = std::num::NonZeroU16::new(width as u16) {
                            cell.set_diff_option(CellDiffOption::ForcedWidth(width));
                        }
                    }
                    Some(id)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(span: &Span) -> Option<String> {
        id(span.style).and_then(url)
    }

    #[test]
    fn a_link_keeps_its_url_and_number() {
        let first = style("https://example.com/a");
        assert!(first.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(first, style("https://example.com/a"), "the same URL keeps its number");
        assert_ne!(first, style("https://example.com/b"));
        let id = id(first).unwrap();
        assert_eq!(url(id).as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn bare_addresses_become_links() {
        let spans = linkify("See https://example.com/docs. Or (http://a.b/c?d=1), then https:// alone.", Style::new());
        let text: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, ["See ", "https://example.com/docs", ". Or (", "http://a.b/c?d=1", "), then https:// alone."]);
        assert_eq!(link(&spans[1]).as_deref(), Some("https://example.com/docs"));
        assert_eq!(link(&spans[3]).as_deref(), Some("http://a.b/c?d=1"));
        assert_eq!(link(&spans[0]), None);
        assert_eq!(link(&spans[4]), None);
        assert_eq!(linkify("", Style::new()).len(), 1);
        let wiki = linkify("(https://en.wikipedia.org/wiki/Rust_(language))", Style::new());
        assert_eq!(wiki[1].content, "https://en.wikipedia.org/wiki/Rust_(language)");
    }

    #[test]
    fn markers_come_off_and_cells_become_hyperlinks() {
        let area = Rect::new(0, 0, 6, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "a ", Style::new());
        buffer.set_string(2, 0, "go", style("https://example.com/go"));
        buffer.set_string(4, 0, "界", style("https://example.com/go"));

        let mut plain = buffer.clone();
        let ids = apply(&mut plain, area, false);
        let id = ids[0][2].unwrap();
        assert_eq!(ids[0][..2], [None, None]);
        assert_eq!(ids[0][2..5], [Some(id), Some(id), Some(id)]);
        assert_eq!(plain[(2, 0)].symbol(), "g");
        assert!(plain.content.iter().all(|c| c.underline_color == Color::Reset), "no marker reaches the screen");

        apply(&mut buffer, area, true);
        let cell = &buffer[(2, 0)];
        assert_eq!(cell.symbol(), format!("\x1b]8;id={};https://example.com/go\x1b\\g\x1b]8;;\x1b\\", id));
        assert_eq!(cell.diff_option, CellDiffOption::ForcedWidth(std::num::NonZeroU16::new(1).unwrap()));
        assert_eq!(buffer[(4, 0)].diff_option, CellDiffOption::ForcedWidth(std::num::NonZeroU16::new(2).unwrap()));
        assert_eq!(buffer[(0, 0)].symbol(), "a");
    }
}
