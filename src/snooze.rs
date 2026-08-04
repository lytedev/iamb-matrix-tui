//! Deferring an inbox entry without reading it.
//!
//! `:unreadsandthreads` is the inbox, and an entry leaves it only when it becomes read. Marking an
//! entry read is destructive: it moves a read receipt that other clients see, and the only way back
//! is `:undoread`, whose stack a restart discards.
//!
//! A snooze separates two things that the read model ties together. A receipt answers "has the user
//! read this". A snooze answers "does the user want to see this now". Nothing here writes a
//! receipt.
//!
//! The state is one wake time per key. That single number does two jobs. An entry is hidden while
//! its wake time is in the future, and the same time is fed to the inbox sort as the entry's
//! `latest`, so a woken entry is the most recent thing in the list and rises to the top without any
//! wake code at all.

use std::collections::{BTreeMap, HashMap};

use chrono::{Duration, Local as LocalTz, NaiveTime, TimeZone};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId};
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch, in UTC.
///
/// Absolute rather than relative, so that a timezone change does not move a wake time. A large
/// clock correction moves one, which is acceptable: the worst outcome is that an unread entry
/// appears in the inbox at the wrong time.
pub type WakeTime = u64;

/// What a snooze is attached to.
///
/// The same shape as `ReadTarget`, because a snooze defers exactly the entries that `:read` can
/// mark read: a room, or one thread in a room.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SnoozeKey {
    pub room_id: OwnedRoomId,
    pub thread: Option<OwnedEventId>,
}

impl SnoozeKey {
    pub fn room(room_id: OwnedRoomId) -> Self {
        SnoozeKey { room_id, thread: None }
    }

    pub fn thread(room_id: OwnedRoomId, thread: OwnedEventId) -> Self {
        SnoozeKey { room_id, thread: Some(thread) }
    }
}

/// The wake times this client knows about.
///
/// A cache of what the rooms' account data says. Lookups are on the hot path, because every inbox
/// entry asks about itself each time the list is built.
#[derive(Default)]
pub struct SnoozeStore {
    wake_times: HashMap<SnoozeKey, WakeTime>,
}

impl SnoozeStore {
    pub fn set(&mut self, key: SnoozeKey, wake_at: WakeTime) {
        self.wake_times.insert(key, wake_at);
    }

    pub fn clear(&mut self, key: &SnoozeKey) -> bool {
        self.wake_times.remove(key).is_some()
    }

    /// Every wake time held, for `:snoozed` and for writing account data.
    pub fn entries(&self) -> impl Iterator<Item = (&SnoozeKey, &WakeTime)> {
        self.wake_times.iter()
    }

    /// The wake time that governs one entry.
    ///
    /// A room snooze acts as a floor for the threads in that room, so that "quiet this room until
    /// Tuesday" is one action rather than one action per thread. An explicit thread snooze always
    /// wins, in both directions: a thread can wake earlier than its room, and it can sleep longer.
    ///
    /// The alternative was to keep the two independent. That was rejected because the missing
    /// capability has no workaround, while an over-broad snooze expires by itself and one
    /// `:unsnooze` undoes it.
    pub fn wake_at(&self, room_id: &OwnedRoomId, thread: Option<&OwnedEventId>) -> Option<WakeTime> {
        match thread {
            None => self.wake_times.get(&SnoozeKey::room(room_id.clone())).copied(),
            Some(thread) => {
                let key = SnoozeKey::thread(room_id.clone(), thread.clone());

                match self.wake_times.get(&key) {
                    // Explicit beats inherited, even when the explicit time is sooner.
                    Some(explicit) => Some(*explicit),
                    None => self.wake_times.get(&SnoozeKey::room(room_id.clone())).copied(),
                }
            },
        }
    }

    /// Whether an entry is deferred at `now`.
    ///
    /// Kept apart from [SnoozeStore::wake_at] because the inbox needs both answers separately: this
    /// one hides the entry, and the wake time itself feeds the sort.
    pub fn is_deferred(
        &self,
        room_id: &OwnedRoomId,
        thread: Option<&OwnedEventId>,
        now: WakeTime,
    ) -> bool {
        self.wake_at(room_id, thread).is_some_and(|wake_at| wake_at > now)
    }

    /// Forget wake times that have passed.
    ///
    /// Pruning is by expiry and not by age, because a deliberately long snooze must survive.
    pub fn prune_expired(&mut self, now: WakeTime) {
        self.wake_times.retain(|_, wake_at| *wake_at > now);
    }
}

/// The account data event type that carries a room's wake times.
///
/// A private type in our own namespace. Other Matrix clients ignore a room account data type they
/// do not know, so no other client changes behaviour because of this event.
pub const SNOOZE_EVENT_TYPE: &str = "dev.lyte.iamb.snooze";

/// One room's wake times, as they are stored on the server.
///
/// Per room rather than one global event, because a tab id is unique only inside its own room and
/// because two machines snoozing different rooms must not overwrite each other. The write race is
/// then confined to the same room at the same moment, where last write wins is genuinely
/// ambiguous anyway.
#[derive(Default, Deserialize, Serialize)]
pub struct SnoozeContent {
    /// When the room itself comes back, if it is snoozed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<WakeTime>,

    /// When each snoozed thread in this room comes back, keyed by its root event id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub threads: BTreeMap<String, WakeTime>,
}

impl SnoozeContent {
    /// Whether this event carries nothing worth storing.
    ///
    /// An empty event is written rather than deleted, because the Matrix API has no way to remove
    /// room account data.
    pub fn is_empty(&self) -> bool {
        self.room.is_none() && self.threads.is_empty()
    }
}

impl SnoozeStore {
    /// Replace what is known about one room with what the server says.
    ///
    /// Expired entries are dropped on the way in, so a stale event cannot resurrect a snooze that
    /// has already run out.
    pub fn load_room(&mut self, room_id: &OwnedRoomId, content: SnoozeContent, now: WakeTime) {
        self.wake_times.retain(|key, _| &key.room_id != room_id);

        if let Some(wake_at) = content.room.filter(|w| *w > now) {
            self.set(SnoozeKey::room(room_id.clone()), wake_at);
        }

        for (root, wake_at) in content.threads {
            if wake_at <= now {
                continue;
            }

            let Ok(root) = EventId::parse(&root) else {
                continue;
            };

            self.set(SnoozeKey::thread(room_id.clone(), root), wake_at);
        }
    }

    /// What should be stored for one room.
    ///
    /// Expired entries are pruned on the way out, so the event cannot grow without bound.
    pub fn room_content(&self, room_id: &OwnedRoomId, now: WakeTime) -> SnoozeContent {
        let mut content = SnoozeContent::default();

        for (key, wake_at) in self.entries() {
            if &key.room_id != room_id || *wake_at <= now {
                continue;
            }

            match &key.thread {
                None => content.room = Some(*wake_at),
                Some(root) => {
                    content.threads.insert(root.to_string(), *wake_at);
                },
            }
        }

        content
    }
}

/// Why a duration could not be read.
#[derive(Debug, Eq, PartialEq)]
pub struct BadDuration(pub String);

impl std::fmt::Display for BadDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cannot read {:?} as a duration. Use a number with m, h, d or w, or \"tomorrow\".",
            self.0
        )
    }
}

/// Read a snooze duration and turn it into an absolute wake time.
///
/// Relative durations come first, because they are what a person types in the middle of work.
/// `tomorrow` is included because it is the one absolute time that is asked for constantly.
///
/// Absolute timestamps are deliberately absent. They need a date parser, they are rarely what the
/// user means at the moment of deferral, and adding them later breaks nothing.
pub fn parse_when(
    input: &str,
    now: WakeTime,
    tomorrow_hour: u32,
) -> Result<WakeTime, BadDuration> {
    let input = input.trim();

    if input.eq_ignore_ascii_case("tomorrow") {
        return next_day_at(now, tomorrow_hour).ok_or_else(|| BadDuration(input.to_string()));
    }

    let (digits, unit) = input.split_at(input.len().saturating_sub(1));
    let count: u64 = digits.parse().map_err(|_| BadDuration(input.to_string()))?;

    // Zero would mean "defer until now", which hides nothing and would look like the command did
    // not work.
    if count == 0 {
        return Err(BadDuration(input.to_string()));
    }

    // Every step is checked. An absurd count must be refused, not wrapped into the past, and an
    // unchecked multiply here panics in a debug build.
    let minutes = match unit {
        "m" => Some(count),
        "h" => count.checked_mul(60),
        "d" => count.checked_mul(60 * 24),
        "w" => count.checked_mul(60 * 24 * 7),
        _ => return Err(BadDuration(input.to_string())),
    };

    minutes
        .and_then(|m| m.checked_mul(60_000))
        .and_then(|ms| now.checked_add(ms))
        .ok_or_else(|| BadDuration(input.to_string()))
}

/// The next calendar day at `hour`, in the local timezone, as UTC milliseconds.
///
/// Local rather than UTC, because "tomorrow" means the user's tomorrow.
fn next_day_at(now: WakeTime, hour: u32) -> Option<WakeTime> {
    let now = LocalTz.timestamp_millis_opt(now as i64).single()?;
    let tomorrow = now.date_naive().checked_add_signed(Duration::days(1))?;
    let at = NaiveTime::from_hms_opt(hour.min(23), 0, 0)?;

    // A daylight-saving change can make a local time ambiguous or absent. Take the earliest valid
    // reading, and step forward an hour when the hour does not exist at all.
    let local = LocalTz
        .from_local_datetime(&tomorrow.and_time(at))
        .earliest()
        .or_else(|| {
            let later = NaiveTime::from_hms_opt(hour.min(22) + 1, 0, 0)?;
            LocalTz.from_local_datetime(&tomorrow.and_time(later)).earliest()
        })?;

    let millis = local.timestamp_millis();

    // Before 1970 cannot happen for a future time, but the conversion must still be total.
    (millis >= 0).then_some(millis as u64)
}

/// A human reading of a wake time, for `:snoozed` and for command feedback.
pub fn describe(wake_at: WakeTime) -> String {
    match LocalTz.timestamp_millis_opt(wake_at as i64).single() {
        Some(t) if t.date_naive() == LocalTz::now().date_naive() => t.format("%H:%M").to_string(),
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use matrix_sdk::ruma::UInt;

    use crate::base::UnreadInfo;
    use crate::message::MessageTimeStamp;
    use matrix_sdk::ruma::{event_id, room_id};

    const MINUTE: WakeTime = 60_000;
    const HOUR: WakeTime = 60 * MINUTE;
    const NOW: WakeTime = 1_800_000_000_000;

    fn store() -> SnoozeStore {
        SnoozeStore::default()
    }

    #[test]
    fn test_a_room_with_no_snooze_is_not_deferred() {
        let room = room_id!("!a:example.com").to_owned();

        assert!(!store().is_deferred(&room, None, NOW));
    }

    #[test]
    fn test_a_snoozed_room_is_deferred_until_its_wake_time() {
        let room = room_id!("!a:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + HOUR);

        assert!(s.is_deferred(&room, None, NOW));
        assert!(!s.is_deferred(&room, None, NOW + HOUR + 1));
    }

    #[test]
    fn test_a_room_snooze_also_defers_its_threads() {
        let room = room_id!("!a:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + HOUR);

        assert!(s.is_deferred(&room, Some(&thread), NOW));
    }

    #[test]
    fn test_an_explicit_thread_snooze_beats_its_room_when_it_is_sooner() {
        let room = room_id!("!a:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + 10 * HOUR);
        s.set(SnoozeKey::thread(room.clone(), thread.clone()), NOW + HOUR);

        // The thread wakes on its own time, and the room keeps sleeping.
        assert!(!s.is_deferred(&room, Some(&thread), NOW + HOUR + 1));
        assert!(s.is_deferred(&room, None, NOW + HOUR + 1));
    }

    #[test]
    fn test_an_explicit_thread_snooze_beats_its_room_when_it_is_later() {
        let room = room_id!("!a:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + HOUR);
        s.set(SnoozeKey::thread(room.clone(), thread.clone()), NOW + 10 * HOUR);

        assert!(s.is_deferred(&room, Some(&thread), NOW + 2 * HOUR));
    }

    #[test]
    fn test_a_thread_in_another_room_is_untouched() {
        let room = room_id!("!a:example.com").to_owned();
        let other = room_id!("!b:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room), NOW + HOUR);

        assert!(!s.is_deferred(&other, Some(&thread), NOW));
    }

    #[test]
    fn test_clearing_a_thread_snooze_falls_back_to_its_room() {
        let room = room_id!("!a:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + 10 * HOUR);
        s.set(SnoozeKey::thread(room.clone(), thread.clone()), NOW + HOUR);
        s.clear(&SnoozeKey::thread(room.clone(), thread.clone()));

        assert_eq!(s.wake_at(&room, Some(&thread)), Some(NOW + 10 * HOUR));
    }

    #[test]
    fn test_pruning_keeps_a_long_snooze_and_drops_a_passed_one() {
        let room = room_id!("!a:example.com").to_owned();
        let other = room_id!("!b:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + 1000 * HOUR);
        s.set(SnoozeKey::room(other.clone()), NOW - HOUR);
        s.prune_expired(NOW);

        assert!(s.wake_at(&room, None).is_some());
        assert!(s.wake_at(&other, None).is_none());
    }

    #[test]
    fn test_a_wake_time_replaces_an_older_message_time() {
        let old = MessageTimeStamp::OriginServer(UInt::new(1_000).unwrap());
        let unread = UnreadInfo { unread: true, latest: Some(old) };

        let woken = unread.with_wake_time(Some(NOW));

        // The entry now claims to be as recent as its wake time, which is what lifts it to the top
        // of the inbox when it comes back.
        assert_eq!(woken.latest(), Some(&MessageTimeStamp::OriginServer(
            UInt::new(NOW).unwrap()
        )));
    }

    #[test]
    fn test_a_newer_message_beats_the_wake_time() {
        let newer = MessageTimeStamp::OriginServer(UInt::new(NOW + HOUR).unwrap());
        let unread = UnreadInfo { unread: true, latest: Some(newer) };

        let woken = unread.with_wake_time(Some(NOW));

        assert_eq!(woken.latest(), Some(&newer));
    }

    #[test]
    fn test_no_wake_time_leaves_the_entry_alone() {
        let old = MessageTimeStamp::OriginServer(UInt::new(1_000).unwrap());
        let unread = UnreadInfo { unread: true, latest: Some(old) };

        assert_eq!(unread.with_wake_time(None).latest(), Some(&old));
    }

    #[test]
    fn test_a_room_survives_a_round_trip_through_account_data() {
        let room = room_id!("!a:example.com").to_owned();
        let thread = event_id!("$t:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + HOUR);
        s.set(SnoozeKey::thread(room.clone(), thread.clone()), NOW + 2 * HOUR);

        let content = s.room_content(&room, NOW);
        let mut loaded = store();
        loaded.load_room(&room, content, NOW);

        assert_eq!(loaded.wake_at(&room, None), Some(NOW + HOUR));
        assert_eq!(loaded.wake_at(&room, Some(&thread)), Some(NOW + 2 * HOUR));
    }

    #[test]
    fn test_an_expired_entry_is_not_written_out() {
        let room = room_id!("!a:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW - HOUR);

        assert!(s.room_content(&room, NOW).is_empty());
    }

    #[test]
    fn test_an_expired_entry_is_not_read_back_in() {
        let room = room_id!("!a:example.com").to_owned();
        let mut content = SnoozeContent::default();
        content.room = Some(NOW - HOUR);

        let mut s = store();
        s.load_room(&room, content, NOW);

        assert!(s.wake_at(&room, None).is_none());
    }

    #[test]
    fn test_loading_a_room_replaces_what_was_known_about_it() {
        let room = room_id!("!a:example.com").to_owned();
        let other = room_id!("!b:example.com").to_owned();
        let mut s = store();

        s.set(SnoozeKey::room(room.clone()), NOW + HOUR);
        s.set(SnoozeKey::room(other.clone()), NOW + HOUR);

        // An empty event means the room has no snooze any more.
        s.load_room(&room, SnoozeContent::default(), NOW);

        assert!(s.wake_at(&room, None).is_none());
        // Another room is untouched, which is the point of storing this per room.
        assert!(s.wake_at(&other, None).is_some());
    }

    #[test]
    fn test_relative_durations_are_read() {
        assert_eq!(parse_when("30m", NOW, 9), Ok(NOW + 30 * MINUTE));
        assert_eq!(parse_when("2h", NOW, 9), Ok(NOW + 2 * HOUR));
        assert_eq!(parse_when("3d", NOW, 9), Ok(NOW + 72 * HOUR));
        assert_eq!(parse_when("1w", NOW, 9), Ok(NOW + 168 * HOUR));
    }

    #[test]
    fn test_surrounding_space_is_ignored() {
        assert_eq!(parse_when("  2h  ", NOW, 9), Ok(NOW + 2 * HOUR));
    }

    #[test]
    fn test_a_bad_duration_is_refused() {
        assert!(parse_when("", NOW, 9).is_err());
        assert!(parse_when("2", NOW, 9).is_err());
        assert!(parse_when("h", NOW, 9).is_err());
        assert!(parse_when("2y", NOW, 9).is_err());
        assert!(parse_when("-2h", NOW, 9).is_err());
        assert!(parse_when("two hours", NOW, 9).is_err());
    }

    #[test]
    fn test_zero_is_refused_because_it_would_hide_nothing() {
        assert!(parse_when("0m", NOW, 9).is_err());
        assert!(parse_when("0h", NOW, 9).is_err());
    }

    #[test]
    fn test_an_absurd_count_does_not_wrap_into_the_past() {
        let result = parse_when(&format!("{}w", u64::MAX), NOW, 9);

        assert!(result.is_err());
    }

    #[test]
    fn test_tomorrow_lands_in_the_future_at_the_configured_hour() {
        let wake = parse_when("tomorrow", NOW, 9).expect("tomorrow parses");

        assert!(wake > NOW);

        let local = LocalTz.timestamp_millis_opt(wake as i64).single().unwrap();

        assert_eq!(local.hour(), 9);
        assert_eq!(local.minute(), 0);
    }

    #[test]
    fn test_tomorrow_is_case_insensitive() {
        assert_eq!(parse_when("Tomorrow", NOW, 9), parse_when("tomorrow", NOW, 9));
    }
}
