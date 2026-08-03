//! # Quick Switcher
//!
//! The `:switch` window, bound to `<C-K>`, is one fuzzy-matched list of everywhere the user might
//! want to jump to: every room, DM, and space they have joined, every thread they follow, together
//! with the windows that iamb's own commands open. Typing narrows it, and taking an entry goes
//! straight there.
//!
//! Every part comes from something that is already the source of truth, so none of them can drift.
//! Rooms come out of [SyncInfo][crate::base::SyncInfo] the same way the `:rooms` and `:dms`
//! windows build their lists, threads come out of the same
//! [followed_thread_items] that fills in `:threads`, and the sections come out of the `window`
//! each [CommandForm][crate::commands::CommandForm] in [IAMB_COMMANDS] declares it opens.
//!
//! Taking an entry emits the same [WindowAction::Switch] that selecting a room in `:rooms` does,
//! so a room reached from here is opened by exactly the machinery that opens it anywhere else.
//!
//! The scoring is [crate::message::mention::fuzzy_score], the same matcher that ranks @-mention
//! completions.
use std::fmt::{self, Display};

use ratatui::{
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
};

use modalkit::{
    actions::{PromptAction, Promptable, WindowAction},
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::list::{ListCursor, ListItem};

use matrix_sdk::ruma::OwnedRoomId;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::base::{
    IambBufferId,
    IambId,
    IambInfo,
    ProgramAction,
    ProgramContext,
    ProgramStore,
    ThreadSummary,
};
use crate::commands::IAMB_COMMANDS;
use crate::message::mention::fuzzy_score;
use crate::message::MessageTimeStamp;
use crate::windows::filtered::{FilteredItem, FilteredListState};
use crate::windows::{followed_thread_items, RoomLikeItem};

/// The `:switch` window.
pub type QuickSwitcherState = FilteredListState<SwitchItem>;

/// The score given to an entry the needle only matched the description of.
///
/// Descriptions are prose, and a subsequence match against prose succeeds far too easily to be
/// worth ranking, so these sort below everything that matched a name outright while still being
/// findable.
const DESCRIPTION_MATCH_SCORE: isize = isize::MIN + 1;

/// The widest the name column is drawn, so that the rest lines up.
const NAME_COLUMN_WIDTH: usize = 36;

/// The narrowest the name column is drawn, when the terminal has no room for more.
const MIN_NAME_COLUMN_WIDTH: usize = 12;

/// The width the kind column is drawn at.
const KIND_COLUMN_WIDTH: usize = 8;

/// The narrowest the detail column is worth drawing at.
const MIN_DETAIL_COLUMN_WIDTH: usize = 8;

/// The width the unread marker is drawn at.
const MARKER_WIDTH: usize = 2;

/// What every column except the name takes up, including the space after each of them.
const OTHER_COLUMNS_WIDTH: usize =
    MARKER_WIDTH + KIND_COLUMN_WIDTH + 1 + MIN_DETAIL_COLUMN_WIDTH + 1;

/// Shown in place of what a column had no room for.
const ELLIPSIS: &str = "…";

/// Shown in front of an entry with activity the user hasn't read yet.
const UNREAD_MARKER: &str = "● ";

/// Shown in front of an entry with nothing unread, to keep the names in one column.
const READ_MARKER: &str = "  ";

/// The recency given to an entry that has no timestamp, which sorts it last.
const NO_RECENCY: u64 = 0;

/// The recency given to a message we have sent but not yet heard back about, which is as recent as
/// anything can be.
const LOCAL_ECHO_RECENCY: u64 = u64::MAX;

/// How recently something happened, as a number that can be sorted on.
fn recency(ts: Option<&MessageTimeStamp>) -> u64 {
    match ts {
        None => NO_RECENCY,
        Some(MessageTimeStamp::LocalEcho) => LOCAL_ECHO_RECENCY,
        Some(MessageTimeStamp::OriginServer(ms)) => u64::from(*ms),
    }
}

/// What a switcher entry is, which is also the sigil and label it is drawn with.
///
/// Rooms and sections share one list, so every row has to say which it is: without that, `#general`
/// and `:rooms` are two lines of text that look alike.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SwitchKind {
    /// A room the user has joined.
    Room,

    /// A direct message.
    Direct,

    /// A space the user has joined.
    Space,

    /// A thread the user follows.
    Thread,

    /// A window that one of iamb's own commands opens.
    Section,
}

impl SwitchKind {
    /// The sigil drawn in front of the entry's name.
    fn sigil(&self) -> &'static str {
        match self {
            SwitchKind::Room => "#",
            SwitchKind::Direct => "@",
            SwitchKind::Space => "+",
            SwitchKind::Thread => ">",
            SwitchKind::Section => ":",
        }
    }

    /// The label drawn in the entry's rightmost column.
    fn label(&self) -> &'static str {
        match self {
            SwitchKind::Room => "room",
            SwitchKind::Direct => "dm",
            SwitchKind::Space => "space",
            SwitchKind::Thread => "thread",
            SwitchKind::Section => "section",
        }
    }

    /// Whether entries of this kind are windows rather than rooms.
    ///
    /// Jumping to a room is the common case, so when nothing has been typed and there is no match
    /// quality to go on, sections sort underneath the rooms.
    fn is_section(&self) -> bool {
        matches!(self, SwitchKind::Section)
    }
}

/// One row in the quick switcher.
#[derive(Clone)]
pub struct SwitchItem {
    /// What the entry is called: a room's name, or the command that opens a window.
    name: String,

    /// A second name the entry can be found by, such as a room's alias.
    ///
    /// Shown after the name, since it is often how the user thinks of the room.
    also_known_as: Option<String>,

    /// What jumping here gets you.
    description: String,

    /// What kind of thing this is.
    kind: SwitchKind,

    /// The window to switch to.
    window: IambId,

    /// Whether there is activity here the user hasn't read yet.
    unread: bool,

    /// How recently something last happened here.
    recency: u64,
}

impl SwitchItem {
    /// The entries for every room, DM, and space the user has joined.
    fn rooms(store: &mut ProgramStore) -> Vec<SwitchItem> {
        let sync_info = &store.application.sync_info;
        let kinds = [
            (sync_info.rooms.clone(), SwitchKind::Room),
            (sync_info.dms.clone(), SwitchKind::Direct),
            (sync_info.spaces.clone(), SwitchKind::Space),
        ];

        let mut items = Vec::new();

        for (room_infos, kind) in kinds {
            for room_info in room_infos {
                let room = &room_info.0;
                let room_id = room.room_id();
                let alias = room.canonical_alias();

                let info = store.application.rooms.get_or_default(room_id.to_owned());
                let name = info.name.clone().unwrap_or_else(|| room_id.to_string());
                let unread = info.unreads(&store.application.settings);

                items.push(SwitchItem {
                    name,
                    also_known_as: alias.map(|alias| alias.to_string()),
                    description: room_id.to_string(),
                    kind,
                    window: IambId::Room(room_id.to_owned(), None),
                    unread: unread.is_unread(),
                    recency: recency(unread.latest()),
                });
            }
        }

        items
    }

    /// The entries for every thread the user follows.
    ///
    /// The threads come from the same [followed_thread_items] that fills in `:threads` and
    /// `:unreadsandthreads`, so the switcher cannot list a different set of threads than they do.
    fn threads(store: &mut ProgramStore) -> Vec<SwitchItem> {
        followed_thread_items(store)
            .into_iter()
            .map(|item| {
                let room_id = item.room_id().to_owned();

                SwitchItem::thread(item.room_name, room_id, ThreadSummary {
                    root: item.thread_root,
                    preview: item.preview,
                    unread: item.unread,
                })
            })
            .collect()
    }

    /// The entry for one followed thread.
    ///
    /// A thread has no name of its own, so the preview of the message that started it is the name,
    /// the same text that `:threads` lists a thread under. The room the thread is in becomes the
    /// second name, because the user who wants "that thread in #general" remembers the room long
    /// after they forget how the thread opened.
    fn thread(room_name: String, room_id: OwnedRoomId, summary: ThreadSummary) -> SwitchItem {
        let ThreadSummary { root, preview, unread } = summary;

        SwitchItem {
            name: preview,
            also_known_as: Some(room_name),
            description: root.to_string(),
            kind: SwitchKind::Thread,
            window: IambId::Room(room_id, Some(root)),
            unread: unread.is_unread(),
            recency: recency(unread.latest()),
        }
    }

    /// The entries for the windows that iamb's own commands open.
    ///
    /// The switcher leaves itself out: it is not somewhere to jump to from inside it.
    fn sections() -> Vec<SwitchItem> {
        let mut items = Vec::new();

        for cmd in IAMB_COMMANDS {
            for form in cmd.forms {
                let Some(window) = &form.window else {
                    continue;
                };

                if window == &IambId::QuickSwitcher {
                    continue;
                }

                let name = match form.args {
                    None => format!(":{}", cmd.name),
                    Some(args) => format!(":{} {args}", cmd.name),
                };

                items.push(SwitchItem {
                    name,
                    also_known_as: None,
                    description: form.description.to_string(),
                    kind: SwitchKind::Section,
                    window: window.clone(),
                    unread: false,
                    recency: NO_RECENCY,
                });
            }
        }

        items
    }

    /// How well this entry matches what the user typed, if it matches at all.
    ///
    /// The names are what get fuzzy-matched. Descriptions are matched too, but only as whole
    /// substrings and only when no name did, since a subsequence match against a sentence
    /// succeeds almost regardless of what was typed.
    fn score(&self, needle: &str) -> Option<isize> {
        let by_name = fuzzy_score(needle, &self.name);
        let by_aka = self.also_known_as.as_deref().and_then(|aka| fuzzy_score(needle, aka));

        if let Some(score) = by_name.max(by_aka) {
            return Some(score);
        }

        if self.description.to_lowercase().contains(&needle.to_lowercase()) {
            return Some(DESCRIPTION_MATCH_SCORE);
        }

        None
    }
}

impl Display for SwitchItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.name)
    }
}

impl FilteredItem for SwitchItem {
    fn filter_buffer() -> IambBufferId {
        IambBufferId::QuickSwitcherFilter
    }

    fn list_buffer() -> IambBufferId {
        IambBufferId::QuickSwitcherList
    }

    fn matching(needle: &str, store: &mut ProgramStore) -> Vec<SwitchItem> {
        let mut items = SwitchItem::rooms(store);

        items.extend(SwitchItem::threads(store));
        items.extend(SwitchItem::sections());

        rank(needle, items)
    }

    fn empty_message() -> &'static str {
        "Nothing to jump to matches that"
    }
}

/// Drop the entries that do not match, and put the best of the rest first.
///
/// How well the needle matched decides the order, so that typing a room's name reaches that room
/// rather than whatever happens to be noisiest. Everything else only breaks ties, which is what
/// decides the whole list when nothing has been typed and every entry scores the same: rooms
/// before sections, unread before quiet, and recent before stale. That is the list the user sees
/// when the switcher opens, so what they most likely want is already at the top.
fn rank(needle: &str, items: Vec<SwitchItem>) -> Vec<SwitchItem> {
    let mut scored = items
        .into_iter()
        .filter_map(|item| Some((item.score(needle)?, item)))
        .collect::<Vec<_>>();

    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then_with(|| a.kind.is_section().cmp(&b.kind.is_section()))
            .then_with(|| b.unread.cmp(&a.unread))
            .then_with(|| b.recency.cmp(&a.recency))
            .then_with(|| a.name.cmp(&b.name))
    });

    scored.into_iter().map(|(_, item)| item).collect()
}

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

/// How wide the name column can be drawn in a viewport this wide.
///
/// The name column is as wide as it can be up to [NAME_COLUMN_WIDTH], because a name the user
/// recognises is worth more than a room ID they do not. It stops at [MIN_NAME_COLUMN_WIDTH] so
/// that a narrow terminal still shows enough of every column to tell the rows apart, rather than
/// one column of names and nothing else.
///
/// A viewport of no width is one that has not been drawn yet, and the full column suits it.
fn name_column_width(viewport: &ViewportContext<ListCursor>) -> usize {
    let available = viewport.dimensions.0;

    if available == 0 {
        return NAME_COLUMN_WIDTH;
    }

    available
        .saturating_sub(OTHER_COLUMNS_WIDTH)
        .clamp(MIN_NAME_COLUMN_WIDTH, NAME_COLUMN_WIDTH)
}

impl ListItem<IambInfo> for SwitchItem {
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

        let marker = if self.unread {
            UNREAD_MARKER
        } else {
            READ_MARKER
        };
        let name = format!("{}{}", self.kind.sigil(), self.name);
        let name = format!("{} ", fit(&name, name_column_width(viewport)));
        let kind = format!("{} ", fit(self.kind.label(), KIND_COLUMN_WIDTH));
        let detail = self.also_known_as.as_deref().unwrap_or(&self.description);

        let name_style = if self.unread {
            style.add_modifier(StyleModifier::BOLD)
        } else {
            style
        };

        let spans = vec![
            Span::styled(marker, style),
            Span::styled(name, name_style),
            Span::styled(kind, style.add_modifier(StyleModifier::DIM)),
            Span::styled(detail, style.add_modifier(StyleModifier::DIM)),
        ];

        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        Some(self.name.clone())
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for SwitchItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                let open = WindowAction::Switch(OpenTarget::Application(self.window.clone()));

                Ok(vec![(open.into(), ctx.clone())])
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
    use crate::base::UnreadInfo;
    use crate::tests::mock_store;
    use matrix_sdk::ruma::{event_id, room_id, RoomId};

    /// A viewport `columns` wide, which is what the switcher is drawn into.
    fn viewport(columns: usize) -> ViewportContext<ListCursor> {
        let mut viewport = ViewportContext::<ListCursor>::new();

        viewport.dimensions = (columns, 40);
        viewport
    }

    /// The column that the kind label starts in, which is what has to line up between rows.
    async fn kind_column(item: &SwitchItem) -> usize {
        kind_column_in(item, viewport(120)).await
    }

    async fn kind_column_in(item: &SwitchItem, viewport: ViewportContext<ListCursor>) -> usize {
        let mut store = mock_store().await;
        let text = item.show(false, &viewport, &mut store);
        let spans = &text.lines[0].spans;

        spans[..2]
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    fn room(name: &str, id: &RoomId, unread: bool, recency: u64) -> SwitchItem {
        SwitchItem {
            name: name.to_string(),
            also_known_as: None,
            description: id.to_string(),
            kind: SwitchKind::Room,
            window: IambId::Room(id.to_owned(), None),
            unread,
            recency,
        }
    }

    fn aliased(name: &str, id: &RoomId, alias: &str) -> SwitchItem {
        SwitchItem {
            also_known_as: Some(alias.to_string()),
            ..room(name, id, false, NO_RECENCY)
        }
    }

    fn thread(preview: &str, room_name: &str, id: &RoomId, unread: bool) -> SwitchItem {
        SwitchItem::thread(room_name.to_string(), id.to_owned(), ThreadSummary {
            root: event_id!("$thread:example.com").to_owned(),
            preview: preview.to_string(),
            unread: UnreadInfo { unread, latest: None },
        })
    }

    fn names(items: &[SwitchItem]) -> Vec<&str> {
        items.iter().map(|item| item.name.as_str()).collect()
    }

    #[test]
    fn test_sections_come_from_the_command_table() {
        let sections = SwitchItem::sections();
        let listed = names(&sections);

        for name in [
            ":rooms", ":dms", ":spaces", ":threads", ":unreads", ":verify", ":welcome",
        ] {
            assert!(listed.contains(&name), "{:?} should be reachable from the switcher", name);
        }

        // Every form that declares a window is listed, and nothing else is.
        let declared = IAMB_COMMANDS
            .iter()
            .flat_map(|cmd| cmd.forms)
            .filter(|form| form.window.is_some())
            .count();

        // ...except the switcher itself, which is not somewhere to jump to from inside it.
        assert_eq!(sections.len(), declared - 1);
        assert!(sections.iter().all(|item| item.window != IambId::QuickSwitcher));
    }

    #[test]
    fn test_sections_switch_to_the_window_they_name() {
        let sections = SwitchItem::sections();
        let rooms = sections.iter().find(|item| item.name == ":rooms").unwrap();

        assert_eq!(rooms.window, IambId::RoomList);
        assert_eq!(rooms.kind, SwitchKind::Section);
    }

    #[test]
    fn test_subcommand_sections_are_listed_by_their_full_form() {
        let sections = SwitchItem::sections();
        let listed = names(&sections);

        assert!(listed.contains(&":unreads threads"));
    }

    #[test]
    fn test_rooms_sort_before_sections_when_nothing_is_typed() {
        let mut items = SwitchItem::sections();
        items.push(room("general", room_id!("!general:example.com"), false, NO_RECENCY));

        let ranked = rank("", items);

        assert_eq!(ranked[0].name, "general");
        assert!(ranked[1..].iter().all(|item| item.kind == SwitchKind::Section));
    }

    #[test]
    fn test_unread_and_recent_rooms_come_first_when_nothing_is_typed() {
        let items = vec![
            room("quiet-but-recent", room_id!("!a:example.com"), false, 3000),
            room("unread-and-old", room_id!("!b:example.com"), true, 1000),
            room("unread-and-recent", room_id!("!c:example.com"), true, 2000),
        ];

        assert_eq!(names(&rank("", items)), vec![
            "unread-and-recent",
            "unread-and-old",
            "quiet-but-recent",
        ]);
    }

    #[test]
    fn test_what_was_typed_outranks_unread_activity() {
        // Otherwise typing a room's name in full would still not reach it.
        let items = vec![
            room("noisy", room_id!("!a:example.com"), true, 9000),
            room("general", room_id!("!b:example.com"), false, NO_RECENCY),
        ];

        assert_eq!(names(&rank("general", items))[0], "general");
    }

    #[test]
    fn test_unread_breaks_ties_between_equally_good_matches() {
        let items = vec![
            room("general", room_id!("!a:example.com"), false, NO_RECENCY),
            room("general", room_id!("!b:example.com"), true, NO_RECENCY),
        ];
        let ranked = rank("general", items);

        assert!(ranked[0].unread);
        assert!(!ranked[1].unread);
    }

    #[test]
    fn test_rooms_are_found_by_alias() {
        let items = vec![
            aliased("Some Room With A Long Name", room_id!("!a:example.com"), "#iamb:0x.badd.cafe"),
            room("unrelated", room_id!("!b:example.com"), false, NO_RECENCY),
        ];

        assert_eq!(names(&rank("iamb", items)), vec!["Some Room With A Long Name"]);
    }

    #[test]
    fn test_rooms_are_found_by_room_id() {
        let items = vec![room(
            "Some Room",
            room_id!("!abcdef:example.com"),
            false,
            NO_RECENCY,
        )];

        assert_eq!(names(&rank("abcdef", items)).len(), 1);
    }

    #[test]
    fn test_sections_are_found_by_what_they_do() {
        // ":unreads" is not something anybody types looking for their unread rooms.
        let ranked = rank("unread rooms", SwitchItem::sections());

        assert!(names(&ranked).contains(&":unreads"));
    }

    #[test]
    fn test_a_name_match_outranks_a_description_match() {
        let items = vec![
            // Matches only because its room ID has the needle in it.
            room("nothing alike", room_id!("!chatter:example.com"), false, NO_RECENCY),
            room("chat", room_id!("!b:example.com"), false, NO_RECENCY),
        ];

        assert_eq!(names(&rank("chat", items)), vec!["chat", "nothing alike"]);
    }

    #[test]
    fn test_entries_that_do_not_match_are_dropped() {
        let items = vec![room(
            "general",
            room_id!("!a:example.com"),
            false,
            NO_RECENCY,
        )];

        assert!(rank("zzzzzz", items).is_empty());
    }

    #[tokio::test]
    async fn test_an_emoji_in_a_name_does_not_shift_the_later_columns() {
        // An emoji is one character but two columns, and the padding has to count columns.
        let plain = room("lytebot", room_id!("!a:example.com"), false, NO_RECENCY);
        let emoji = room("lytebot 💕", room_id!("!b:example.com"), false, NO_RECENCY);

        assert_eq!(kind_column(&plain).await, kind_column(&emoji).await);
    }

    #[tokio::test]
    async fn test_a_multi_character_emoji_does_not_shift_the_later_columns() {
        // A ZWJ sequence is several characters, and a variation selector adds another.
        let plain = room("family", room_id!("!a:example.com"), false, NO_RECENCY);
        let zwj = room("family 👨‍👩‍👧", room_id!("!b:example.com"), false, NO_RECENCY);
        let selector = room("warning ⚠️", room_id!("!c:example.com"), false, NO_RECENCY);

        assert_eq!(kind_column(&plain).await, kind_column(&zwj).await);
        assert_eq!(kind_column(&plain).await, kind_column(&selector).await);
    }

    #[tokio::test]
    async fn test_an_over_long_name_is_cut_down_to_the_column() {
        let long = room(
            "Discord bridge bot, Matthew Petry, lytedev",
            room_id!("!a:example.com"),
            false,
            NO_RECENCY,
        );
        let short = room("general", room_id!("!b:example.com"), false, NO_RECENCY);

        assert_eq!(kind_column(&long).await, kind_column(&short).await);
        assert_eq!(kind_column(&long).await, MARKER_WIDTH + NAME_COLUMN_WIDTH + 1);
    }

    #[tokio::test]
    async fn test_a_cut_name_ends_in_an_ellipsis() {
        let name = "Discord bridge bot, Matthew Petry, lytedev";

        assert!(fit(name, NAME_COLUMN_WIDTH).trim_end().ends_with(ELLIPSIS));
        assert_eq!(
            UnicodeWidthStr::width(fit(name, NAME_COLUMN_WIDTH).as_str()),
            NAME_COLUMN_WIDTH
        );
    }

    #[test]
    fn test_a_cut_never_splits_a_grapheme() {
        // The cut lands in the middle of the emoji, so the whole emoji goes.
        assert_eq!(fit("aaa💕", 4), "aaa…");

        // Only one column is left over, and half an emoji cannot fill it, so a space does.
        let fitted = fit("💕💕", 2);

        assert_eq!(fitted, "… ");
        assert_eq!(UnicodeWidthStr::width(fitted.as_str()), 2);
    }

    #[tokio::test]
    async fn test_a_narrow_terminal_still_shows_every_column() {
        let long = room(
            "Discord bridge bot, Matthew Petry, lytedev",
            room_id!("!a:example.com"),
            false,
            NO_RECENCY,
        );

        // The name column gives up its width first, but never all of it.
        let narrow = kind_column_in(&long, viewport(40)).await;
        let widest = MARKER_WIDTH + NAME_COLUMN_WIDTH + 1;
        let narrowest = MARKER_WIDTH + MIN_NAME_COLUMN_WIDTH + 1;

        assert!(narrow < widest);
        assert!(narrow >= narrowest);
        assert!(narrow + KIND_COLUMN_WIDTH < 40, "the kind column has to fit in the terminal");
    }

    #[test]
    fn test_thread_entries_switch_to_the_thread_in_its_room() {
        // The same window that taking the thread in `:unreadsandthreads` opens.
        let item = thread("what about lunch", "general", room_id!("!a:example.com"), false);

        assert_eq!(item.kind, SwitchKind::Thread);
        assert_eq!(
            item.window,
            IambId::Room(
                room_id!("!a:example.com").to_owned(),
                Some(event_id!("$thread:example.com").to_owned()),
            )
        );
    }

    #[test]
    fn test_threads_are_found_by_their_preview_and_by_their_room() {
        let items = vec![
            thread("what about lunch", "general", room_id!("!a:example.com"), false),
            room("unrelated", room_id!("!b:example.com"), false, NO_RECENCY),
        ];

        assert_eq!(names(&rank("lunch", items.clone())), vec!["what about lunch"]);
        assert_eq!(names(&rank("general", items)), vec!["what about lunch"]);
    }

    #[test]
    fn test_unread_threads_come_first_when_nothing_is_typed() {
        let items = vec![
            room("quiet", room_id!("!a:example.com"), false, NO_RECENCY),
            thread("noisy thread", "general", room_id!("!b:example.com"), true),
        ];

        assert_eq!(names(&rank("", items))[0], "noisy thread");
    }

    #[test]
    fn test_room_entries_switch_to_the_room() {
        let item = room("general", room_id!("!a:example.com"), false, NO_RECENCY);

        assert_eq!(item.window, IambId::Room(room_id!("!a:example.com").to_owned(), None));
    }
}
