//! # Unread Message Feed
//!
//! The `:activity` window is every unread message in every room and followed thread, one row per
//! message, newest first. The other inbox windows answer "where is there something to read"; this
//! one answers "what does it say", which is the question the user actually has when they come back
//! to a client that has been running all day.
//!
//! It is shaped like the [`:search` window][crate::windows::search]: a filter bar over rows of
//! room, sender, time and body, where taking a row goes to that message where it lives. What
//! differs is where the rows come from. A search is one answer from the homeserver, fetched once;
//! these rows are read out of the scrollback the client already holds, and rebuilt on every draw,
//! so a message that arrives while the window is open appears in it.
//!
//! ## One order, and it is time
//!
//! Rows are sorted by when they were sent, newest first, across every room and thread together.
//! Nothing groups them: the window is a stream of what has come in, and a stream that reorders
//! itself to keep rooms together stops answering "what is the newest thing I have not read",
//! which is the question it is open for.
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

use std::collections::HashMap;

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, UserId};

use modalkit::{
    actions::{PromptAction, Promptable},
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::list::{ListCursor, ListItem};

use crate::base::{
    ChatStore,
    IambBufferId,
    IambInfo,
    MessageJump,
    ProgramAction,
    ProgramContext,
    ProgramStore,
    Reach,
    RoomFetchStatus,
};
use crate::config::UserDisplayStyle;
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

/// The most rows the feed draws.
///
/// The list is rebuilt on every draw, so its size is paid for again on every keystroke and every
/// message that arrives. An account that has gone unread for a month would otherwise rebuild many
/// thousands of rows to show the twenty that fit on screen. The newest messages are kept, since
/// those are the ones being read.
const MAX_ROWS: usize = 500;

/// One unread message in the feed.
#[derive(Clone)]
pub struct ActivityItem {
    /// The room the message is in, by the name the user knows it under.
    room: String,

    /// Who sent it, by display name where one is known.
    sender: String,

    /// Who sent it.
    sender_id: OwnedUserId,

    /// When it was sent, in the user's own time zone.
    timestamp: DateTime<LocalTz>,

    /// What it said.
    body: String,

    /// Where the message sits in its room, which is what orders the feed.
    ///
    /// The timestamp alone cannot: two messages sent in the same millisecond in different rooms
    /// would have no order at all, and the feed would shuffle them on every redraw.
    key: MessageKey,

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

/// What one room or thread has waiting in it.
struct Unread {
    /// Its unread messages, as rows.
    rows: Vec<ActivityItem>,

    /// Whether it may reach further back than the client has loaded.
    cut_short: bool,
}

/// Every unread message worth a row, newest first, and what the feed could not reach.
pub fn rows(store: &mut ProgramStore) -> (Vec<ActivityItem>, Reach) {
    let user_id = store.application.settings.profile.user_id.clone();

    let unread = unread_entries(store)
        .into_iter()
        .filter_map(|(room_id, thread)| unread_in(&room_id, thread, &user_id, store))
        .collect::<Vec<_>>();

    let entries_cut_short = unread.iter().filter(|unread| unread.cut_short).count();
    let mut rows = unread.into_iter().flat_map(|unread| unread.rows).collect::<Vec<_>>();

    newest_first(&mut rows);

    // Everything past the cap is older than everything kept, because the sort ran first.
    let rows_left_out = rows.len() > MAX_ROWS;
    rows.truncate(MAX_ROWS);

    seek_missing_names(&rows, store);

    (rows, Reach { entries_cut_short, rows_left_out })
}

/// Ask for the name of anybody on screen the client cannot name yet.
///
/// The feed lists rooms the user has never opened, and a room is only asked for its member list
/// when it is opened, so without this the feed draws a user ID for everybody in those rooms and
/// goes on doing so however long it is left open.
///
/// Only the senders actually on a row are asked about, which bounds this by what fits in the
/// window rather than by how many people are in the rooms. Each one is asked about once: the
/// worker records every user it looked for, found or not, and this skips them from then on.
fn seek_missing_names(rows: &[ActivityItem], store: &mut ProgramStore) {
    // Nothing to look up when the setting draws user IDs anyway.
    if !matches!(
        store.application.settings.tunables.username_display,
        UserDisplayStyle::DisplayName
    ) {
        return;
    }

    let mut wanted: HashMap<OwnedRoomId, Vec<OwnedUserId>> = HashMap::new();

    for row in rows {
        let info = store.application.rooms.get_or_default(row.room_id.clone());

        if info.display_names.get(&row.sender_id).is_some() ||
            !info.display_names_sought.insert(row.sender_id.clone())
        {
            continue;
        }

        wanted.entry(row.room_id.clone()).or_default().push(row.sender_id.clone());
    }

    for (room_id, senders) in wanted {
        store.application.need_load.need_sender_names(room_id, senders);
    }
}

/// Put the messages in the order the feed shows them: the newest one first.
fn newest_first(rows: &mut [ActivityItem]) {
    rows.sort_by(|a, b| b.key.cmp(&a.key));
}

/// The unread messages in one room or thread, oldest first.
fn unread_in(
    room_id: &OwnedRoomId,
    thread: Option<OwnedEventId>,
    user_id: &UserId,
    store: &mut ProgramStore,
) -> Option<Unread> {
    let title = store.application.get_room_title(room_id);
    let ChatStore { rooms, settings, .. } = &mut store.application;
    let info = rooms.get(room_id)?;
    let first_unread = info.first_unread(thread.as_deref(), user_id)?;
    let messages = info.get_thread(thread.as_deref())?;

    let rows = messages
        .range(first_unread.clone()..)
        .filter(|(_, msg)| is_worth_a_row(msg) && msg.sender != user_id)
        .filter_map(|(key, msg)| {
            Some(ActivityItem {
                room: title.clone(),
                sender: settings.get_user_name(&msg.sender, info).to_string(),
                sender_id: msg.sender.clone(),
                timestamp: timestamp(key),
                body: msg.event.body().to_string(),
                key: key.clone(),
                room_id: room_id.clone(),
                thread: thread.clone(),
                event_id: msg.event.event_id()?.to_owned(),
            })
        })
        .collect::<Vec<_>>();

    // Everything unread here is the user's own, so there is nothing incoming to put in a feed of
    // incoming messages.
    if rows.is_empty() {
        return None;
    }

    // A run of unread messages that starts at the oldest message the client holds has nothing
    // above it to show, and there is more to fetch, so the messages it cannot reach are real.
    let oldest_loaded = messages.first_key_value().map(|(key, _)| key == &first_unread);
    let cut_short =
        oldest_loaded.unwrap_or(false) && !matches!(info.fetch_id, RoomFetchStatus::Done);

    Some(Unread { rows, cut_short })
}

/// Whether a message belongs in a feed of what people said.
///
/// A state event is something that happened to a room rather than something somebody said, and a
/// redacted message no longer says anything.
fn is_worth_a_row(msg: &Message) -> bool {
    msg.event.event_id().is_some() && !matches!(msg.event, MessageEvent::State(_))
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

impl ListItem<IambInfo> for ActivityItem {
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

    use crate::base::{EventLocation, Need, RoomFetchStatus};
    use crate::config::UserDisplayStyle;
    use crate::tests::{
        TEST_ROOM1_ID,
        TEST_USER1,
        TEST_USER5,
        key_at,
        mock_room1_message,
        mock_store,
    };

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

    /// What the test room has waiting, as the feed reads it.
    fn waiting(store: &mut ProgramStore) -> Option<Unread> {
        let user_id = store.application.settings.profile.user_id.clone();

        unread_in(&TEST_ROOM1_ID.clone(), None, &user_id, store)
    }

    /// The bodies of some rows, in the order they are in.
    fn bodies(rows: &[ActivityItem]) -> Vec<&str> {
        rows.iter().map(|row| row.body.as_str()).collect()
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
    async fn test_a_room_offers_everything_past_the_read_receipt() {
        let (mut store, event_ids) = store_with(vec![stranger(); 5]).await;
        read_through(&mut store, &event_ids, 2);

        let unread = waiting(&mut store).expect("the room has unread messages");

        assert_eq!(bodies(&unread.rows), vec!["4", "5"]);
    }

    /// The whole point of the window: the newest message is the first row, whichever room it is
    /// in, so the feed reads as a stream of what has come in.
    #[test]
    fn test_the_newest_message_comes_first_whatever_room_it_is_in() {
        let rows = vec![
            row_at(1, "ops", "older"),
            row_at(9, "general", "newest"),
            row_at(5, "ops", "middle"),
        ];
        let mut rows = rows;

        newest_first(&mut rows);

        assert_eq!(bodies(&rows), vec!["newest", "middle", "older"]);
    }

    /// Two messages sent in the same millisecond still have an order, and it is the same one on
    /// every redraw.
    #[test]
    fn test_messages_sent_in_the_same_millisecond_keep_a_stable_order() {
        let first = row_at(7, "ops", "one");
        let second = row_at(7, "general", "two");

        let mut one_way = vec![first.clone(), second.clone()];
        let mut other_way = vec![second, first];

        newest_first(&mut one_way);
        newest_first(&mut other_way);

        assert_eq!(bodies(&one_way), bodies(&other_way));
    }

    /// A row built by hand, sent `millis` since the epoch.
    fn row_at(millis: u64, room: &str, body: &str) -> ActivityItem {
        let event_id = EventId::new_v1(server_name!("example.com"));

        ActivityItem {
            room: room.to_string(),
            sender: "somebody".to_string(),
            sender_id: TEST_USER1.clone(),
            timestamp: Default::default(),
            body: body.to_string(),
            key: key_at(millis, event_id.clone()),
            room_id: TEST_ROOM1_ID.clone(),
            thread: None,
            event_id,
        }
    }

    /// The feed is for incoming traffic, and the user's own message is not that.
    #[tokio::test]
    async fn test_the_users_own_unread_messages_are_left_out() {
        let (mut store, event_ids) = store_with(vec![stranger(), own(), stranger()]).await;
        read_through(&mut store, &event_ids, 0);

        let unread = waiting(&mut store).expect("somebody else's message is still unread");

        assert_eq!(bodies(&unread.rows), vec!["3"]);
    }

    /// A room whose only unread messages are the user's own has nothing to say here.
    #[tokio::test]
    async fn test_a_room_the_user_last_spoke_in_offers_nothing() {
        let (mut store, event_ids) = store_with(vec![stranger(), own()]).await;
        read_through(&mut store, &event_ids, 0);

        assert!(waiting(&mut store).is_none());
    }

    #[tokio::test]
    async fn test_a_read_room_offers_nothing() {
        let (mut store, event_ids) = store_with(vec![stranger(); 3]).await;
        read_through(&mut store, &event_ids, 2);

        assert!(waiting(&mut store).is_none());
    }

    /// The unread messages start at the oldest message the client holds and there is more to
    /// fetch, so there may be unread messages the feed cannot show. The title has to say so.
    #[tokio::test]
    async fn test_a_room_that_reaches_the_end_of_what_is_loaded_is_cut_short() {
        let (mut store, _) = store_with(vec![stranger(); 3]).await;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::HaveMore("more".into());

        let unread = waiting(&mut store).expect("nothing has been read");
        assert!(unread.cut_short);

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::Done;

        let unread = waiting(&mut store).expect("nothing has been read");
        assert!(!unread.cut_short, "a room loaded to its start is not cut short");
    }

    /// The feed names people the way the rest of the client does, rather than working it out for
    /// itself: `username_display` and a `[settings.users]` override both decide what it draws.
    #[tokio::test]
    async fn test_a_sender_is_named_the_way_the_settings_say() {
        let (mut store, event_ids) = store_with(vec![stranger(), TEST_USER5.clone()]).await;
        read_through(&mut store, &event_ids, 0);

        // mock_settings displays usernames, and overrides this one user's name.
        let unread = waiting(&mut store).expect("the room has unread messages");
        assert_eq!(unread.rows[0].sender, "USER 5");

        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(TEST_USER5.clone(), Some("Ada Lovelace".into()));

        // The override still wins over the display name, as it does in a room.
        let unread = waiting(&mut store).expect("the room has unread messages");
        assert_eq!(unread.rows[0].sender, "USER 5");
    }

    /// A room whose members have not been loaded has no display names to draw, and the user ID is
    /// what is left to tell people apart.
    #[tokio::test]
    async fn test_a_sender_with_no_display_name_yet_is_named_by_their_user_id() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let unread = waiting(&mut store).expect("the room has unread messages");
        assert_eq!(unread.rows[0].sender, stranger().as_str());

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(stranger(), Some("Ada Lovelace".into()));

        // Loading the members is what resolves it, and the feed picks that up on its next build.
        let unread = waiting(&mut store).expect("the room has unread messages");
        assert_eq!(unread.rows[0].sender, "Ada Lovelace");
    }

    /// Every sender the feed draws as a user ID is one it wants a name for.
    #[tokio::test]
    async fn test_a_sender_without_a_name_is_asked_about() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let unread = waiting(&mut store).expect("the room has unread messages");
        seek_missing_names(&unread.rows, &mut store);

        let needs = std::mem::take(&mut store.application.need_load)
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(needs, vec![(TEST_ROOM1_ID.clone(), Need {
            members: false,
            messages: None,
            senders: Some(vec![stranger()]),
        })]);
    }

    /// The feed rebuilds on every draw, so asking has to be a once-only thing or the same missing
    /// name is asked for forever.
    #[tokio::test]
    async fn test_a_sender_is_only_asked_about_once() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let unread = waiting(&mut store).expect("the room has unread messages");
        seek_missing_names(&unread.rows, &mut store);
        let _ = std::mem::take(&mut store.application.need_load);

        // A second build of the same rows, as the next draw would do.
        seek_missing_names(&unread.rows, &mut store);

        assert_eq!(store.application.need_load.rooms(), 0);
    }

    #[tokio::test]
    async fn test_a_sender_who_already_has_a_name_is_not_asked_about() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(stranger(), Some("Ada Lovelace".into()));

        let unread = waiting(&mut store).expect("the room has unread messages");
        seek_missing_names(&unread.rows, &mut store);

        assert_eq!(store.application.need_load.rooms(), 0);
    }

    /// Nothing is worth asking for when the setting draws user IDs anyway.
    #[tokio::test]
    async fn test_nobody_is_asked_about_when_user_ids_are_what_is_drawn() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        // mock_settings displays usernames.
        let unread = waiting(&mut store).expect("the room has unread messages");
        seek_missing_names(&unread.rows, &mut store);

        assert_eq!(store.application.need_load.rooms(), 0);
    }

    #[tokio::test]
    async fn test_the_filter_narrows_by_body_sender_and_room() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let unread = waiting(&mut store).expect("the room has unread messages");
        let row = &unread.rows[0];

        assert!(row.matches("2"), "the body is matched");
        assert!(row.matches(&stranger().localpart().to_lowercase()), "the sender is matched");
        assert!(row.matches("watercooler"), "the room is matched");
        assert!(!row.matches("zzzzzz"));
    }

    #[tokio::test]
    async fn test_a_row_goes_to_the_message_where_it_lives() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let unread = waiting(&mut store).expect("the room has unread messages");
        let row = &unread.rows[0];

        assert_eq!(row.jump().event_id, event_ids[1]);
        assert_eq!(row.read_at(), (TEST_ROOM1_ID.clone(), event_ids[1].clone()));
    }
}
