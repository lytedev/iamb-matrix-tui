//! # Utility functions
use std::borrow::Cow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::{Regex, RegexBuilder};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};

pub fn split_cow(cow: Cow<'_, str>, idx: usize) -> (Cow<'_, str>, Cow<'_, str>) {
    match cow {
        Cow::Borrowed(s) => {
            let s1 = Cow::Borrowed(&s[idx..]);
            let s0 = Cow::Borrowed(&s[..idx]);

            (s0, s1)
        },
        Cow::Owned(mut s) => {
            let s1 = Cow::Owned(s.split_off(idx));
            let s0 = Cow::Owned(s);

            (s0, s1)
        },
    }
}

pub fn take_width(s: Cow<'_, str>, width: usize) -> ((Cow<'_, str>, usize), Cow<'_, str>) {
    // Find where to split the line.
    let mut cur_width = 0;

    let mut idx = UnicodeSegmentation::split_word_bound_indices(s.as_ref())
        .find_map(|(i, word)| {
            let word_width = UnicodeWidthStr::width(word);
            if cur_width + word_width > width {
                Some(i)
            } else {
                cur_width += word_width;
                None
            }
        })
        .unwrap_or(s.len());

    if idx == 0 {
        // first word is wider than available; fall back to splitting by width
        idx = UnicodeSegmentation::grapheme_indices(s.as_ref(), true)
            .find_map(|(i, graph)| {
                let graph_width = UnicodeWidthStr::width(graph);
                if cur_width + graph_width > width {
                    Some(i)
                } else {
                    cur_width += graph_width;
                    None
                }
            })
            .unwrap_or(s.len());
    }

    let (s0, s1) = split_cow(s, idx);

    ((s0, cur_width), s1)
}

pub struct WrappedLinesIterator<'a> {
    iter: std::vec::IntoIter<Cow<'a, str>>,
    curr: Option<Cow<'a, str>>,
    width: usize,
}

impl<'a> WrappedLinesIterator<'a> {
    fn new<T>(input: T, width: usize) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        let width = width.max(2);

        let cows: Vec<Cow<'a, str>> = match input.into() {
            Cow::Borrowed(s) => s.lines().map(Cow::Borrowed).collect(),
            Cow::Owned(s) => s.lines().map(ToOwned::to_owned).map(Cow::Owned).collect(),
        };

        WrappedLinesIterator { iter: cows.into_iter(), curr: None, width }
    }
}

impl<'a> Iterator for WrappedLinesIterator<'a> {
    type Item = (Cow<'a, str>, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr.is_none() {
            self.curr = self.iter.next();
        }

        if let Some(s) = self.curr.take() {
            let width = UnicodeWidthStr::width(s.as_ref());

            if width <= self.width {
                return Some((s, width));
            } else {
                let (prefix, s1) = take_width(s, self.width);
                self.curr = Some(s1);
                return Some(prefix);
            }
        } else {
            return None;
        }
    }
}

pub fn wrap<'a, T>(input: T, width: usize) -> WrappedLinesIterator<'a>
where
    T: Into<Cow<'a, str>>,
{
    WrappedLinesIterator::new(input, width)
}

pub fn wrapped_text<'a, T>(s: T, width: usize, style: Style) -> Text<'a>
where
    T: Into<Cow<'a, str>>,
{
    let mut text = Text::default();

    for (line, w) in wrap(s, width) {
        let space = space_span(width.saturating_sub(w), style);
        let spans = Line::from(vec![Span::styled(line, style), space]);

        text.lines.push(spans);
    }

    return text;
}

/// Shown in place of what a column had no room for.
pub const ELLIPSIS: &str = "…";

/// Make `s` occupy exactly `width` terminal columns: pad it out, or cut it down.
///
/// Rust's own width formatting counts characters, and a terminal draws columns. One emoji is one
/// character in two columns, so a name such as "lytebot 💕" formatted that way pushes every
/// column after it one to the right, and the list no longer lines up. Rust's own formatting also
/// never cuts anything down, so a name longer than the column pushes the later columns right by
/// however much it overran.
///
/// The cut lands on a grapheme boundary, because half of an emoji is not a character the terminal
/// can draw. A grapheme two columns wide that straddles the boundary is dropped whole, and the
/// column it would have half-filled becomes a space.
pub fn fit(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return pad(s, width);
    }

    let budget = width.saturating_sub(UnicodeWidthStr::width(ELLIPSIS));
    let mut kept = String::new();
    let mut used = 0;

    for grapheme in s.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);

        if used + grapheme_width > budget {
            break;
        }

        kept.push_str(grapheme);
        used += grapheme_width;
    }

    kept.push_str(ELLIPSIS);

    pad(&kept, width)
}

/// Pad `s` out to `width` terminal columns, counting columns rather than characters.
pub fn pad(s: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(s));

    format!("{s}{}", " ".repeat(padding))
}

pub fn space(width: usize) -> String {
    " ".repeat(width)
}

pub fn space_span(width: usize, style: Style) -> Span<'static> {
    Span::styled(space(width), style)
}

pub fn space_text(width: usize, style: Style) -> Text<'static> {
    space_span(width, style).into()
}

pub fn join_cell_text<'a>(texts: Vec<(Text<'a>, usize)>, join: Span<'a>, style: Style) -> Text<'a> {
    let height = texts.iter().map(|t| t.0.height()).max().unwrap_or(0);
    let mut text = Text::from(vec![Line::from(vec![join.clone()]); height]);

    for (mut t, w) in texts.into_iter() {
        for i in 0..height {
            if let Some(line) = t.lines.get_mut(i) {
                text.lines[i].spans.append(&mut line.spans);
            } else {
                text.lines[i].spans.push(space_span(w, style));
            }

            text.lines[i].spans.push(join.clone());
        }
    }

    text
}

fn replace_emoji_in_grapheme(grapheme: &str) -> String {
    emojis::get(grapheme)
        .and_then(|emoji| emoji.shortcode())
        .map(|shortcode| format!(":{shortcode}:"))
        .unwrap_or_else(|| grapheme.to_owned())
}

pub fn replace_emojis_in_str(s: &str) -> String {
    let graphemes = s.graphemes(true);
    graphemes.map(replace_emoji_in_grapheme).collect()
}

pub fn replace_emojis_in_span(span: &mut Span) {
    span.content = Cow::Owned(replace_emojis_in_str(span.content.as_ref()))
}

pub fn replace_emojis_in_line(line: &mut Line) {
    for span in &mut line.spans {
        replace_emojis_in_span(span);
    }
}

/// Compile a search pattern, optionally ignoring case.
pub fn compile_search(pattern: &str, case_insensitive: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).case_insensitive(case_insensitive).build()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_compile_search_case_insensitive() {
        let re = compile_search("chicken", true).unwrap();
        assert!(re.is_match("CHICKEN"));
        assert!(re.is_match("Chicken"));
        assert!(re.is_match("chicken"));

        let re = compile_search("chicken", false).unwrap();
        assert!(!re.is_match("CHICKEN"));
        assert!(!re.is_match("Chicken"));
        assert!(re.is_match("chicken"));
    }

    #[test]
    fn test_wrapped_lines_ascii() {
        let s = "hello world!\nabcdefghijklmnopqrstuvwxyz\ngoodbye";

        let mut iter = wrap(s, 100);
        assert_eq!(iter.next(), Some((Cow::Borrowed("hello world!"), 12)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("abcdefghijklmnopqrstuvwxyz"), 26)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("goodbye"), 7)));
        assert_eq!(iter.next(), None);

        let mut iter = wrap(s, 5);
        assert_eq!(iter.next(), Some((Cow::Borrowed("hello"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed(" "), 1)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("world"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("!"), 1)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("abcde"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("fghij"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("klmno"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("pqrst"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("uvwxy"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("z"), 1)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("goodb"), 5)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("ye"), 2)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_wrapped_lines_unicode() {
        let s = "ＣＨＩＣＫＥＮ";

        let mut iter = wrap(s, 14);
        assert_eq!(iter.next(), Some((Cow::Borrowed(s), 14)));
        assert_eq!(iter.next(), None);

        let mut iter = wrap(s, 5);
        assert_eq!(iter.next(), Some((Cow::Borrowed("ＣＨ"), 4)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("ＩＣ"), 4)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("ＫＥ"), 4)));
        assert_eq!(iter.next(), Some((Cow::Borrowed("Ｎ"), 2)));
        assert_eq!(iter.next(), None);
    }
}

/// The frames of the spinner shown while iamb waits on the server.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each frame of the spinner is shown for.
pub const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// The frame of the spinner to show at `at`.
///
/// The time is the only input, so there is no animation to step and nothing to keep in sync: a
/// redraw that arrives late shows the frame for when it arrived rather than rewinding, and two
/// spinners drawn in the same pass agree without being told about each other.
pub fn spinner_frame(at: SystemTime) -> &'static str {
    let millis = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let frame = millis / SPINNER_FRAME.as_millis();

    SPINNER_FRAMES[(frame % SPINNER_FRAMES.len() as u128) as usize]
}

#[cfg(test)]
mod spinner {
    use super::*;

    #[test]
    fn it_advances_one_frame_per_interval() {
        let start = UNIX_EPOCH + SPINNER_FRAME * 40;

        assert_eq!(spinner_frame(start), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(start + SPINNER_FRAME), SPINNER_FRAMES[1]);
        assert_eq!(spinner_frame(start + SPINNER_FRAME * 9), SPINNER_FRAMES[9]);
    }

    #[test]
    fn it_starts_over_after_the_last_frame() {
        let start = UNIX_EPOCH + SPINNER_FRAME * 40;

        assert_eq!(spinner_frame(start + SPINNER_FRAME * 10), SPINNER_FRAMES[0]);
    }

    /// Two draws within one interval must agree, or the spinner flickers between frames instead of
    /// turning.
    #[test]
    fn it_holds_a_frame_for_the_whole_interval() {
        let start = UNIX_EPOCH + SPINNER_FRAME * 40;
        let nearly_over = start + SPINNER_FRAME - Duration::from_millis(1);

        assert_eq!(spinner_frame(start), spinner_frame(nearly_over));
    }

    /// Every frame is one column wide, so the spinner cannot push what shares its row around.
    #[test]
    fn every_frame_takes_one_column() {
        for frame in SPINNER_FRAMES {
            assert_eq!(UnicodeWidthStr::width(frame), 1, "{frame:?}");
        }
    }
}
