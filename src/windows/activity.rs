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
const TIME_COLUMN_WIDTH: usize = 16;

/// How a row's timestamp is drawn.
///
/// The full date, because unread is not the same as recent: a room can go unread for weeks, and a
/// weekday and a time of day cannot tell this Tuesday from one three weeks ago. Rows in date order
/// then read as though they were in no order at all, which is worse than the four columns the
/// date costs.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M";

/// The narrowest the body column is worth drawing at.
const MIN_BODY_COLUMN_WIDTH: usize = 16;

/// The narrowest the room and sender columns are drawn, when the terminal has no room for more.
const MIN_LABEL_COLUMN_WIDTH: usize = 8;

/// The most rows the feed draws.
///
/// The list is rebuilt on every draw, so its size is paid for again on every keystroke and every
/// message that arrives, and an account that has gone unread for a month can hold far more unread
/// messages than anybody will scroll through. The newest are kept, since those are the ones being
/// read, and the title says when older ones were left out.
///
/// Only the messages that survive this are built into rows, so the cap bounds the drawing work
/// rather than the finding work.
const MAX_ROWS: usize = 2000;

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

/// One unread message, before there is any reason to build a row out of it.
///
/// The feed can hold far more unread messages than it will ever draw, and finding which ones are
/// newest costs nothing but their keys. Bodies, names and titles are built only for the messages
/// that survive [MAX_ROWS], so a backlog of thousands costs a sort rather than thousands of rows.
#[derive(Clone)]
struct UnreadMessage {
    room_id: OwnedRoomId,
    thread: Option<OwnedEventId>,
    key: MessageKey,
}

/// What one room or thread has waiting in it.
struct Unread {
    /// Its unread messages.
    messages: Vec<UnreadMessage>,

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
    let mut messages = unread.into_iter().flat_map(|unread| unread.messages).collect::<Vec<_>>();

    newest_first(&mut messages);

    // Everything past the cap is older than everything kept, because the sort ran first.
    let rows_left_out = messages.len() > MAX_ROWS;
    messages.truncate(MAX_ROWS);

    let rows = rows_for(messages, store);

    seek_missing_names(&rows, store);

    (rows, Reach { entries_cut_short, rows_left_out })
}

/// Put the messages in the order the feed shows them: the newest one first.
fn newest_first(messages: &mut [UnreadMessage]) {
    messages.sort_by(|a, b| b.key.cmp(&a.key));
}

/// Build the row for each message, in the order they were given.
fn rows_for(messages: Vec<UnreadMessage>, store: &mut ProgramStore) -> Vec<ActivityItem> {
    let mut titles = HashMap::new();

    for message in &messages {
        titles
            .entry(message.room_id.clone())
            .or_insert_with(|| store.application.get_room_title(&message.room_id));
    }

    let ChatStore { rooms, settings, .. } = &store.application;

    messages
        .into_iter()
        .filter_map(|message| {
            let info = rooms.get(&message.room_id)?;
            let msg = info.get_thread(message.thread.as_deref())?.get(&message.key)?;

            Some(ActivityItem {
                room: titles.get(&message.room_id)?.clone(),
                sender: settings.get_user_name(&msg.sender, info).to_string(),
                sender_id: msg.sender.clone(),
                timestamp: timestamp(&message.key),
                body: msg.event.body().to_string(),
                room_id: message.room_id.clone(),
                thread: message.thread.clone(),
                event_id: msg.event.event_id()?.to_owned(),
            })
        })
        .collect()
}

/// The unread messages in one room or thread, oldest first.
fn unread_in(
    room_id: &OwnedRoomId,
    thread: Option<OwnedEventId>,
    user_id: &UserId,
    store: &mut ProgramStore,
) -> Option<Unread> {
    let info = store.application.rooms.get(room_id)?;
    let first_unread = info.first_unread(thread.as_deref(), user_id)?;
    let messages = info.get_thread(thread.as_deref())?;

    let unread = messages
        .range(first_unread.clone()..)
        .filter(|(_, msg)| is_worth_a_row(msg) && msg.sender != user_id)
        .map(|(key, _)| {
            UnreadMessage {
                room_id: room_id.clone(),
                thread: thread.clone(),
                key: key.clone(),
            }
        })
        .collect::<Vec<_>>();

    // Everything unread here is the user's own, so there is nothing incoming to put in a feed of
    // incoming messages.
    if unread.is_empty() {
        return None;
    }

    // A run of unread messages that starts at the oldest message the client holds has nothing
    // above it to show, and there is more to fetch, so the messages it cannot reach are real.
    let oldest_loaded = messages.first_key_value().map(|(key, _)| key == &first_unread);
    let cut_short =
        oldest_loaded.unwrap_or(false) && !matches!(info.fetch_id, RoomFetchStatus::Done);

    Some(Unread { messages: unread, cut_short })
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

        for (position, sender) in senders.into_iter().enumerate() {
            let event_id = EventId::new_v1(server_name!("example.com"));
            let key = key_at(position as u64 + 1, event_id.clone());
            let content = RoomMessageEventContent::text_plain((position + 1).to_string());
            let message = mock_room1_message(content, sender, key.clone());

            info.keys
                .insert(event_id.clone(), EventLocation::Message(None, key.clone()));
            info.get_thread_mut(None).insert(key, message);
            event_ids.push(event_id);
        }

        (store, event_ids)
    }

    /// Add a reply to the thread rooted at `root`, sent by somebody other than the user.
    fn reply_in_thread(
        store: &mut ProgramStore,
        root: &EventId,
        body: &str,
        millis: u64,
    ) -> OwnedEventId {
        let event_id = EventId::new_v1(server_name!("example.com"));
        let key = key_at(millis, event_id.clone());
        let content = RoomMessageEventContent::text_plain(body);
        let message = mock_room1_message(content, stranger(), key.clone());
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());

        info.keys
            .insert(event_id.clone(), EventLocation::Message(Some(root.to_owned()), key.clone()));
        info.get_thread_mut(Some(root.to_owned())).insert(key, message);

        event_id
    }

    /// Read the room up to and including the message at `position`.
    fn read_through(store: &mut ProgramStore, event_ids: &[OwnedEventId], position: usize) {
        let user_id = store.application.settings.profile.user_id.clone();
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());

        info.set_receipt(ReceiptThread::Main, user_id, event_ids[position].clone());
    }

    /// What the test room's main scrollback has waiting, as the feed reads it.
    fn waiting(store: &mut ProgramStore) -> Option<Unread> {
        let user_id = store.application.settings.profile.user_id.clone();

        unread_in(&TEST_ROOM1_ID.clone(), None, &user_id, store)
    }

    /// What the test room's main scrollback has waiting, drawn as rows.
    fn waiting_rows(store: &mut ProgramStore) -> Vec<ActivityItem> {
        let unread = waiting(store).expect("the test room has unread messages to draw");

        rows_for(unread.messages, store)
    }

    /// The bodies of some rows, in the order they are in.
    fn bodies(rows: &[ActivityItem]) -> Vec<&str> {
        rows.iter().map(|row| row.body.as_str()).collect()
    }

    /// An unread message sent `millis` since the epoch, with nothing else about it that matters.
    fn unread_at(millis: u64) -> UnreadMessage {
        UnreadMessage {
            room_id: TEST_ROOM1_ID.clone(),
            thread: None,
            key: key_at(millis, EventId::new_v1(server_name!("example.com"))),
        }
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

        let rows = waiting_rows(&mut store);
        let drawn = bodies(&rows);

        assert_eq!(
            drawn,
            vec!["4", "5"],
            "the receipt sits on the third of five messages, so the fourth and fifth are what is \
             left unread, but the feed offered {drawn:?}"
        );
    }

    /// The whole point of the window: the newest message is the first row, whichever room it is
    /// in, so the feed reads as a stream of what has come in.
    #[test]
    fn test_the_newest_message_comes_first_whatever_room_it_is_in() {
        let mut messages = vec![unread_at(1), unread_at(9), unread_at(5)];

        newest_first(&mut messages);

        let order = messages
            .iter()
            .map(|message| u64::from(message.key.ts.0.0))
            .collect::<Vec<_>>();

        assert_eq!(
            order,
            vec![9, 5, 1],
            "messages must be ordered by when they were sent, newest first, but they came out in \
             the order {order:?}"
        );
    }

    /// Two messages sent in the same millisecond still have an order, and it is the same one on
    /// every redraw, because the sort falls through to the event ID rather than giving up.
    #[test]
    fn test_messages_sent_in_the_same_millisecond_keep_a_stable_order() {
        let first = unread_at(7);
        let second = unread_at(7);

        let mut one_way = vec![first.clone(), second.clone()];
        let mut other_way = vec![second, first];

        newest_first(&mut one_way);
        newest_first(&mut other_way);

        let one_way = one_way.iter().map(|message| message.key.clone()).collect::<Vec<_>>();
        let other_way = other_way.iter().map(|message| message.key.clone()).collect::<Vec<_>>();

        assert_eq!(
            one_way, other_way,
            "two messages sharing a timestamp must sort the same way whichever order they arrive \
             in, or the feed shuffles them on every redraw"
        );
    }

    /// A thread the user has never posted in is still somewhere unread messages can be, and a
    /// feed of everything going on cannot leave it out.
    #[tokio::test]
    async fn test_a_thread_the_user_never_posted_in_offers_its_replies() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 1);
        reply_in_thread(&mut store, &event_ids[0], "in the thread", 20);

        let user_id = store.application.settings.profile.user_id.clone();
        let root = event_ids[0].clone();
        let unread = unread_in(&TEST_ROOM1_ID.clone(), Some(root), &user_id, &mut store)
            .expect("the thread has an unread reply");
        let rows = rows_for(unread.messages, &mut store);
        let drawn = bodies(&rows);

        assert_eq!(
            drawn,
            vec!["in the thread"],
            "the reply is unread and was not sent by the user, so it belongs in the feed whether \
             or not the user follows the thread, but the feed offered {drawn:?}"
        );
    }

    /// The feed is for incoming traffic, and the user's own message is not that.
    #[tokio::test]
    async fn test_the_users_own_unread_messages_are_left_out() {
        let (mut store, event_ids) = store_with(vec![stranger(), own(), stranger()]).await;
        read_through(&mut store, &event_ids, 0);

        let rows = waiting_rows(&mut store);
        let drawn = bodies(&rows);

        assert_eq!(
            drawn,
            vec!["3"],
            "the second message is the user's own, so only the third is incoming traffic, but the \
             feed offered {drawn:?}"
        );
    }

    /// A room whose only unread messages are the user's own has nothing to say here.
    #[tokio::test]
    async fn test_a_room_the_user_last_spoke_in_offers_nothing() {
        let (mut store, event_ids) = store_with(vec![stranger(), own()]).await;
        read_through(&mut store, &event_ids, 0);

        assert!(
            waiting(&mut store).is_none(),
            "everything past the receipt is the user's own message, so the room has no incoming \
             traffic and must not appear in the feed at all"
        );
    }

    #[tokio::test]
    async fn test_a_read_room_offers_nothing() {
        let (mut store, event_ids) = store_with(vec![stranger(); 3]).await;
        read_through(&mut store, &event_ids, 2);

        assert!(
            waiting(&mut store).is_none(),
            "the receipt sits on the last message, so nothing is unread and the room must not be \
             in the feed"
        );
    }

    /// The unread messages start at the oldest message the client holds and there is more to
    /// fetch, so there may be unread messages the feed cannot show. The title has to say so.
    #[tokio::test]
    async fn test_a_room_that_reaches_the_end_of_what_is_loaded_is_cut_short() {
        let (mut store, _) = store_with(vec![stranger(); 3]).await;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::HaveMore("more".into());

        let unread = waiting(&mut store).expect("nothing has been read, so everything is unread");
        assert!(
            unread.cut_short,
            "the oldest unread message is the oldest message loaded and the room has more history \
             to fetch, so the feed cannot claim to be showing all of it"
        );

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.fetch_id = RoomFetchStatus::Done;

        let unread = waiting(&mut store).expect("nothing has been read, so everything is unread");
        assert!(
            !unread.cut_short,
            "the room is loaded back to its very first message, so there is nothing out of reach \
             and nothing to warn about"
        );
    }

    /// The feed names people the way the rest of the client does, rather than working it out for
    /// itself: `username_display` and a `[settings.users]` override both decide what it draws.
    #[tokio::test]
    async fn test_a_sender_is_named_the_way_the_settings_say() {
        let (mut store, event_ids) = store_with(vec![stranger(), TEST_USER5.clone()]).await;
        read_through(&mut store, &event_ids, 0);

        let drawn = waiting_rows(&mut store)[0].sender.clone();
        assert_eq!(
            drawn, "USER 5",
            "the settings override this user's name, so the feed must draw the override rather \
             than their user ID, but it drew {drawn:?}"
        );

        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;
        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(TEST_USER5.clone(), Some("Ada Lovelace".into()));

        let drawn = waiting_rows(&mut store)[0].sender.clone();
        assert_eq!(
            drawn, "USER 5",
            "an override beats a display name in a room, and the feed must not disagree with the \
             room, but it drew {drawn:?}"
        );
    }

    /// A room whose members have not been loaded has no display names to draw, and the user ID is
    /// what is left to tell people apart.
    #[tokio::test]
    async fn test_a_sender_with_no_display_name_yet_is_named_by_their_user_id() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let drawn = waiting_rows(&mut store)[0].sender.clone();
        assert_eq!(
            drawn,
            stranger().as_str(),
            "nothing knows this user's display name yet, so the user ID is all the feed can draw, \
             but it drew {drawn:?}"
        );

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(stranger(), Some("Ada Lovelace".into()));

        let drawn = waiting_rows(&mut store)[0].sender.clone();
        assert_eq!(
            drawn, "Ada Lovelace",
            "loading the members is what resolves a name, and the feed rebuilds on every draw, so \
             the very next build must use it, but it drew {drawn:?}"
        );
    }

    /// Every sender the feed draws as a user ID is one it wants a name for.
    #[tokio::test]
    async fn test_a_sender_without_a_name_is_asked_about() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let rows = waiting_rows(&mut store);
        seek_missing_names(&rows, &mut store);

        let needs = std::mem::take(&mut store.application.need_load)
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(
            needs,
            vec![(TEST_ROOM1_ID.clone(), Need {
                members: false,
                messages: None,
                senders: Some(vec![stranger()]),
            })],
            "the feed drew a user ID for this sender, so it must ask for their name, and for \
             their name alone rather than the room's whole member list, but it asked for {needs:?}"
        );
    }

    /// The feed rebuilds on every draw, so asking has to be a once-only thing or the same missing
    /// name is asked for forever.
    #[tokio::test]
    async fn test_a_sender_is_only_asked_about_once() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let rows = waiting_rows(&mut store);
        seek_missing_names(&rows, &mut store);
        let _ = std::mem::take(&mut store.application.need_load);

        seek_missing_names(&rows, &mut store);

        let asked_again = store.application.need_load.rooms();
        assert_eq!(
            asked_again, 0,
            "the name was already asked for once, so the next draw must ask for nothing, but it \
             asked about {asked_again} room(s)"
        );
    }

    #[tokio::test]
    async fn test_a_sender_who_already_has_a_name_is_not_asked_about() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);
        store.application.settings.tunables.username_display = UserDisplayStyle::DisplayName;

        let info = store.application.rooms.get_or_default(TEST_ROOM1_ID.clone());
        info.display_names.set(stranger(), Some("Ada Lovelace".into()));

        let rows = waiting_rows(&mut store);
        seek_missing_names(&rows, &mut store);

        let asked = store.application.need_load.rooms();
        assert_eq!(
            asked, 0,
            "this sender already has a name to draw, so nothing needs looking up, but the feed \
             asked about {asked} room(s)"
        );
    }

    /// Nothing is worth asking for when the setting draws user IDs anyway.
    #[tokio::test]
    async fn test_nobody_is_asked_about_when_user_ids_are_what_is_drawn() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let rows = waiting_rows(&mut store);
        seek_missing_names(&rows, &mut store);

        let asked = store.application.need_load.rooms();
        assert_eq!(
            asked, 0,
            "the settings draw user IDs, so a display name would never be shown even if it were \
             fetched, but the feed asked about {asked} room(s)"
        );
    }

    #[tokio::test]
    async fn test_the_filter_narrows_by_body_sender_and_room() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let rows = waiting_rows(&mut store);
        let row = &rows[0];

        assert!(row.matches("2"), "the body is matched, and this row's body is \"2\"");
        assert!(
            row.matches(&stranger().localpart().to_lowercase()),
            "the sender is matched, so that the feed can be narrowed to one person"
        );
        assert!(
            row.matches("watercooler"),
            "the room name is matched, so that the feed can be narrowed to one room"
        );
        assert!(
            !row.matches("zzzzzz"),
            "text that appears in no column must match nothing, or the filter narrows nothing"
        );
    }

    #[tokio::test]
    async fn test_a_row_goes_to_the_message_where_it_lives() {
        let (mut store, event_ids) = store_with(vec![stranger(); 2]).await;
        read_through(&mut store, &event_ids, 0);

        let rows = waiting_rows(&mut store);
        let row = &rows[0];

        assert_eq!(
            row.jump().event_id,
            event_ids[1],
            "taking a row must go to the message the row is drawn from"
        );
        assert_eq!(
            row.read_at(),
            (TEST_ROOM1_ID.clone(), event_ids[1].clone()),
            "reading a row must move the receipt to the message the row is drawn from"
        );
    }
}
