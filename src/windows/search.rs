//! # Message Search
//!
//! The `:search <term>` window lists the messages the homeserver found for a term, across every
//! room, and takes the user to the one they pick.
//!
//! It is shaped like the [quick switcher][crate::windows::switcher]: a filter bar over a list.
//! What differs is where the rows come from. The switcher ranks a set it already holds, so it can
//! rebuild on every keystroke for free. These rows are the answer to one request to the
//! homeserver, which is made once, when the window is opened, and never again while the user
//! narrows what came back. Searching as the user typed would be one request per keystroke, and
//! the results would flicker between what each prefix happened to match.
//!
//! The filter is therefore a narrowing of what was found, not a second search. It matches whole
//! substrings rather than fuzzily: a message body is prose, and a subsequence match against prose
//! succeeds almost regardless of what was typed, which is the same reason the switcher only
//! fuzzy-matches names.
//!
//! The order is the order the homeserver gave, which is newest first. Ranking the rows again
//! would throw that away, and recency is what makes a result recognisable: the user is looking
//! for something they remember happening, and they remember roughly when.
//!
//! Encrypted rooms are missing from every result. The homeserver cannot read them, so it cannot
//! index them, and no request made from here changes that.
use std::fmt::{self, Display};
use std::sync::Arc;

use chrono::{DateTime, Local as LocalTz};

use ratatui::{
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
};

use modalkit::{
    actions::{PromptAction, Promptable},
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::list::{ListCursor, ListItem};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::base::{
    IambBufferId,
    IambId,
    IambInfo,
    MessageJump,
    ProgramAction,
    ProgramContext,
    ProgramStore,
};
use crate::search::MessageHit;

use crate::windows::filtered::{FilteredItem, FilteredListState};

/// The `:search` window.
pub type MessageSearchState = FilteredListState<SearchItem>;

/// The widest the room column is drawn.
const ROOM_COLUMN_WIDTH: usize = 20;

/// The widest the sender column is drawn.
const SENDER_COLUMN_WIDTH: usize = 16;

/// The width the timestamp column is drawn at, which is exactly what [TIMESTAMP_FORMAT] produces.
const TIME_COLUMN_WIDTH: usize = 16;

/// How a hit's timestamp is drawn.
///
/// Results reach back years, so the date is as much a part of recognising a message as the time
/// of day is.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M";

/// The narrowest the body column is worth drawing at.
const MIN_BODY_COLUMN_WIDTH: usize = 16;

/// The narrowest the room and sender columns are drawn, when the terminal has no room for more.
const MIN_LABEL_COLUMN_WIDTH: usize = 8;

/// Shown in place of what a column had no room for.
const ELLIPSIS: &str = "…";

/// One message the homeserver found.
#[derive(Clone)]
pub struct SearchItem {
    /// The room the message is in, by the name the user knows it under.
    room: String,

    /// Who sent it, by display name where one is known.
    sender: String,

    /// When it was sent, in the user's own time zone.
    timestamp: DateTime<LocalTz>,

    /// What it said.
    body: String,

    /// Where taking this row goes.
    jump: MessageJump,
}

impl SearchItem {
    /// Turn what the homeserver found into rows, in the order it found them.
    ///
    /// The homeserver gives identifiers, and the user recognises names, so the room and the sender
    /// are resolved here against what the client has synced. A room or a person the client has not
    /// seen falls back to the identifier, which is still enough to tell the rows apart.
    pub fn rows(hits: Vec<MessageHit>, store: &ProgramStore) -> Vec<SearchItem> {
        hits.into_iter().map(|hit| SearchItem::row(hit, store)).collect()
    }

    fn row(hit: MessageHit, store: &ProgramStore) -> SearchItem {
        let room = store.application.get_room_title(&hit.room_id);
        // Display names are tracked per room, because the same person can be called different
        // things in different rooms.
        let sender = store
            .application
            .rooms
            .get(&hit.room_id)
            .and_then(|info| info.display_names.get(&hit.sender))
            .map(|name| name.to_string())
            .unwrap_or_else(|| hit.sender.to_string());

        SearchItem {
            room,
            sender,
            timestamp: timestamp(&hit),
            body: hit.body,
            jump: MessageJump::to_message(hit.room_id, hit.thread, hit.event_id),
        }
    }

    /// Whether this row survives what the user has typed into the filter bar.
    ///
    /// The room and the sender are matched as well as the body, because narrowing a search down to
    /// one room or one person is the common thing to want, and both are on the row already.
    fn matches(&self, needle: &str) -> bool {
        self.body.to_lowercase().contains(needle) ||
            self.sender.to_lowercase().contains(needle) ||
            self.room.to_lowercase().contains(needle)
    }
}

/// When a hit was sent, in the user's own time zone.
///
/// A timestamp the homeserver cannot express falls back to the epoch rather than being dropped:
/// the message is still real and still worth going to.
fn timestamp(hit: &MessageHit) -> DateTime<LocalTz> {
    hit.timestamp
        .to_system_time()
        .map(DateTime::<LocalTz>::from)
        .unwrap_or_default()
}

impl Display for SearchItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.body)
    }
}

impl FilteredItem for SearchItem {
    /// The messages the homeserver found, which belong to this window and to no other.
    ///
    /// They are shared rather than copied so that splitting the window is cheap.
    type Context = Arc<Vec<SearchItem>>;

    fn filter_buffer() -> IambBufferId {
        IambBufferId::MessageSearchFilter
    }

    fn list_buffer() -> IambBufferId {
        IambBufferId::MessageSearchList
    }

    fn matching(
        found: &Arc<Vec<SearchItem>>,
        needle: &str,
        _: &mut ProgramStore,
    ) -> Vec<SearchItem> {
        let needle = needle.to_lowercase();

        found.iter().filter(|item| item.matches(&needle)).cloned().collect()
    }

    fn empty_message() -> &'static str {
        "Nothing here matches. Encrypted rooms are missing: the homeserver cannot read them."
    }
}

/// How wide each column can be drawn in a viewport this wide.
///
/// The body is what the user is reading, so it takes whatever is left over and gives up its width
/// last. The room and sender columns shrink together once there is nothing left to take, and stop
/// at [MIN_LABEL_COLUMN_WIDTH], because a row with no room on it cannot be told from the row above.
///
/// A viewport of no width is one that has not been drawn yet, and the full columns suit it.
fn column_widths(viewport: &ViewportContext<ListCursor>) -> (usize, usize) {
    let available = viewport.dimensions.0;

    if available == 0 {
        return (ROOM_COLUMN_WIDTH, SENDER_COLUMN_WIDTH);
    }

    // Each of the four columns is followed by a space.
    let fixed = TIME_COLUMN_WIDTH + MIN_BODY_COLUMN_WIDTH + 4;
    let labels = available.saturating_sub(fixed);
    let wanted = ROOM_COLUMN_WIDTH + SENDER_COLUMN_WIDTH;

    if labels >= wanted {
        return (ROOM_COLUMN_WIDTH, SENDER_COLUMN_WIDTH);
    }

    let room = (labels * ROOM_COLUMN_WIDTH / wanted).max(MIN_LABEL_COLUMN_WIDTH);
    let sender = labels.saturating_sub(room).max(MIN_LABEL_COLUMN_WIDTH);

    (room, sender)
}

/// Make `s` occupy exactly `width` terminal columns: pad it out, or cut it down.
///
/// A terminal draws columns, and Rust's own width formatting counts characters. One emoji is one
/// character in two columns, so a name formatted that way pushes every column after it one to the
/// right and the list no longer lines up. The cut lands on a grapheme boundary, because half of an
/// emoji is not a character the terminal can draw.
fn fit(s: &str, width: usize) -> String {
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
fn pad(s: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(s));

    format!("{s}{}", " ".repeat(padding))
}

impl ListItem<IambInfo> for SearchItem {
    fn show(
        &self,
        selected: bool,
        viewport: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let style = if selected {
            Style::default().add_modifier(StyleModifier::REVERSED)
        } else {
            Style::default()
        };

        let (room_width, sender_width) = column_widths(viewport);
        let room = format!("{} ", fit(&self.room, room_width));
        let sender = format!("{} ", fit(&self.sender, sender_width));
        let when = self.timestamp.format(TIMESTAMP_FORMAT).to_string();
        let when = format!("{} ", fit(&when, TIME_COLUMN_WIDTH));

        let spans = vec![
            Span::styled(room, style.add_modifier(StyleModifier::BOLD)),
            Span::styled(sender, style),
            Span::styled(when, style.add_modifier(StyleModifier::DIM)),
            Span::styled(self.body.as_str(), style),
        ];

        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        Some(self.body.clone())
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for SearchItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                // Going to a message is two actions in order, and only the main loop can emit
                // them, so the target is left where the loop picks it up. This is the same path a
                // clicked desktop notification takes.
                store.application.message_jump = Some(self.jump.clone());

                Ok(vec![(ProgramAction::NoOp, ctx.clone())])
            },
            PromptAction::Abort(_) => {
                let msg = "Cannot abort entry inside a list";

                Err(EditError::Failure(msg.into()))
            },
            PromptAction::Recall(..) => {
                let msg = "Cannot recall history inside a list";

                Err(EditError::Failure(msg.into()))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::mock_store;
    use matrix_sdk::ruma::{event_id, room_id, user_id, MilliSecondsSinceUnixEpoch, UInt};

    /// A viewport `columns` wide, which is what the window is drawn into.
    fn viewport(columns: usize) -> ViewportContext<ListCursor> {
        let mut viewport = ViewportContext::<ListCursor>::new();

        viewport.dimensions = (columns, 40);
        viewport
    }

    fn hit(body: &str) -> MessageHit {
        MessageHit {
            room_id: room_id!("!general:example.com").to_owned(),
            event_id: event_id!("$found:example.com").to_owned(),
            thread: None,
            sender: user_id!("@nobody:example.com").to_owned(),
            timestamp: MilliSecondsSinceUnixEpoch(UInt::new(1_700_000_000_000).unwrap()),
            body: body.to_string(),
        }
    }

    /// A row as it would be built from a hit, with the names already resolved.
    fn row(room: &str, sender: &str, body: &str) -> SearchItem {
        let hit = hit(body);

        SearchItem {
            room: room.to_string(),
            sender: sender.to_string(),
            timestamp: timestamp(&hit),
            body: body.to_string(),
            jump: MessageJump::to_message(hit.room_id, hit.thread, hit.event_id),
        }
    }

    fn bodies(items: &[SearchItem]) -> Vec<&str> {
        items.iter().map(|item| item.body.as_str()).collect()
    }

    fn filtered(needle: &str, rows: Vec<SearchItem>) -> Vec<SearchItem> {
        let needle = needle.to_lowercase();

        rows.into_iter().filter(|item| item.matches(&needle)).collect()
    }

    /// The column that the body starts in, which is what has to line up between rows.
    async fn body_column(item: &SearchItem, viewport: ViewportContext<ListCursor>) -> usize {
        let mut store = mock_store().await;
        let text = item.show(false, &viewport, &mut store);

        text.lines[0].spans[..3]
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    #[test]
    fn test_the_filter_narrows_by_body_sender_and_room() {
        let rows = vec![
            row("general", "@dan:example.com", "the deploy is done"),
            row("ops", "@sam:example.com", "lunch?"),
        ];

        assert_eq!(bodies(&filtered("deploy", rows.clone())), vec!["the deploy is done"]);
        assert_eq!(bodies(&filtered("sam", rows.clone())), vec!["lunch?"]);
        assert_eq!(bodies(&filtered("ops", rows.clone())), vec!["lunch?"]);
        assert_eq!(filtered("zzzzzz", rows).len(), 0);
    }

    #[test]
    fn test_the_filter_ignores_case() {
        let rows = vec![row("general", "@dan:example.com", "The Deploy Is Done")];

        assert_eq!(filtered("deploy", rows).len(), 1);
    }

    #[test]
    fn test_the_filter_keeps_the_order_the_homeserver_gave() {
        // The homeserver returns newest first, and re-ranking would throw that away.
        let rows = vec![
            row("general", "@dan:example.com", "deploy three"),
            row("general", "@dan:example.com", "deploy two"),
            row("general", "@dan:example.com", "deploy one"),
        ];

        assert_eq!(bodies(&filtered("deploy", rows)), vec![
            "deploy three",
            "deploy two",
            "deploy one",
        ]);
    }

    #[test]
    fn test_a_row_goes_to_the_message_in_its_room() {
        let item = row("general", "@dan:example.com", "the deploy is done");

        assert_eq!(item.jump, MessageJump {
            window: IambId::Room(room_id!("!general:example.com").to_owned(), None),
            event_id: event_id!("$found:example.com").to_owned(),
        });
    }

    #[test]
    fn test_a_row_for_a_threaded_message_goes_to_the_thread() {
        // The room's own scrollback does not hold a thread reply, so the room is the wrong window.
        let mut hit = hit("in the thread");
        hit.thread = Some(event_id!("$root:example.com").to_owned());

        let jump = MessageJump::to_message(hit.room_id, hit.thread, hit.event_id);

        assert_eq!(
            jump.window,
            IambId::Room(
                room_id!("!general:example.com").to_owned(),
                Some(event_id!("$root:example.com").to_owned()),
            )
        );
    }

    #[tokio::test]
    async fn test_an_emoji_in_a_name_does_not_shift_the_later_columns() {
        // An emoji is one character but two columns, and the padding has to count columns.
        let plain = row("general", "lytebot", "hello");
        let emoji = row("general 💕", "lytebot 💕", "hello");

        assert_eq!(
            body_column(&plain, viewport(120)).await,
            body_column(&emoji, viewport(120)).await
        );
    }

    #[tokio::test]
    async fn test_an_over_long_room_or_sender_is_cut_down_to_its_column() {
        let long = row(
            "Discord bridge bot, Matthew Petry, lytedev",
            "@a-very-long-user-name-indeed:example.com",
            "hello",
        );
        let short = row("ops", "@dan:example.com", "hello");
        let expected = ROOM_COLUMN_WIDTH + SENDER_COLUMN_WIDTH + TIME_COLUMN_WIDTH + 3;

        assert_eq!(body_column(&long, viewport(120)).await, expected);
        assert_eq!(body_column(&short, viewport(120)).await, expected);
    }

    #[tokio::test]
    async fn test_a_narrow_terminal_still_leaves_the_body_room_to_read() {
        let item = row("general", "@dan:example.com", "the deploy is done");
        let narrow = body_column(&item, viewport(60)).await;

        assert!(narrow < ROOM_COLUMN_WIDTH + SENDER_COLUMN_WIDTH + TIME_COLUMN_WIDTH + 3);
        assert!(60 - narrow >= MIN_BODY_COLUMN_WIDTH, "the body has to stay readable");
    }

    #[test]
    fn test_a_cut_never_splits_a_grapheme() {
        // The cut lands in the middle of the emoji, so the whole emoji goes.
        assert_eq!(fit("aaa💕", 4), "aaa…");
        assert_eq!(UnicodeWidthStr::width(fit("💕💕", 2).as_str()), 2);
    }
}
