//! # Full-Text Message Search
//!
//! The homeserver indexes the messages it can read, and answers
//! [`POST /_matrix/client/v3/search`][spec] with the ones that match a term. This module builds
//! that request and turns the answer into [MessageHit]s. The request goes out from
//! [the worker][crate::worker], and the hits are what the
//! [`:search` window][crate::windows::search] lists.
//!
//! The server cannot read an encrypted room, so it cannot index one either. The answer to that is
//! a second source: an index that matrix-sdk keeps on this machine, over the text after it is
//! decrypted. See [crate::config::LocalIndex] for what that costs and why it is off by default.
//!
//! One search reads both sources. A room the server can read goes to the server, and an encrypted
//! room goes to the local index, so no message can come back twice. The two sources rank their
//! answers by measures that cannot be compared -- the server has its own, and the index has BM25
//! -- so the merged list is put in time order instead, by [sort_newest_first].
//!
//! The local source is incomplete by construction: it holds only what was indexed, and a room that
//! was never indexed answers with nothing. Nothing is indistinguishable from no match unless the
//! interface says which it was, so every search carries a [Coverage] saying how many rooms it
//! could not look at.
//!
//! [spec]: https://spec.matrix.org/v1.18/client-server-api/#post_matrixclientv3search
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::{
    api::client::{
        filter::RoomEventFilter,
        search::search_events::v3::{
            Categories,
            Criteria,
            OrderBy,
            Request as SearchRequest,
            ResultRoomEvents,
            SearchKeys,
            SearchResult,
        },
    },
    events::{
        room::message::Relation,
        AnyMessageLikeEvent,
        AnySyncMessageLikeEvent,
        AnySyncTimelineEvent,
        AnyTimelineEvent,
        MessageLikeEvent,
        SyncMessageLikeEvent,
    },
    MilliSecondsSinceUnixEpoch,
    OwnedEventId,
    OwnedRoomId,
    OwnedUserId,
    RoomId,
    UInt,
};

/// How many hits one request asks the homeserver for.
///
/// The server is free to return fewer, and says so by handing back a `next_batch` to ask again
/// with.
pub const HITS_PER_REQUEST: u32 = 100;

/// How many hits are kept.
///
/// The window filters the hits it was given without going back to the network, so the whole set
/// has to be in hand before the user types anything. That argues for fetching everything, and a
/// common word matches thousands of messages, which argues for stopping. This is the compromise:
/// enough that the filter has something real to work on, few enough that the search returns in a
/// moment. The hits are put in order before the cap is applied, so what is dropped is the oldest.
pub const MAX_HITS: usize = 200;

/// One message the homeserver found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageHit {
    /// The room the message was sent in.
    pub room_id: OwnedRoomId,

    /// The message itself.
    pub event_id: OwnedEventId,

    /// The thread the message is in, when it is in one.
    ///
    /// A message in a thread is not in the room's own scrollback, so going to it means opening
    /// the thread rather than the room.
    pub thread: Option<OwnedEventId>,

    /// Who sent it.
    pub sender: OwnedUserId,

    /// When the homeserver received it.
    pub timestamp: MilliSecondsSinceUnixEpoch,

    /// What it said, as one line.
    pub body: String,
}

/// Everything one search found, and how much of the account it was able to look at.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchResults {
    /// The messages found, newest first.
    pub hits: Vec<MessageHit>,

    /// What the search could not look at.
    pub coverage: Coverage,
}

/// The rooms a search could not look at.
///
/// A search tool that returns an empty list for a room it never opened is worse than one that
/// finds nothing, because the user cannot tell the two apart and stops trusting either. So the
/// count travels with the results and the window shows it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Coverage {
    /// How many encrypted rooms have no local index to ask.
    pub unindexed_rooms: usize,

    /// Whether the user has turned the local index on.
    ///
    /// This separates "you have not enabled this" from "these rooms are enabled but empty",
    /// which need different things done about them.
    pub local_index_enabled: bool,
}

impl Coverage {
    /// What to tell the user about the rooms this search did not reach, if anything.
    pub fn note(&self) -> Option<String> {
        if self.unindexed_rooms == 0 {
            return None;
        }

        let rooms = self.unindexed_rooms;
        let plural = if rooms == 1 { "room" } else { "rooms" };

        if self.local_index_enabled {
            Some(format!("{rooms} encrypted {plural} not indexed yet"))
        } else {
            Some(format!("{rooms} encrypted {plural} not searched: local_index is off"))
        }
    }
}

/// One message the local index found, when it is a message worth showing.
///
/// The index stores an event identifier and nothing else of the message, so the caller has already
/// loaded the event back. The room identifier comes from the caller because a
/// [matrix_sdk::deserialized_responses::TimelineEvent] holds the sync form of an event, which
/// carries no room.
pub fn local_hit(room_id: &RoomId, event: &TimelineEvent) -> Option<MessageHit> {
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(message),
    )) = event.raw().deserialize().ok()?
    else {
        return None;
    };

    let thread = match &message.content.relates_to {
        Some(Relation::Thread(thread)) => Some(thread.event_id.clone()),
        _ => None,
    };

    Some(MessageHit {
        room_id: room_id.to_owned(),
        event_id: message.event_id,
        thread,
        sender: message.sender,
        timestamp: message.origin_server_ts,
        body: one_line(message.content.body()),
    })
}

/// Ask the homeserver for the messages that match `term`.
///
/// Only `content.body` is searched, because that is the text the user remembers typing. The order
/// asked for is recency, since a message somebody is looking for is far more often a recent one
/// than the best-ranked one out of years of chat. What comes back still has to be put in order by
/// [sort_newest_first]; see there for why.
///
/// `next_batch` continues an earlier search, and is the token that search handed back.
pub fn search_request(term: String, next_batch: Option<String>) -> SearchRequest {
    let mut filter = RoomEventFilter::default();
    filter.limit = Some(UInt::from(HITS_PER_REQUEST));

    let mut criteria = Criteria::new(term);
    criteria.keys = Some(vec![SearchKeys::ContentBody]);
    criteria.order_by = Some(OrderBy::Recent);
    criteria.filter = filter;

    let mut categories = Categories::new();
    categories.room_events = Some(criteria);

    let mut request = SearchRequest::new(categories);
    request.next_batch = next_batch;

    request
}

/// The messages in one page of results, in the order the homeserver gave them.
///
/// A result that is not a message the user can read is dropped rather than shown as an empty row.
/// The homeserver can return a redacted message, a state event, or an event this version of iamb
/// cannot parse, and none of those are something to jump to.
pub fn hits(results: &ResultRoomEvents) -> Vec<MessageHit> {
    results.results.iter().filter_map(hit).collect()
}

/// Put the newest hit first.
///
/// The homeserver's "recent" ordering is per room, not across the search: it returns each room's
/// matches newest first and then puts the rooms one after another. Verified against Synapse, where
/// a term found in 21 rooms came back as 21 descending runs rather than one.
///
/// The user reads one list, so the list needs one order. The cap on how many hits are kept also
/// depends on it: without this, dropping the tail would drop whichever rooms happened to come
/// last rather than the oldest messages.
pub fn sort_newest_first(hits: &mut [MessageHit]) {
    hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
}

/// One result, when it is a message.
fn hit(result: &SearchResult) -> Option<MessageHit> {
    let event = result.result.as_ref()?.deserialize().ok()?;

    let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
        MessageLikeEvent::Original(message),
    )) = event
    else {
        return None;
    };

    let thread = match &message.content.relates_to {
        Some(Relation::Thread(thread)) => Some(thread.event_id.clone()),
        _ => None,
    };

    Some(MessageHit {
        room_id: message.room_id,
        event_id: message.event_id,
        thread,
        sender: message.sender,
        timestamp: message.origin_server_ts,
        body: one_line(message.content.body()),
    })
}

/// Squeeze `body` onto one line, since a row of the list is one line.
///
/// A newline drawn into a list row is not a line break, it is a gap in the text, so every run of
/// whitespace becomes one space.
fn one_line(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{event_id, room_id, serde::Raw, user_id};
    use serde_json::{from_value, json, Value};

    /// The `room_events` out of a response body, as the worker gets it.
    fn results(results: Value) -> ResultRoomEvents {
        from_value(json!({ "count": 1, "results": results })).unwrap()
    }

    /// A hit that is nothing but its timestamp, for checking the order.
    fn at(millis: u64) -> MessageHit {
        MessageHit {
            room_id: room_id!("!general:example.com").to_owned(),
            event_id: event_id!("$found:example.com").to_owned(),
            thread: None,
            sender: user_id!("@dan:example.com").to_owned(),
            timestamp: MilliSecondsSinceUnixEpoch(UInt::new(millis).unwrap()),
            body: "deploy".to_string(),
        }
    }

    fn message(extra: Value) -> Value {
        let mut content = json!({ "msgtype": "m.text", "body": "let us deploy on friday" });
        let object = content.as_object_mut().unwrap();

        for (key, value) in extra.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }

        json!({
            "rank": 0.5,
            "result": {
                "type": "m.room.message",
                "event_id": "$found:example.com",
                "room_id": "!general:example.com",
                "sender": "@dan:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": content,
            }
        })
    }

    #[test]
    fn test_a_hit_keeps_everything_needed_to_show_and_reach_it() {
        let hits = hits(&results(json!([message(json!({}))])));

        assert_eq!(hits, vec![MessageHit {
            room_id: room_id!("!general:example.com").to_owned(),
            event_id: event_id!("$found:example.com").to_owned(),
            thread: None,
            sender: user_id!("@dan:example.com").to_owned(),
            timestamp: MilliSecondsSinceUnixEpoch(UInt::new(1_700_000_000_000).unwrap()),
            body: "let us deploy on friday".to_string(),
        }]);
    }

    #[test]
    fn test_a_hit_in_a_thread_remembers_the_thread() {
        // Otherwise going to the hit opens a room whose scrollback does not hold it.
        let threaded = message(json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root:example.com",
            }
        }));

        assert_eq!(
            hits(&results(json!([threaded])))[0].thread,
            Some(event_id!("$root:example.com").to_owned())
        );
    }

    #[test]
    fn test_a_result_that_is_not_a_readable_message_is_dropped() {
        // A state event and a redacted message both come back with no body to show.
        let state = json!({
            "rank": 0.5,
            "result": {
                "type": "m.room.topic",
                "event_id": "$topic:example.com",
                "room_id": "!general:example.com",
                "sender": "@dan:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": { "topic": "deploys" },
            }
        });
        let redacted = json!({
            "rank": 0.5,
            "result": {
                "type": "m.room.message",
                "event_id": "$gone:example.com",
                "room_id": "!general:example.com",
                "sender": "@dan:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": {},
                "unsigned": { "redacted_because": {
                    "type": "m.room.redaction",
                    "event_id": "$redaction:example.com",
                    "sender": "@dan:example.com",
                    "origin_server_ts": 1_700_000_001_000u64,
                    "content": {},
                }},
            }
        });

        assert!(hits(&results(json!([state, redacted]))).is_empty());
    }

    #[test]
    fn test_a_body_is_squeezed_onto_one_line() {
        let multiline = message(json!({ "body": "one\n\ntwo   three\tfour" }));

        assert_eq!(hits(&results(json!([multiline])))[0].body, "one two three four");
    }

    #[test]
    fn test_hits_are_put_in_order_across_every_room() {
        // The homeserver returns each room's matches newest first, one room after another, so a
        // hit from a quiet room can arrive before a much newer hit from a busy one.
        let mut hits = vec![
            at(1_700_000_003_000),
            at(1_700_000_001_000),
            at(1_700_000_004_000),
            at(1_700_000_002_000),
        ];

        sort_newest_first(&mut hits);

        let order = hits.iter().map(|hit| hit.timestamp.0).collect::<Vec<_>>();
        let newest_first = vec![
            UInt::new(1_700_000_004_000).unwrap(),
            UInt::new(1_700_000_003_000).unwrap(),
            UInt::new(1_700_000_002_000).unwrap(),
            UInt::new(1_700_000_001_000).unwrap(),
        ];

        assert_eq!(order, newest_first);
    }

    #[test]
    fn test_the_request_searches_message_bodies_newest_first() {
        let request = search_request("deploy".to_string(), None);
        let criteria = request.search_categories.room_events.unwrap();

        assert_eq!(criteria.search_term, "deploy");
        assert_eq!(criteria.keys, Some(vec![SearchKeys::ContentBody]));
        assert_eq!(criteria.order_by, Some(OrderBy::Recent));
        assert_eq!(criteria.filter.limit, Some(UInt::from(HITS_PER_REQUEST)));
    }

    #[test]
    fn test_a_search_that_reached_every_room_says_nothing() {
        let reached = Coverage { unindexed_rooms: 0, local_index_enabled: true };

        assert_eq!(reached.note(), None);
    }

    #[test]
    fn test_a_search_that_missed_rooms_says_which_reason() {
        // The two states need different things done about them, so they cannot read alike: one
        // asks the user to turn the index on, and the other asks them to wait for it to fill.
        let off = Coverage { unindexed_rooms: 3, local_index_enabled: false };
        let empty = Coverage { unindexed_rooms: 3, local_index_enabled: true };

        assert_eq!(
            off.note(),
            Some("3 encrypted rooms not searched: local_index is off".to_string())
        );
        assert_eq!(empty.note(), Some("3 encrypted rooms not indexed yet".to_string()));
    }

    #[test]
    fn test_one_missed_room_is_not_called_rooms() {
        let one = Coverage { unindexed_rooms: 1, local_index_enabled: true };

        assert_eq!(one.note(), Some("1 encrypted room not indexed yet".to_string()));
    }

    #[test]
    fn test_a_local_hit_carries_the_room_the_index_was_asked_about() {
        // The index returns the sync form of an event, which names no room, so a hit that lost
        // the room would be a row the user cannot go to.
        let event = TimelineEvent::from_plaintext(
            Raw::new(&json!({
                "type": "m.room.message",
                "event_id": "$found:example.com",
                "sender": "@dan:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": { "msgtype": "m.text", "body": "let us   deploy\non friday" },
            }))
            .unwrap()
            .cast_unchecked(),
        );

        let hit = local_hit(room_id!("!general:example.com"), &event).unwrap();

        assert_eq!(hit, MessageHit {
            room_id: room_id!("!general:example.com").to_owned(),
            event_id: event_id!("$found:example.com").to_owned(),
            thread: None,
            sender: user_id!("@dan:example.com").to_owned(),
            timestamp: MilliSecondsSinceUnixEpoch(UInt::new(1_700_000_000_000).unwrap()),
            body: "let us deploy on friday".to_string(),
        });
    }

    #[test]
    fn test_a_local_result_that_is_not_a_readable_message_is_dropped() {
        let state = TimelineEvent::from_plaintext(
            Raw::new(&json!({
                "type": "m.room.topic",
                "event_id": "$topic:example.com",
                "sender": "@dan:example.com",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": { "topic": "deploys" },
            }))
            .unwrap()
            .cast_unchecked(),
        );

        assert!(local_hit(room_id!("!general:example.com"), &state).is_none());
    }

    #[test]
    fn test_a_continued_search_carries_the_token_it_was_given() {
        let request = search_request("deploy".to_string(), Some("token".to_string()));

        assert_eq!(request.next_batch.as_deref(), Some("token"));
    }
}
