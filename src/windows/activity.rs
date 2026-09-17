//! # Unread Message Feed
//!
//! The `:activity` window is every unread message in every room and followed thread, one row per
//! message, newest conversation first. The other inbox windows answer "where is there something to
//! read"; this one answers "what does it say", which is the question the user actually has when
//! they come back to a client that has been running all day.
//!
//! It is shaped like the [`:search` window][crate::windows::search]: a filter bar over rows of
//! room, sender, time and body, where taking a row goes to that message where it lives. What
//! differs is where the rows come from. A search is one answer from the homeserver, fetched once;
//! these rows are read out of the scrollback the client already holds, and rebuilt on every draw,
//! so a message that arrives while the window is open appears in it.
//!
//! ## Runs, not a flat stream
//!
//! Rows are grouped into runs -- one run per room or thread that has unread messages -- and the
//! runs are ordered by their newest message, newest first. Inside a run the messages stay in the
//! order they were sent.
//!
//! Sorting every message by time on its own would interleave conversations, and three words from
//! one room between two halves of another is not readable. Ordering runs by recency still puts
//! the newest traffic at the top, which is what recency is wanted for.
//!
//! ## Context
//!
//! A run begins with the [ApplicationSettings::unread_context_messages] messages that come before
//! its first unread one, drawn dim. An unread reply is very often an answer, and an answer without
//! the question is not worth reading. Context messages include the user's own, because "what I
//! last said here" is most of what places a reply.
//!
//! Unread messages the user sent themselves are left out. Their own message is not incoming
//! traffic, and a client that advances a receipt on send would not have shown it here anyway.
//!
//! ## What it cannot show
//!
//! The rows come from loaded scrollback, and iamb loads a bounded window of each room. A room with
//! more unread than that reaches further back than the feed can see, and the title says how many
//! rooms are in that position -- an inbox that quietly showed a fraction of itself would be worse
//! than one that admits what it is missing. [Reach] is that count.
use std::fmt::{self, Display};

use chrono::{DateTime, Local as LocalTz};

use ratatui::{
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
};

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, UserId};

use modalkit::{
    actions::{PromptAction, Promptable},
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::list::{ListCursor, ListItem};

use crate::base::{
    IambBufferId,
    IambInfo,
    MessageJump,
    ProgramAction,
    ProgramContext,
    ProgramStore,
    Reach,
    RoomFetchStatus,
    RoomInfo,
};
use crate::message::{Message, MessageEvent, MessageKey};
use crate::util::fit;
use crate::windows::filtered::{FilteredItem, FilteredListState};
use crate::windows::unread_entries;

/// The `:activity` window.
pub type ActivityState = FilteredListState<ActivityItem>;

/// The widest the room column is drawn.
const ROOM_COLUMN_WIDTH: usize = 20;

/// The widest the sender column is drawn.
const SENDER_COLUMN_WIDTH: usize = 16;

/// The width the timestamp column is drawn at, which is exactly what [TIMESTAMP_FORMAT] produces.
const TIME_COLUMN_WIDTH: usize = 11;

/// How a row's timestamp is drawn.
///
/// Everything here is unread, so it is recent, and the day and time of day are what place it. A
/// full date would cost four columns that the body wants more.
const TIMESTAMP_FORMAT: &str = "%a %H:%M";

/// The narrowest the body column is worth drawing at.
const MIN_BODY_COLUMN_WIDTH: usize = 16;

/// The narrowest the room and sender columns are drawn, when the terminal has no room for more.
const MIN_LABEL_COLUMN_WIDTH: usize = 8;

/// Drawn in front of a message that has not been read.
const UNREAD_MARK: &str = "●";

/// Drawn in front of a message that is only there to place the unread ones.
const CONTEXT_MARK: &str = " ";

/// The most rows the feed draws.
///
/// The list is rebuilt on every draw, so its size is paid for again on every keystroke and every
/// message that arrives. An account that has gone unread for a month would otherwise rebuild many
/// thousands of rows to show the twenty that fit on screen. The newest runs are kept, since those
/// are the ones being read.
const MAX_ROWS: usize = 500;

/// One message in the feed: unread, or context for the unread ones under it.
#[derive(Clone)]
pub struct ActivityItem {
    /// The room the message is in, by the name the user knows it under.
    room: String,

    /// Who sent it, by display name where one is known.
    sender: String,

    /// When it was sent, in the user's own time zone.
    timestamp: DateTime<LocalTz>,

    /// What it said.
    body: String,

    /// Whether this message is unread, rather than context drawn to place the unread ones.
    unread: bool,

    /// The room the message lives in.
    room_id: OwnedRoomId,

    /// The thread the message lives in, when it is in one.
    thread: Option<OwnedEventId>,

    /// The message itself.
    event_id: OwnedEventId,
}

impl ActivityItem {
    /// Where taking this row goes.
    pub fn jump(&self) -> MessageJump {
        MessageJump::to_message(self.room_id.clone(), self.thread.clone(), self.event_id.clone())
    }

    /// The room the message is in, and the message itself, for reading up to it.
    pub fn read_at(&self) -> (OwnedRoomId, OwnedEventId) {
        (self.room_id.clone(), self.event_id.clone())
    }

    /// Whether this row survives what the user has typed into the filter bar.
    ///
    /// The room and the sender are matched as well as the body, because narrowing the feed to one
    /// room or one person is the common thing to want, and both are on the row already.
    fn matches(&self, needle: &str) -> bool {
        self.body.to_lowercase().contains(needle) ||
            self.sender.to_lowercase().contains(needle) ||
            self.room.to_lowercase().contains(needle)
    }
}

/// One room or thread with unread messages, and the context that places them.
struct Run {
    /// The context messages, oldest first, followed by the unread ones.
    rows: Vec<ActivityItem>,

    /// When the newest message in the run was sent, which is what orders the runs.
    newest: MessageKey,

    /// Whether the run may reach further back than the client has loaded.
    cut_short: bool,
}

/// Every unread message worth a row, newest run first, and what the feed could not reach.
pub fn rows(store: &mut ProgramStore) -> (Vec<ActivityItem>, Reach) {
    let context = store.application.settings.tunables.unread_context_messages;
    let user_id = store.application.settings.profile.user_id.clone();

    let mut runs = unread_entries(store)
        .into_iter()
        .filter_map(|(room_id, thread)| run(&room_id, thread, &user_id, context, store))
        .collect::<Vec<_>>();

    order_runs(&mut runs);

    let entries_cut_short = runs.iter().filter(|run| run.cut_short).count();
    let mut rows = Vec::new();
    let mut rows_left_out = false;

    for run in runs {
        if rows.len() + run.rows.len() > MAX_ROWS {
            rows_left_out = true;
            break;
        }

        rows.extend(run.rows);
    }

    (rows, Reach { entries_cut_short, rows_left_out })
}

/// Put the runs in the order the feed shows them: the one with the newest message first.
///
/// A run is kept whole rather than having its messages sorted in among another run's. Newest-first
/// still puts the newest traffic at the top of the window, which is what the user is looking for,
/// and a conversation stays readable on the way down.
fn order_runs(runs: &mut [Run]) {
    runs.sort_by(|a, b| b.newest.cmp(&a.newest));
}

/// The run for one room or thread, if it still has an unread message worth showing.
fn run(
    room_id: &OwnedRoomId,
    thread: Option<OwnedEventId>,
    user_id: &UserId,
    context: usize,
    store: &mut ProgramStore,
) -> Option<Run> {
    let title = store.application.get_room_title(room_id);
    let info = store.application.rooms.get(room_id)?;
    let first_unread = info.first_unread(thread.as_deref(), user_id)?;
    let messages = info.get_thread(thread.as_deref())?;

    let row = |key: &MessageKey, msg: &Message, unread: bool| {
        Some(ActivityItem {
            room: title.clone(),
            sender: sender_name(info, msg),
            timestamp: timestamp(key),
            body: msg.event.body().to_string(),
            unread,
            room_id: room_id.clone(),
            thread: thread.clone(),
            event_id: msg.event.event_id()?.to_owned(),
        })
    };

    let incoming = || {
        messages
            .range(first_unread.clone()..)
            .filter(|(_, msg)| is_worth_a_row(msg) && msg.sender != user_id)
    };

    // A run with nothing incoming left in it -- every unread message is one the user sent
    // themselves -- has nothing to put in a feed of incoming messages.
    let newest = incoming().next_back().map(|(key, _)| key.clone())?;

    let unread = incoming().filter_map(|(key, msg)| row(key, msg, true)).collect::<Vec<_>>();

    let mut rows = messages
        .range(..first_unread.clone())
        .rev()
        .filter(|(_, msg)| is_worth_a_row(msg))
        .filter_map(|(key, msg)| row(key, msg, false))
        .take(context)
        .collect::<Vec<_>>();

    rows.reverse();
    rows.extend(unread);

    // A run that starts at the oldest message the client holds has nothing above it to show, and
    // there is more to fetch, so the messages it cannot reach are real.
    let oldest_loaded = messages.first_key_value().map(|(key, _)| key == &first_unread);
    let cut_short =
        oldest_loaded.unwrap_or(false) && !matches!(info.fetch_id, RoomFetchStatus::Done);

    Some(Run { rows, newest, cut_short })
}

/// Whether a message belongs in a feed of what people said.
///
/// A state event is something that happened to a room rather than something somebody said, and a
/// redacted message no longer says anything.
fn is_worth_a_row(msg: &Message) -> bool {
    msg.event.event_id().is_some() && !matches!(msg.event, MessageEvent::State(_))
}

/// Who sent a message, by the name they go by in the room it was sent in.
fn sender_name(info: &RoomInfo, msg: &Message) -> String {
    info.display_names
        .get(&msg.sender)
        .map(|name| name.to_string())
        .unwrap_or_else(|| msg.sender.to_string())
}

/// When a message was sent, in the user's own time zone.
fn timestamp(key: &MessageKey) -> DateTime<LocalTz> {
    key.ts
        .0
        .to_system_time()
        .map(DateTime::<LocalTz>::from)
        .unwrap_or_default()
}

impl Display for ActivityItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.body)
    }
}

impl FilteredItem for ActivityItem {
    /// The feed is built from the store alone, so there is nothing for the window to carry.
    type Context = ();

    fn filter_buffer() -> IambBufferId {
        IambBufferId::ActivityFilter
    }

    fn list_buffer() -> IambBufferId {
        IambBufferId::ActivityList
    }

    fn matching(_: &(), needle: &str, store: &mut ProgramStore) -> Vec<ActivityItem> {
        let (rows, reach) = rows(store);

        // The title is drawn from the window, which cannot look inside its own list, so what the
        // feed could not reach is left where the title picks it up. Same bargain as
        // [crate::base::ChatStore::list_counts].
        store.application.unread_feed_reach = reach;

        if needle.is_empty() {
            return rows;
        }

        let needle = needle.to_lowercase();

        rows.into_iter().filter(|item| item.matches(&needle)).collect()
    }

    fn empty_message() -> &'static str {
        "Nothing unread. The title says whether anything was out of reach."
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

    // The mark and each of the four columns are followed by a space.
    let fixed = UNREAD_MARK.len() + TIME_COLUMN_WIDTH + MIN_BODY_COLUMN_WIDTH + 5;
    let labels = available.saturating_sub(fixed);
    let wanted = ROOM_COLUMN_WIDTH + SENDER_COLUMN_WIDTH;

    if labels >= wanted {
        return (ROOM_COLUMN_WIDTH, SENDER_COLUMN_WIDTH);
    }

    let room = (labels * ROOM_COLUMN_WIDTH / wanted).max(MIN_LABEL_COLUMN_WIDTH);
    let sender = labels.saturating_sub(room).max(MIN_LABEL_COLUMN_WIDTH);

    (room, sender)
}

impl ListItem<IambInfo> for ActivityItem {
    fn show(
        &self,
        selected: bool,
        viewport: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let mut style = if selected {
            Style::default().add_modifier(StyleModifier::REVERSED)
        } else {
            Style::default()
        };

        // A context row is drawn dim as a whole: it is there to be read past, and nothing on it is
        // the thing the user came for.
        if !self.unread {
            style = style.add_modifier(StyleModifier::DIM);
        }

        let (room_width, sender_width) = column_widths(viewport);
        let mark = if self.unread {
            UNREAD_MARK
        } else {
            CONTEXT_MARK
        };
        let room = format!("{} ", fit(&self.room, room_width));
        let sender = format!("{} ", fit(&self.sender, sender_width));
        let when = self.timestamp.format(TIMESTAMP_FORMAT).to_string();
        let when = format!("{} ", fit(&when, TIME_COLUMN_WIDTH));

        let room_style = if self.unread {
            style.add_modifier(StyleModifier::BOLD)
        } else {
            style
        };

        let spans = vec![
            Span::styled(format!("{mark} "), style),
            Span::styled(room, room_style),
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

impl Promptable<ProgramContext, ProgramStore, IambInfo> for ActivityItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                // Going to a message is two actions in order, and only the main loop can emit
                // them, so the target is left where the loop picks it up. This is the same path
                // `:search` and a clicked desktop notification take.
                store.application.message_jump = Some(self.jump());

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

    use matrix_sdk::ruma::{
        EventId,
        OwnedUserId,
        events::{receipt::ReceiptThread, room::message::RoomMessageEventContent},
        server_name,
        user_id,
    };

    use crate::base::{EventLocation, RoomFetchStatus};
    use crate::tests::{TEST_ROOM1_ID, TEST_USER1, key_at, mock_room1_message, mock_store};

    /// A room holding `senders`, one message each, in the order given, with nothing read yet.
    ///
    /// The bodies are the message's place in the room ("1", "2", ...), so that a test can say
    /// which messages it expects to see by naming them.
    async fn store_with(senders: Vec<OwnedUserId>) -> (ProgramStore, Vec<OwnedEventId>) {
        let mut store = mock_store().await;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());

        // mock_store fills the room in with scrollback of its own, which these tests would
        // otherwise have to reason about alongside their own messages.
        *info.get_thread_mut(None) = crate::message::Messages::main();
        info.keys.clear();
        info.fetch_id = RoomFetchStatus::Done;

        let mut event_ids = Vec::new();

        for (i, sender) in senders.into_iter().enumerate() {
            let event_id = EventId::new_v1(server_name!("example.com"));
            let key = key_at(i as u64 + 1, event_id.clone());
            let content = RoomMessageEventContent::text_plain((i + 1).to_string());
            let msg = mock_room1_message(content, sender, key.clone());

            info.keys
                .insert(event_id.clone(), EventLocation::Message(None, key.clone()));
            info.get_thread_mut(None).insert(key, msg);
            event_ids.push(event_id);
        }

        (store, event_ids)
    }

    /// Read the room up to and including the message at `index`.
    fn read_through(store: &mut ProgramStore, event_ids: &[OwnedEventId], index: usize) {
        let user_id = store.application.settings.profile.user_id.clone();
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());

        info.set_receipt(ReceiptThread::Main, user_id, event_ids[index].clone());
    }

    /// The run for the test room, as the feed builds it.
    fn feed(store: &mut ProgramStore, context: usize) -> Option<Run> {
        let user_id = store.application.settings.profile.user_id.clone();

        run(&TEST_ROOM1_ID.clone(), None, &user_id, context, store)
    }

    /// Every row in the run, as "body" and whether it is unread.
    fn rows_of(run: &Run) -> Vec<(&str, bool)> {
        run.rows.iter().map(|row| (row.body.as_str(), row.unread)).collect()
    }

    /// Somebody other than the user, so that their messages are incoming traffic.
    fn stranger() -> OwnedUserId {
        TEST_USER1.clone()
    }

    /// The user the feed is being built for, which is the one [crate::tests::mock_settings] sets.
    fn own() -> OwnedUserId {
        user_id!("@user:example.com").to_owned()
    }

    #[tokio::test]
    async fn test_the_feed_shows_what_is_unread_under_the_messages_that_place_it() {
        let (mut store, event_ids) = store_with(vec![stranger(); 5]).await;
        read_through(&mut store, &event_ids, 2);

        let run = feed(&mut store, 2).expect("the room has unread messages");

        // The context comes first and in the order it was sent, so the run reads downwards like
        // the conversation it is.
        assert_eq!(rows_of(&run), vec![("2", false), ("3", false), ("4", true), ("5", true),]);
    }

    #[tokio::test]
    async fn test_the_context_is_as_long_as_the_setting_says() {
        let (mut store, event_ids) = store_with(vec![stranger(); 5]).await;
        read_through(&mut store, &event_ids, 3);

        let none = feed(&mut store, 0).expect("the room has unread messages");
        assert_eq!(rows_of(&none), vec![("5", true)]);

        // The context is the three messages before the first unread one, which is "5".
        let long = feed(&mut store, 3).expect("the room has unread messages");
        assert_eq!(rows_of(&long), vec![("2", false), ("3", false), ("4", false), ("5", true),]);
    }

    #[tokio::test]
    async fn test_the_context_can_only_be_as_long_as_the_room_is() {
        let (mut store, event_ids) = store_with(vec![stranger(); 3]).await;
        read_through(&mut store, &event_ids, 1);

        let run = feed(&mut store, 10).expect("the room has unread messages");

        assert_eq!(rows_of(&run), vec![("1", false), ("2", false), ("3", true)]);
    }

    /// The feed is for incoming traffic, and the user's own message is not that.
    #[tokio::test]
    async fn test_the_users_own_unread_messages_are_left_out() {
        let (mut store, event_ids) = store_with(vec![stranger(), own(), stranger()]).await;
        read_through(&mut store, &event_ids, 0);

        let run = feed(&mut store, 0).expect("somebody else's message is still unread");

        assert_eq!(rows_of(&run), vec![("3", true)]);
    }

    /// A room whose only unread messages are the user's own has nothing to say here.
    #[tokio::test]
    async fn test_a_room_the_user_last_spoke_in_has_no_run() {
        let (mut store, event_ids) = store_with(vec![stranger(), own()]).await;
        read_through(&mut store, &event_ids, 0);

        assert!(feed(&mut store, 2).is_none());
    }

    #[tokio::test]
    async fn test_a_read_room_has_no_run() {
        let (mut store, event_ids) = store_with(vec![stranger(); 3]).await;
        read_through(&mut store, &event_ids, 2);

        assert!(feed(&mut store, 2).is_none());
    }

    /// The run starts at the oldest message the client holds and there is more to fetch, so there
    /// may be unread messages the feed cannot show. The title has to say so.
    #[tokio::test]
    async fn test_a_run_that_reaches_the_end_of_what_is_loaded_is_cut_short() {
        let (mut store, _) = store_with(vec![stranger(); 3]).await;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::HaveMore("more".into());

        let run = feed(&mut store, 2).expect("nothing has been read");
        assert!(run.cut_short);

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::Done;

        let run = feed(&mut store, 2).expect("nothing has been read");
        assert!(!run.cut_short, "a room loaded to its start is not cut short");
    }

    #[tokio::test]
    async fn test_the_run_with_the_newest_message_comes_first() {
        let (mut store, event_ids) = store_with(vec![stranger(); 3]).await;
        read_through(&mut store, &event_ids, 0);

        let oldest = feed(&mut store, 0).expect("the room has unread messages");
        let newest = Run {
            rows: vec![],
            newest: key_at(9_000, EventId::new_v1(server_name!("example.com"))),
            cut_short: false,
        };
        let expected = vec![newest.newest.clone(), oldest.newest.clone()];

        let mut runs = vec![oldest, newest];
        order_runs(&mut runs);

        assert_eq!(runs.iter().map(|run| run.newest.clone()).collect::<Vec<_>>(), expected);
    }

    #[tokio::test]
    async fn test_the_filter_narrows_by_body_sender_and_room() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let run = feed(&mut store, 0).expect("the room has unread messages");
        let row = &run.rows[0];

        assert!(row.matches("2"), "the body is matched");
        assert!(row.matches(&stranger().localpart().to_lowercase()), "the sender is matched");
        assert!(row.matches("watercooler"), "the room is matched");
        assert!(!row.matches("zzzzzz"));
    }

    #[tokio::test]
    async fn test_a_row_goes_to_the_message_where_it_lives() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let run = feed(&mut store, 0).expect("the room has unread messages");
        let row = &run.rows[0];

        assert_eq!(row.jump().event_id, event_ids[1]);
        assert_eq!(row.read_at(), (TEST_ROOM1_ID.clone(), event_ids[1].clone()));
    }
}
