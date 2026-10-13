//! Links in the conversation, clickable two ways: docent opens one on a
//! plain click, since it has the mouse for selecting, and each is wrapped in
//! an OSC 8 hyperlink for terminals that hover and cmd-click them.
//!
//! A link rides on its text's style, so it survives wrapping and scrolling
//! with no bookkeeping: the underline color holds the URL's number in a
//! table here. That color is only a marker; `apply` takes it off every cell
//! before the frame reaches the terminal.

use std::sync::Mutex;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
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
    // Text before the next address, gathered into one span.
    let mut plain = String::new();
    let mut rest = text;
    while let Some(start) = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min() {
        let tail = &rest[start..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')).unwrap_or(tail.len());
        let url = trim_url(&tail[..end]);
        if url.len() <= "https://".len() {
            plain.push_str(&rest[..start + end]);
            rest = &rest[start + end..];
            continue;
        }
        plain.push_str(&rest[..start]);
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut plain), style));
        }
        spans.push(Span::styled(url.to_string(), style.patch(self::style(url))));
        rest = &rest[start + url.len()..];
    }
    plain.push_str(rest);
    if !plain.is_empty() || spans.is_empty() {
        spans.push(Span::styled(plain, style));
    }
    spans
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
