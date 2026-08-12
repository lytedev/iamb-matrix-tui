//! # Full-Text Message Search
//!
//! The homeserver indexes the messages it can read, and answers
//! [`POST /_matrix/client/v3/search`][spec] with the ones that match a term. This module builds
//! that request and turns the answer into [MessageHit]s. The request goes out from
//! [the worker][crate::worker], and the hits are what the
//! [`:search` window][crate::windows::search] lists.
//!
//! The server cannot read an encrypted room, so it cannot index one either. A search of an
//! encrypted room therefore returns nothing, however much was said there. Nothing here can fix
//! that, and nothing here pretends to: the window says so instead.
//!
//! [spec]: https://spec.matrix.org/v1.18/client-server-api/#post_matrixclientv3search
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
    events::{room::message::Relation, AnyMessageLikeEvent, AnyTimelineEvent, MessageLikeEvent},
    MilliSecondsSinceUnixEpoch,
    OwnedEventId,
    OwnedRoomId,
    OwnedUserId,
    UInt,
};

/// How many hits one request asks the homeserver for.
///
/// The server is free to return fewer, and says so by handing back a `next_batch` to ask again
/// with.
pub const HITS_PER_REQUEST: u32 = 100;

/// How many hits are collected before the search stops asking for more.
///
/// The window filters the hits it was given without going back to the network, so the whole set
/// has to be in hand before the user types anything. That argues for fetching everything, and a
/// common word matches thousands of messages, which argues for stopping. This is the compromise:
/// enough that the filter has something real to work on, few enough that the search returns in a
/// moment. Hits arrive newest first, so what is dropped is the oldest.
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

/// Ask the homeserver for the messages that match `term`.
///
/// Only `content.body` is searched, because that is the text the user remembers typing. The
/// results come back newest first, since a message somebody is looking for is far more often a
/// recent one than the best-ranked one out of years of chat.
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
    use matrix_sdk::ruma::{event_id, room_id, user_id};
    use serde_json::{from_value, json, Value};

    /// The `room_events` out of a response body, as the worker gets it.
    fn results(results: Value) -> ResultRoomEvents {
        from_value(json!({ "count": 1, "results": results })).unwrap()
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
    fn test_the_request_searches_message_bodies_newest_first() {
        let request = search_request("deploy".to_string(), None);
        let criteria = request.search_categories.room_events.unwrap();

        assert_eq!(criteria.search_term, "deploy");
        assert_eq!(criteria.keys, Some(vec![SearchKeys::ContentBody]));
        assert_eq!(criteria.order_by, Some(OrderBy::Recent));
        assert_eq!(criteria.filter.limit, Some(UInt::from(HITS_PER_REQUEST)));
    }

    #[test]
    fn test_a_continued_search_carries_the_token_it_was_given() {
        let request = search_request("deploy".to_string(), Some("token".to_string()));

        assert_eq!(request.next_batch.as_deref(), Some("token"));
    }
}
