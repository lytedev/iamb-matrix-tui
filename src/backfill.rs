//! # Backfilling the local message index
//!
//! The [local index][crate::config::LocalIndex] holds what arrived while it was turned on, and
//! nothing older. That answers almost nothing a person actually asks: what somebody wants from a
//! search is what was decided months ago, not what was said this afternoon. This module fills the
//! index with the history that already exists.
//!
//! It needs no indexing code. matrix-sdk feeds the index from event cache updates, and it makes no
//! distinction between an event that arrived from sync and an event that arrived from
//! back-pagination. So walking a room backwards through
//! [`RoomPagination`][matrix_sdk::event_cache::RoomPagination] indexes every message it passes.
//! The work here is therefore the walk: which rooms, how fast, where it stopped, and what it could
//! not read.
//!
//! ## Why it resumes rather than restarts
//!
//! A walk over years of history in hundreds of rooms will be interrupted. A laptop closes, a
//! network drops, iamb restarts. A backfill that began again from the newest message every time
//! would never reach the oldest one.
//!
//! Two things make resumption work, and only one of them is written here. The event cache stores
//! its own pagination tokens, so a room continues from where its walk stopped rather than from the
//! top. What this module adds is [BackfillState]: a record of which rooms finished, so that a
//! resumed run skips them instead of asking the homeserver to confirm that a walk it already
//! completed is still complete. Indexing the same event twice is harmless, because the index keys
//! on the event identifier, but the requests are not free.
//!
//! ## Why it does not stop the interface
//!
//! The walk runs as a background task and reports through
//! [BackgroundReport][crate::base::BackgroundReport], the same way unlocking the key backup does.
//! Awaiting it in the main loop would freeze the screen for as long as the walk takes, which is
//! hours rather than the twenty seconds that made the key backup a background task.
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use matrix_sdk::encryption::backups::BackupState;
use matrix_sdk::event_cache::EventCacheError;
use matrix_sdk::room::Room;
use matrix_sdk::ruma::{OwnedRoomId, RoomId};
use matrix_sdk::Client;
use modalkit::prelude::EditInfo;
use serde::{Deserialize, Serialize};

use crate::base::{BackgroundReport, IambError, IambResult, ProgramStore};

/// How many events one back-pagination request asks for.
///
/// The homeserver caps this, and a larger batch means fewer round trips for the same history.
pub const BATCH_SIZE: u16 = 100;

/// How long the walk waits between batches.
///
/// This is the whole of the throttle, and it exists because the backfill and the user compete for
/// one request budget and one processor. Without it a backfill saturates both, and the client it
/// is running inside feels broken: sending a message waits behind a queue of pagination requests.
/// A quarter of a second per batch costs a long backfill some hours and keeps the client usable
/// throughout, which is the right trade for something that is expected to run unattended.
pub const BATCH_DELAY: Duration = Duration::from_millis(250);

/// How many empty batches in a row end a room's walk.
///
/// One empty batch can be a gap rather than the end of the history.
const EMPTY_BATCH_LIMIT: usize = 3;

/// The name of the file that records which rooms finished.
const STATE_FILE: &str = "backfill.json";

/// What a backfill records about one room, so that a later run can continue it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoomBackfill {
    /// Whether the walk reached the start of this room's timeline.
    ///
    /// A finished room is skipped by a later run. Nothing else about the walk needs recording,
    /// because the event cache holds its own pagination tokens and continues an unfinished room
    /// from where it stopped.
    pub done: bool,

    /// How many messages of this room went into the index.
    pub indexed: usize,

    /// How many events of this room could not be decrypted.
    ///
    /// These are the messages a search will never find. They are counted rather than dropped, so
    /// that the end of a backfill can say what it could not read instead of implying it read
    /// everything.
    pub undecryptable: usize,
}

/// Which rooms a backfill has already walked.
///
/// This is written beside the index and is as disposable as the index is. Losing it costs a
/// repeated walk, not correctness.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackfillState {
    /// What is known about each room, keyed by room identifier.
    ///
    /// Ordered so that the file does not change merely because a map iterated differently, which
    /// would make every run look like a change to anything watching the directory.
    #[serde(default)]
    pub rooms: BTreeMap<OwnedRoomId, RoomBackfill>,
}

impl BackfillState {
    /// Where the record lives, given the index directory.
    pub fn path(search_index_dir: &Path) -> PathBuf {
        search_index_dir.join(STATE_FILE)
    }

    /// Read the record, treating anything unreadable as an empty one.
    ///
    /// A missing file is the normal first run. A corrupt file is worth no more than a missing one:
    /// the cost of ignoring it is a repeated walk, and the cost of failing on it is a backfill
    /// that cannot start until the user deletes a file nobody told them about.
    pub fn load(search_index_dir: &Path) -> BackfillState {
        let path = Self::path(search_index_dir);

        let Ok(file) = File::open(&path) else {
            return BackfillState::default();
        };

        match serde_json::from_reader(BufReader::new(file)) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(?path, "ignoring an unreadable backfill record: {e}");

                BackfillState::default()
            },
        }
    }

    /// Write the record.
    ///
    /// The caller writes after every room rather than at the end, because the end may never come:
    /// the run this is recording is the one that gets interrupted.
    pub fn save(&self, search_index_dir: &Path) -> Result<(), std::io::Error> {
        let path = Self::path(search_index_dir);
        let file = File::create(&path)?;

        serde_json::to_writer(BufWriter::new(file), self).map_err(std::io::Error::other)
    }

    /// Whether this room's walk already reached the start of its timeline.
    pub fn is_done(&self, room_id: &RoomId) -> bool {
        self.rooms.get(room_id).is_some_and(|room| room.done)
    }

    /// Add what one walk of `room_id` achieved to what earlier walks achieved.
    ///
    /// The counts accumulate because a room is usually walked over several runs, and the user
    /// asked how much of their history is searchable, not how much of it the last run managed.
    pub fn record(&mut self, room_id: &RoomId, done: bool, indexed: usize, undecryptable: usize) {
        let room = self.rooms.entry(room_id.to_owned()).or_default();

        room.done = done;
        room.indexed += indexed;
        room.undecryptable += undecryptable;
    }
}

/// What a running backfill is doing, for the user to look at.
///
/// A long job that says nothing cannot be told apart from a stuck one, so this holds enough to
/// answer both "how far" and "still moving": a count of rooms, and the name of the room being
/// walked right now.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BackfillProgress {
    /// How many rooms this run set out to walk.
    pub rooms_total: usize,

    /// How many of them it has finished.
    pub rooms_done: usize,

    /// The room being walked, by the name the user knows it under.
    pub current: Option<String>,

    /// How many messages this run has put into the index.
    pub indexed: usize,

    /// How many events this run could not decrypt.
    pub undecryptable: usize,

    /// Whether a stop has been asked for and not yet taken effect.
    pub stopping: bool,
}

impl BackfillProgress {
    /// One line saying how far the run has got.
    pub fn describe(&self) -> String {
        let BackfillProgress { rooms_done, rooms_total, indexed, .. } = self;

        let mut line = format!("{rooms_done}/{rooms_total} rooms, {indexed} messages indexed");

        if let Some(current) = &self.current {
            line.push_str(&format!(", now: {current}"));
        }

        if self.stopping {
            line.push_str(" (stopping)");
        }

        line
    }
}

/// A backfill that a command can watch and stop.
///
/// The walk runs on a background task and the commands run on the main loop, so everything they
/// share sits behind this. The stop flag is separate from the progress so that asking to stop
/// never waits for the lock the walk holds while it updates its counts.
#[derive(Debug, Default)]
pub struct Backfill {
    /// Whether a walk is running.
    running: AtomicBool,

    /// Whether the running walk has been asked to stop.
    stop: AtomicBool,

    /// What the running walk is doing.
    progress: Mutex<BackfillProgress>,
}

impl Backfill {
    /// Claim the right to run, or refuse because a walk is already running.
    ///
    /// Two walks at once would ask the same rooms for the same history twice and double the load
    /// this is throttled to keep down.
    pub fn start(&self, rooms_total: usize) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }

        self.stop.store(false, Ordering::SeqCst);
        *self.progress() = BackfillProgress { rooms_total, ..BackfillProgress::default() };

        true
    }

    /// Give up the right to run.
    pub fn finish(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.progress().current = None;
    }

    /// Whether a walk is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Ask the running walk to stop.
    ///
    /// The walk checks this between batches, so a stop takes effect within one batch rather than
    /// at the end of the room. What was already indexed stays indexed, and the record of which
    /// rooms finished is written before this returns to the user, so stopping never costs the
    /// progress that was made.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.progress().stopping = true;
    }

    /// Whether the walk should give up now.
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// What the walk is doing, for a command to read or the walk itself to update.
    ///
    /// A poisoned lock is recovered from rather than panicked on. The data behind it is a set of
    /// counts to show the user, so a task that died holding it has cost nothing that is worth
    /// bringing the client down over.
    pub fn progress(&self) -> std::sync::MutexGuard<'_, BackfillProgress> {
        self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Start walking the history of `rooms` in the background.
///
/// The rooms that already finished are dropped here rather than inside the walk, so that the
/// count the user is shown is the work that remains and not the work that was done last week.
///
/// Nothing is awaited. The walk takes hours, and the loop that would await it is the loop that
/// draws the screen.
pub fn start_backfill(rooms: Vec<OwnedRoomId>, store: &mut ProgramStore) -> IambResult<EditInfo> {
    if !store.application.settings.tunables.local_index.enabled {
        return Err(IambError::LocalIndexDisabled.into());
    }

    let client = store.application.worker.client.clone();

    // Only an encrypted room is worth walking. The homeserver answers for the rest, over their
    // whole history rather than over what this machine indexed, so indexing one would spend hours
    // to produce results that :search already has.
    let encrypted = rooms.iter().any(|room_id| {
        client
            .get_room(room_id)
            .is_some_and(|room| room.encryption_state().is_encrypted())
    });

    if !encrypted {
        return Err(IambError::RoomNotEncrypted.into());
    }

    let dir = store.application.settings.search_index_dir.clone();
    let state = BackfillState::load(&dir);

    let todo: Vec<OwnedRoomId> = rooms
        .into_iter()
        .filter(|room_id| !state.is_done(room_id))
        .filter(|room_id| {
            client
                .get_room(room_id)
                .is_some_and(|room| room.encryption_state().is_encrypted())
        })
        .collect();

    if todo.is_empty() {
        return Ok(Some("Already indexed. Delete the index directory to start again.".into()));
    }

    // Room titles are resolved now, while the store is in hand. The walk runs without it.
    let titles = todo
        .iter()
        .map(|room_id| (room_id.clone(), store.application.get_room_title(room_id)))
        .collect();

    let backfill = store.application.backfill.clone();

    if !backfill.start(todo.len()) {
        return Err(IambError::BackfillRunning.into());
    }

    let started = format!("Indexing {} rooms in the background. :reindex status", todo.len());

    let reports = store.application.reports.clone();

    tokio::spawn(async move {
        let report = walk(&client, todo, titles, &dir, &backfill).await;

        backfill.finish();

        let _ = reports.send(report);
    });

    Ok(Some(started.into()))
}

/// Walk every room in `todo` backwards, and say how it went.
///
/// The record of which rooms finished is written after every room, not at the end, because the run
/// this records is the one that gets interrupted. A run that only saved on success would save
/// nothing on the occasions that matter.
async fn walk(
    client: &Client,
    todo: Vec<OwnedRoomId>,
    titles: HashMap<OwnedRoomId, String>,
    dir: &Path,
    backfill: &Backfill,
) -> BackgroundReport {
    let mut state = BackfillState::load(dir);

    for room_id in todo {
        if backfill.stop_requested() {
            break;
        }

        backfill.progress().current = titles.get(&room_id).cloned();

        let Some(room) = client.get_room(&room_id) else {
            // The room was left while the walk was queued. It is not an error, and there is no
            // history left to reach.
            backfill.progress().rooms_done += 1;
            continue;
        };

        let (done, indexed, undecryptable) = match walk_room(&room, backfill).await {
            Ok(walked) => walked,
            Err(e) => {
                tracing::warn!(?room_id, "could not index the history of a room: {e}");
                (false, 0, 0)
            },
        };

        state.record(&room_id, done, indexed, undecryptable);

        if let Err(e) = state.save(dir) {
            tracing::warn!("could not record what was indexed: {e}");
        }

        backfill.progress().rooms_done += 1;
    }

    let stopped = backfill.stop_requested();
    let backup_enabled = client.encryption().backups().state() == BackupState::Enabled;
    let progress = backfill.progress().clone();

    BackgroundReport::Info(summarize(&progress, stopped, backup_enabled))
}

/// Walk one room backwards until it runs out of history, or until a stop is asked for.
///
/// Nothing here indexes anything. matrix-sdk feeds the index from the event cache, and every event
/// this walk pulls into the cache is indexed by that path. What this returns is what happened, so
/// that the record and the progress can be kept.
///
/// An event that cannot be decrypted is counted here rather than left out. matrix-sdk skips it
/// silently, which is correct for the index and wrong for the user: those are exactly the messages
/// a later search will fail to find, and the only chance to say so is now.
async fn walk_room(
    room: &Room,
    backfill: &Backfill,
) -> Result<(bool, usize, usize), EventCacheError> {
    let (cache, _drop_handles) = room.event_cache().await?;
    let pagination = cache.pagination();

    let mut indexed = 0;
    let mut undecryptable = 0;
    let mut empty_batches = 0;

    loop {
        if backfill.stop_requested() {
            return Ok((false, indexed, undecryptable));
        }

        let outcome = pagination.run_backwards_once(BATCH_SIZE).await?;

        let batch_undecryptable = outcome.events.iter().filter(|event| event.kind.is_utd()).count();
        let batch_indexed = outcome.events.len() - batch_undecryptable;

        indexed += batch_indexed;
        undecryptable += batch_undecryptable;

        {
            let mut progress = backfill.progress();
            progress.indexed += batch_indexed;
            progress.undecryptable += batch_undecryptable;
        }

        if outcome.reached_start {
            return Ok((true, indexed, undecryptable));
        }

        // A room that returns nothing and does not say it reached the start has nothing more to
        // give this walk. Without this the walk would ask again forever, once per delay, and the
        // progress would sit still while the client looked busy. Give it a few tries first,
        // because one empty batch can mean a gap rather than the end.
        if outcome.events.is_empty() {
            empty_batches += 1;

            if empty_batches >= EMPTY_BATCH_LIMIT {
                return Ok((false, indexed, undecryptable));
            }
        } else {
            empty_batches = 0;
        }

        // The throttle. See BATCH_DELAY for why a backfill that could run flat out does not.
        tokio::time::sleep(BATCH_DELAY).await;
    }
}

/// What to say when a backfill finishes.
///
/// The rooms that could not be read are named as a count rather than passed over. A search that
/// silently omits them is the thing this whole feature exists to stop.
pub fn summarize(progress: &BackfillProgress, stopped: bool, backup_enabled: bool) -> String {
    let BackfillProgress {
        rooms_done, rooms_total, indexed, undecryptable, ..
    } = progress;

    let verb = if stopped { "Stopped" } else { "Finished" };
    let mut summary =
        format!("{verb} indexing: {rooms_done}/{rooms_total} rooms, {indexed} messages");

    if *undecryptable > 0 {
        summary.push_str(&format!(". {undecryptable} events could not be decrypted"));

        // The key backup holds the keys these events need. Say so only when it is switched on,
        // because telling somebody to recover a backup they do not have wastes their time.
        if backup_enabled {
            summary.push_str(", and may be readable after :keys recover");
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::room_id;

    fn progress() -> BackfillProgress {
        BackfillProgress {
            rooms_total: 10,
            rooms_done: 3,
            current: None,
            indexed: 500,
            undecryptable: 0,
            stopping: false,
        }
    }

    #[test]
    fn test_a_finished_room_is_skipped_by_a_later_run() {
        // This is the whole of resumption that iamb owns: the event cache continues an unfinished
        // room by itself, and this stops a finished one from being walked again.
        let mut state = BackfillState::default();
        let general = room_id!("!general:example.com");

        assert!(!state.is_done(general));

        state.record(general, false, 100, 0);
        assert!(!state.is_done(general));

        state.record(general, true, 50, 0);
        assert!(state.is_done(general));
    }

    #[test]
    fn test_the_counts_add_up_across_runs() {
        // A room is usually walked over several runs, and the question is how much of the history
        // is searchable now, not how much the last run managed.
        let mut state = BackfillState::default();
        let general = room_id!("!general:example.com");

        state.record(general, false, 100, 2);
        state.record(general, true, 50, 3);

        assert_eq!(state.rooms[general], RoomBackfill {
            done: true,
            indexed: 150,
            undecryptable: 5,
        });
    }

    #[test]
    fn test_a_record_survives_being_written_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = BackfillState::default();

        state.record(room_id!("!general:example.com"), true, 150, 5);
        state.save(dir.path()).unwrap();

        assert_eq!(BackfillState::load(dir.path()), state);
    }

    #[test]
    fn test_a_missing_record_reads_as_an_empty_one() {
        // The normal first run. It must not need a file that nobody has created yet.
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(BackfillState::load(dir.path()), BackfillState::default());
    }

    #[test]
    fn test_an_unreadable_record_reads_as_an_empty_one() {
        // Failing here would stop a backfill until the user deleted a file nobody told them about.
        // Ignoring it costs a repeated walk.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(BackfillState::path(dir.path()), b"{not json").unwrap();

        assert_eq!(BackfillState::load(dir.path()), BackfillState::default());
    }

    #[test]
    fn test_progress_says_how_far_and_what_it_is_doing_now() {
        // Rooms done against rooms total answers "how far". The current room answers "is it
        // stuck", which the counts alone cannot.
        let mut progress = progress();
        assert_eq!(progress.describe(), "3/10 rooms, 500 messages indexed");

        progress.current = Some("#general".to_string());
        assert_eq!(progress.describe(), "3/10 rooms, 500 messages indexed, now: #general");

        progress.stopping = true;
        assert_eq!(
            progress.describe(),
            "3/10 rooms, 500 messages indexed, now: #general (stopping)"
        );
    }

    #[test]
    fn test_a_summary_reports_what_could_not_be_read() {
        let mut progress = progress();

        assert_eq!(
            summarize(&progress, false, false),
            "Finished indexing: 3/10 rooms, 500 messages"
        );

        progress.undecryptable = 7;
        assert_eq!(
            summarize(&progress, false, false),
            "Finished indexing: 3/10 rooms, 500 messages. 7 events could not be decrypted"
        );
    }

    #[test]
    fn test_a_summary_points_at_the_key_backup_only_when_there_is_one() {
        // Telling somebody to recover a backup they do not have wastes their time.
        let progress = BackfillProgress { undecryptable: 7, ..progress() };

        assert!(!summarize(&progress, false, false).contains(":keys recover"));
        assert!(summarize(&progress, false, true).contains(":keys recover"));
    }

    #[test]
    fn test_a_stopped_run_says_so() {
        assert!(summarize(&progress(), true, false).starts_with("Stopped indexing:"));
    }

    #[test]
    fn test_only_one_walk_runs_at_a_time() {
        // Two walks would ask the same rooms for the same history twice, and double the load the
        // throttle exists to keep down.
        let backfill = Backfill::default();

        assert!(backfill.start(10));
        assert!(!backfill.start(10));

        backfill.finish();
        assert!(backfill.start(10));
    }

    #[test]
    fn test_a_stop_is_visible_to_the_walk_and_to_the_user() {
        let backfill = Backfill::default();
        backfill.start(10);

        assert!(!backfill.stop_requested());
        assert!(!backfill.progress().stopping);

        backfill.stop();

        assert!(backfill.stop_requested());
        assert!(backfill.progress().stopping);
    }

    #[test]
    fn test_a_new_run_clears_the_stop_from_the_last_one() {
        // Otherwise a run started after a stop would give up on its first batch.
        let backfill = Backfill::default();

        backfill.start(10);
        backfill.stop();
        backfill.finish();

        backfill.start(10);

        assert!(!backfill.stop_requested());
        assert!(!backfill.progress().stopping);
    }
}
