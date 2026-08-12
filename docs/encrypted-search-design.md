# Encrypted search: find text in rooms the server cannot read

Status: proposal. No code is written yet.

Every `matrix-sdk` path in this document was read from `matrix-sdk-0.18.0` and
`matrix-sdk-search-0.18.0`, which is the version `Cargo.lock` pins. A stale
0.14 copy exists in the nix store. Do not read that copy. The search code in
this document does not exist in 0.14.

## The problem

`POST /_matrix/client/v3/search` finds text, but only in the rooms the server
can read. An encrypted room gives the server ciphertext. The server cannot
index ciphertext. Therefore server search finds nothing in the rooms that hold
most of the private conversation.

This is the same limit that Element web reports when it says that it cannot
cache encrypted messages in a browser. Element Desktop removes the limit with
seshat, which is a local full-text index over the decrypted bodies.

The `:search` command that the other agent builds calls the server endpoint.
That command answers the question for unencrypted rooms. This document
answers the same question for encrypted rooms, and specifies how one command
shows both answers.

## Recommendation in short

Do not write an index. `matrix-sdk` 0.18.0 already contains one.

Turn on the `experimental-search` feature of `matrix-sdk`. That feature pulls
in `matrix-sdk-search`, which is a tantivy index with a per-room directory and
an optional AES layer over the directory. The key file is literally named
`seshat-index.key`, because the crate is the successor to seshat. The SDK
already runs a background task that feeds the index from the event cache
(`event_cache/tasks.rs:469`), already handles edits and redactions
(`search_index/mod.rs:250`), and already offers a global search across rooms
with pagination (`message_search.rs`).

The work is therefore not "build an index". The work is **connect iamb to the
event cache**, because iamb does not use it today, and the index is fed only
from event cache updates.

Store the index encrypted, in a new directory beside the sqlite stores. Feed
history into it with an explicit, resumable backfill command, because history
is the thing Daniel wants to search and no automatic path will produce it.

## 1. What exists locally today, and why the event cache is empty

`~/.local/share/iamb/profiles/personal/sqlite/` holds four stores. The state
store is 8.9 MB and knows 365 rooms. The media store is 208 MB. The crypto
store is 364 KB. The event cache is 69 KB, and it is empty:

    sqlite> select count(*) from events;
    0
    sqlite> select count(*) from linked_chunks;
    0

The file exists because `Client::builder().sqlite_store(...)`
(`src/worker.rs:841`) opens all four stores together. It is empty because
nothing ever writes to it. `EventCache::subscribe` is never called. A grep for
`event_cache` across `src/` returns nothing.

iamb does not use the event cache because it paginates by hand.
`load_older_forward` calls `room.messages(opts)` directly
(`src/worker.rs:407`), which is the raw `/messages` endpoint, and then stores
the result in its own in-memory `Messages` map in `RoomInfo`. That map is
never written to disk. When iamb exits, the scrollback is gone.

This has two consequences that decide the rest of the design.

First, **the event cache cannot be "made to hold enough to search" by tuning
retention**. There is no retention to tune. There is no data. The question is
not how long the cache keeps events; it is whether iamb writes any.

Second, **the schema does hold decrypted bodies once it is used**. The
`events` table stores a JSON-serialized `TimelineEvent` in a `content` column,
and the column comment says "encrypted value", which means the store
encryption of `matrix-sdk-store-encryption`, not Matrix encryption. iamb
passes `None` as the passphrase to `sqlite_store`, so that layer is off today.
A `TimelineEvent` for a decrypted message holds the plaintext body. So turning
the event cache on is itself a decision to write plaintext to disk, before any
search index is considered. Section 6 returns to this.

## 2. What matrix-sdk offers

### The index

`matrix-sdk-search` 0.18.0 is tantivy 0.25 with a small schema
(`schema.rs:47`):

| field               | tantivy options | meaning                        |
| ------------------- | --------------- | ------------------------------ |
| `event_id`          | `STORED｜STRING` | primary key, returned by search |
| `original_event_id` | `STRING`        | deletion key, points at the edited original |
| `body`              | `TEXT`          | the searched text              |
| `date`              | `INDEXED`, fast | origin server timestamp        |
| `sender`            | `STRING`        | the sender                     |

Only `event_id` is `STORED`. The body is tokenized into the inverted index and
is not kept as a retrievable string. Section 6 explains why that is a weaker
protection than it sounds.

Three storage kinds exist (`search_index/mod.rs:47`): an unencrypted
directory, an encrypted directory that takes a password, and memory. The
encrypted kind derives a key with PBKDF2 and encrypts the tantivy directory
files with AES-CTR plus an HMAC (`encrypted/encrypted_dir.rs`).

### The feed

`EventCache::subscribe` spawns `search_indexing_task`
(`event_cache/mod.rs:313`). That task subscribes to linked chunk updates of
the event cache and, for every batch, derives index operations and applies
them (`event_cache/tasks.rs:469`). Because the feed is linked chunk updates,
**anything that enters the event cache is indexed, whether it came from sync
or from back-pagination.** That single fact is what makes a history backfill
possible without new indexing code.

`parse_timeline_event` (`search_index/mod.rs:335`) skips events that failed to
decrypt, indexes `m.room.message`, resolves edits to the newest version, and
removes redacted events.

### The query surface

- `Room::search(query, limit, offset)` returns event IDs
  (`message_search.rs:82`).
- `Room::search_messages(query, batch)` returns a paginating iterator, and
  `next_events` resolves each ID with `load_or_fetch_event`
  (`message_search.rs:166`).
- `Client::search_messages(query, batch)` builds a global iterator over all
  joined rooms, with `only_dm_rooms` and `no_dms` filters
  (`message_search.rs:255`).

The query string goes to the tantivy `QueryParser`, so it supports field
syntax such as `sender:"@x:y"` and boolean operators, not only bare words.

### The state store offers nothing

The state store holds `state_event`, `member`, `profile`, `room_info` and
receipts. It holds no message bodies. It is not a search source.

## 3. Where text enters the index

Four candidate points exist. The recommendation uses two of them, and the
choice is forced by section 1.

**At sync time, through the event cache.** This is the SDK default and it
costs one line. It covers everything that arrives from the moment the feature
is on. It covers nothing older.

**At decryption time.** The SDK skips events that are undecryptable at the
moment they enter the cache. The redecryptor re-decrypts them when a room key
arrives later and re-posts the events. Whether that re-post reaches the
indexing task is stated as an open question in section 10, because the
redecryptor sends a `RoomEventCacheUpdate` (`redecryptor.rs:418`) and the
indexing task listens for a `RoomEventCacheLinkedChunkUpdate`. This matters
because Daniel's key backup recovery landed recently, so a large number of
previously undecryptable events can become readable in one moment.

**On back-pagination.** This is the important one. Because the index is fed
from linked chunk updates, paginating a room backwards through the event cache
indexes every message it walks past. History indexing therefore needs no index
code at all. It needs pagination through the right API.

**A background pass over history.** This is back-pagination, driven by a
command rather than by the user scrolling. It is the recommended mechanism for
old history, and it is the only mechanism that answers "what about messages
from before the feature existed".

The recommendation is: sync-time indexing for new messages, and a
**`:reindex` command** that runs back-pagination through the event cache for a
chosen set of rooms, reports progress, and can be stopped and resumed.
Resumption is free, because the event cache stores its own gap tokens in
`gap_chunks` and re-indexing an event that is already indexed is idempotent on
the `event_id` primary key.

## 4. Storage and cost

**Index size.** The index stores tokens of message bodies, plus an event ID,
sender and timestamp per message. Text-only tantivy indexes typically land
between one third and one half of the raw text. For a history of 500 000
messages averaging 80 bytes of text, the raw text is about 40 MB and the index
should land between 20 MB and 60 MB. This is small next to the 208 MB media
store that already exists.

Two costs are larger than that estimate suggests, and both come from the
per-room design:

- **Per-room directory overhead.** There is one tantivy index per room, and
  Daniel has 365 rooms. Each index carries metadata files even when it holds
  few documents. Indexes are created lazily on first write or first search, so
  this only applies to rooms that were actually indexed, but a full backfill
  over 365 rooms creates 365 directories.
- **Writer memory.** `TANTIVY_INDEX_MEMORY_BUDGET` is 50 000 000 bytes
  (`matrix-sdk-search-0.18.0/src/lib.rs:7`), and `get_writer` creates a writer
  with that budget for every `execute` or `bulk_execute` call
  (`index/mod.rs:87`). The writer is dropped at the end of the call, so this
  is churn rather than a steady 50 MB per room, but a backfill that walks many
  rooms will allocate and release a 50 MB arena repeatedly. This is a
  performance risk to measure, not a correctness one.

**Event cache size.** Turning the event cache on is a second, larger storage
cost, and it is not optional, because the index is fed from it. The event
cache holds the full JSON of each event, not just its text. Expect it to be
several times the size of the index. A first backfill of a few large rooms is
the cheap way to measure this before committing to all 365.

**A NixOS rebuild that wipes caches.** Both stores live under
`dirs::data_dir()`, not `dirs::cache_dir()`. `sqlite_dir` is
`<data>/profiles/<profile>/sqlite` (`src/config.rs:1194`). Only `layout_json`
lives under the cache directory (`src/config.rs:1204`). So a cache wipe does
not destroy the index if the index is placed beside the sqlite directory,
which is the recommendation. A wipe of the data directory destroys the
session, which is a larger problem than the index.

**Rebuildability.** The index must be treated as fully disposable, and
`:reindex` must be able to rebuild it from nothing. This holds because every
input is re-fetchable: the events come from `/messages` on the server, and the
keys come from the key backup, which now works. Rebuilding is slow, not
impossible. Design the command so that deleting the index directory and
running `:reindex` is a supported, documented recovery.

## 5. Scope: what to build, and what not to build

A complete index of all 365 rooms over all history is a large project mostly
because of the backfill, not the index. Propose it in three stages, each of
which delivers value alone.

**Stage 1: new messages, all rooms.** Subscribe the event cache, enable the
feature, add the encrypted store kind, and route encrypted rooms in `:search`
to the local index. Daniel can immediately search anything said from that day
onward. This is the small, safe piece.

**Stage 2: `:reindex` for a named room.** Backfill one room on demand. Daniel
chooses which conversations matter. This gives most of the practical value for
a fraction of the cost, because the rooms a person wants to search are few.

**Stage 3: bulk backfill with a bound.** `:reindex --all --since 6mo`, run in
the background across rooms, with a time bound so the cost is predictable.

If only one stage is built, build stage 2. "Index the rooms I ask for" is
smaller than "index the last N months", because it needs no scheduling and no
policy, and it matches how a person actually searches.

## 6. The privacy question

State this plainly rather than in a footnote.

A local full-text index of encrypted messages **defeats one of the properties
end-to-end encryption provides**. Encryption stops the homeserver and the
network from reading the messages. It never stopped the endpoint from reading
them, because the endpoint must read them to display them. But an endpoint
that displays a message and forgets it leaves nothing behind. An endpoint that
indexes it leaves the content on disk, indefinitely, in a form built for fast
retrieval. That is a different exposure, and it is one the user is choosing.

**Where it sits.** The index directory sits beside the sqlite stores, under
`~/.local/share/iamb/profiles/<profile>/`. The crypto store sits in the same
directory, unencrypted at rest, because iamb passes `None` as the store
passphrase. That crypto store holds the Megolm session keys. Anyone who can
read the crypto store can decrypt the message history anyway.

**What that implies for encrypting the index.** Encrypting the index buys less
than it appears to, precisely because the keys next to it are not encrypted.
An attacker with read access to the directory already wins. Encrypt it anyway,
for three reasons that are honest about their limits:

1. It costs one enum variant. `SearchIndexStoreKind::EncryptedDirectory` is
   already implemented and already tested upstream.
2. The threat it actually addresses is not a targeted attacker. It is a backup
   that leaves the machine, a stolen laptop with the disk unlocked at a later
   date, or a misconfigured sync. A plaintext tantivy index is trivially
   greppable in those situations. An encrypted one is not.
3. It sets the direction for encrypting the crypto and event stores with the
   same passphrase later, which is the change that would actually raise the
   floor.

**Do not overstate what the schema protects.** The body field is `TEXT` and
not `STORED`, so the index does not keep the sentence. It keeps every token of
every sentence, with the document each token came from. Reconstructing the
**set of words** in a message from an unencrypted tantivy index is
straightforward. Reconstructing word order is harder. Nobody should read "not
stored" as "the message is not on disk".

**The user must be told.** Local indexing must never turn itself on. It must
be an explicit setting in the config file, with the trade-off written next to
it, and the first `:reindex` should state what it is about to write and where.

## 7. Integration with `:search`

One command. Two sources. The other agent owns the command and the window.
This design contributes a source, not a competitor.

The natural seam is at the point where `:search` decides where to send a
query. The rule is per room, and it is decidable without asking the user:

- If a room is encrypted, ask the local index.
- If a room is not encrypted, ask the server.

`Room::is_encrypted` gives the answer, and `request_encryption_state` is
already called on the pagination path (`src/worker.rs:397`), so the state is
usually cached.

Two properties of the local source make the merge awkward, and the other
agent needs to know both:

1. **Ranking is not comparable.** The server returns its own rank order. The
   tantivy index returns a BM25 score order. Merging two ranked lists into one
   ranking would produce an order that means nothing. Merge by **timestamp**
   instead, newest first. A person searching their own history is usually
   looking for a moment, and time is the axis they can reason about.
2. **The local source is incomplete by construction.** A room whose history
   was never backfilled returns nothing, and "nothing" is indistinguishable
   from "no match" unless the interface says otherwise. The result window must
   be able to show a per-room note such as `3 rooms not indexed — :reindex`.
   This is the single most important interface requirement in this document.
   Silent incompleteness in a search tool destroys trust in it.

Concretely, propose to the other agent that the search source is an enum with
two variants behind one function that returns
`Vec<(OwnedRoomId, OwnedEventId, MilliSecondsSinceUnixEpoch)>` plus a list of
rooms that could not be searched. The window renders results and the
not-searched list together.

## 8. Known gaps in the upstream index

These are limits of `matrix-sdk-search` 0.18.0, not of this design. They
should be written into the user-facing documentation, because each of them is
a case where a search will fail to find something the user knows exists.

- **Only `MessageType::Text` is indexed.** `make_doc` returns
  `MessageTypeNotSupported` for every other type (`schema.rs:96`). Notices,
  emotes, and the captions and file names of images and files are all
  invisible to the local index. Bot output is frequently `m.notice`, so this
  gap is larger in practice than it looks.
- **Formatted bodies are not indexed separately.** Only the plain `body` is
  indexed, which is normally the right choice.
- **The feature is marked experimental upstream.** The feature gate is named
  `experimental-search`. Expect the API to change across SDK versions, and
  expect the index format to change with it. This is another reason the index
  must be disposable and rebuildable.
- **Commits are frequent.** `execute` commits and reloads the searcher per
  operation (`index/mod.rs:281`); `bulk_execute` commits once per batch. The
  sync path uses the bulk form, so this is acceptable, but a commit per sync
  response per room is still real work.

## 9. Decisions that belong to Daniel

1. **Turn on the event cache at all.** Everything else follows from this. It
   writes decrypted message JSON to disk for every room, not only the rooms
   that are searched. Is that acceptable?
2. **Encrypt the index, and where the passphrase comes from.** A prompt at
   startup is the only strong answer and it is hostile to a TUI that is
   expected to start unattended. A passphrase stored in the config file next
   to the index is close to no protection at all, but it still defeats the
   backup and stolen-disk cases in section 6. A third option is to derive it
   from the same source as a future store passphrase. Which trade does he
   want?
3. **Which rooms, and how far back.** Stage 2 says "the rooms you name". If
   Daniel would rather have "everything, once, overnight", that is stage 3 and
   it is a different amount of work and disk.
4. **Whether a failed local search is an error or a silent zero.** Section 7
   argues loudly for showing the un-indexed rooms. If Daniel finds that noisy,
   the alternative is a quieter indicator, but it must exist in some form.
5. **Whether `:reindex` may run while he uses iamb.** Backfill paginates
   aggressively and will compete for the request budget and for CPU. A
   foreground command that blocks is honest; a background one is convenient
   and will make the client feel slow at times.

## 10. Implementation shape

Roughly in order. Names are the files that change.

**`Cargo.toml`.** Add `experimental-search` to the `matrix-sdk` feature list
at line 92. This pulls in `matrix-sdk-search` 0.18.0 and therefore tantivy
0.25, aes, pbkdf2 and hmac. MSRV is unchanged: `matrix-sdk-search` 0.18.0
declares `rust-version = "1.93"`, which iamb already declares.

**`src/config.rs`.** Add a search directory beside `sqlite_dir` at line 1194,
built the same way, named `search-index`. Add a tunable that enables local
indexing, defaulting to off, and a place for the index passphrase or for the
choice of how it is obtained.

**`src/worker.rs`.** Two changes in `create_client_inner` near line 840: call
`.search_index_store(SearchIndexStoreKind::EncryptedDirectory(dir, pass))` on
the builder, and after the client is built and logged in, call
`client.event_cache().subscribe()`. The second call is what starts both the
event cache and the indexing task.

**`src/worker.rs`, pagination.** This is the largest piece and it is the one
that can be staged. `load_older_forward` at line 395 calls `room.messages`
directly. To feed the index during normal scrollback, that path must go
through `RoomEventCache::pagination` instead. Doing this changes how iamb gets
its scrollback, and it touches the `Messages`/`RoomInfo` model in
`src/base.rs`. It is worth treating as its own change, landed before or after
the search work, never inside it. Until it lands, only `:reindex` and sync
feed the index.

**A new `src/search.rs` or an addition to the other agent's module.** Hold the
per-room source selection, call `Client::search_messages` or
`Room::search_messages`, and return results with timestamps plus the list of
rooms that were not searched.

**`src/commands.rs`.** Add `:reindex`, with an optional room argument.

**`src/base.rs`.** Add worker messages for "start a backfill" and "backfill
progress", so the command can report and can be stopped.

**Size estimate.** Stage 1 is small: perhaps 150 lines across `Cargo.toml`,
`config.rs` and `worker.rs`, plus whatever the merge with `:search` needs.
Stage 2 is medium: the backfill loop, the progress reporting and the command,
perhaps 400 lines, and it is where the real work is. The pagination migration
in `load_older_forward` is separately medium to large and carries the most
risk, because it changes a path that currently works well. Do not fold it into
either stage.

**Risk.** The largest risk is not the index. It is that turning on the event
cache changes the behaviour of a client that currently keeps everything in
memory, in ways that reading the code does not predict. Run the first version
against a second profile, not against Daniel's account.

## 11. Alternatives considered

**Write a new sqlite FTS5 index in iamb.** This was the obvious answer before
reading the SDK. It loses because `matrix-sdk-search` already exists, already
handles edits and redactions correctly, already encrypts, and is maintained by
the people who maintain the SDK. A local FTS5 table would need all of that
built and kept correct. It wins on one axis only: it would not require the
event cache, because iamb could index from its own `Messages` map. If decision
1 in section 9 is "no, do not turn on the event cache", this alternative comes
back, and it is then the right answer.

**Reuse the event cache alone, with `LIKE` queries over the events table.**
This needs no new dependency. It loses on quality and speed: no tokenizing, no
stemming, no ranking, no edit handling, and a full scan of a JSON blob column
per query. It is a reasonable half-day spike to prove the event cache holds
what is claimed, and a poor product.

**Index only at sync time, and accept that history is unsearchable.** This is
stage 1 alone. It loses because Daniel said clearly that history is the thing
he wants. It is still the correct first thing to land.

**Encrypt nothing, on the grounds that the crypto store is already
unencrypted.** The argument is coherent, and section 6 concedes most of it.
It loses because the encrypted variant costs one line and the plaintext
variant makes a laptop backup trivially greppable.

**Search the server for encrypted rooms and accept empty results.** This is
today's behaviour. It loses because it is the problem.

## 12. What could not be determined from the code

- **Whether late-decrypted events reach the index.** The redecryptor posts a
  `RoomEventCacheUpdate` (`redecryptor.rs:418`) while the indexing task
  subscribes to `RoomEventCacheLinkedChunkUpdate`
  (`event_cache/tasks.rs:471`). Whether the post-processing step also emits a
  linked chunk update was not traced to a conclusion. This matters a great
  deal given the key backup recovery. Settle it with a test before building
  stage 2, not by reading further.
- **The real size of the event cache for a 365-room account.** The store is
  empty, so no measurement is possible. Only a backfill of a few real rooms
  will answer it.
- **How `/messages` pagination through the event cache differs in rate-limit
  behaviour from the direct call iamb makes today.** A backfill across many
  rooms will meet server limits, and the SDK's handling of that was not
  examined.
- **Whether the per-room index count of 365 causes file descriptor or mmap
  pressure.** Indexes are lazily created, and the map is held for the process
  lifetime (`search_index/mod.rs:59`). Nothing evicts them. A long session
  that searches everything holds every index open.
- **What the tantivy `QueryParser` does with a query a user types casually.**
  A bare colon or an unbalanced quote produces a `QueryParserError`, which
  `execute_with_retry` bubbles rather than swallows (`index/mod.rs:262`). The
  interface must turn that into a readable message, and the exact set of
  inputs that trigger it was not enumerated.
- **Whether `matrix-sdk-search` compiles cleanly with iamb's
  `default-features = false` on `matrix-sdk`.** The feature declaration
  (`experimental-search = ["matrix-sdk-search"]`) suggests no coupling to
  `sqlite` or `e2e-encryption`, but this was not built.
