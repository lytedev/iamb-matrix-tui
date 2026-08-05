# Snooze: defer an item out of the inbox without reading it

Status: proposal. No code is written yet.

Every `matrix-sdk` path in this document was read from
`matrix-sdk-0.18.0`, which is the version `Cargo.lock` pins.

## The problem

`:unreadsandthreads` is the inbox. An item leaves that list only when it becomes
read. Marking an item read is destructive: it moves a read receipt, other
clients see the receipt, and the only way back is `:undoread`, which holds a
bounded in-memory stack that a restart discards.

Some items must leave the inbox without being read. An example is a thread that
is important, but that is not actionable until tomorrow. Today the only choices
are to read it, which loses the information that it is unread, or to leave it in
the list, which makes the list useless.

Snooze separates two things that the current model ties together: whether the
user has read an item, and whether the user wants to see the item now. A
receipt answers the first question. A snooze answers the second.

## Recommendation in short

Store one number per key: a wake time. The key is the same key `:read` uses,
which is a room plus an optional thread root. Store that number in **per-room**
account data, so the annotation lives on the room it describes.

Treat the wake time as a **virtual unread timestamp**. While the time is in the
future, the item is hidden. Once the time passes, the item is the most recent
thing in the list and sorts to the top of the inbox. Read receipts are never
touched. Desktop notifications for the item are suppressed until it wakes.

## 1. Granularity

The unread model computes unread state at exactly two granularities.
`RoomInfo::unreads` compares the newest message in the room against the main and
unthreaded receipts (`src/base.rs:1453`). `RoomInfo::thread_unreads` compares
the newest reply in one thread against that thread's receipt
(`src/base.rs:1186`).

`:unreadsandthreads` builds its list from those two functions only. The window
mixes `GenericChatItem` values for rooms with `ThreadItem` values for threads
(`src/windows/mod.rs:748`). No entry in that list is a message.

Therefore snooze needs one mechanism with one key, and that key is the existing
`ReadTarget` in `src/windows/mod.rs:361`. Room and thread are not different
features. They are the same feature with a different key, in the same way that
`:read` is.

A message is a third thing, and it is not directly representable. A
message-scoped command is still worth offering as a convenience that *resolves*
to a coarser key: a message in a thread snoozes that thread, and a message
outside any thread snoozes the room. The command reports which key it chose, so
the user is never misled about the scope. A true per-message snooze would need a
new concept inside the scrollback, which does not serve the inbox use case.

The `:undoread` trap does not apply with equal force here. Room-level `:read` is
dangerous because one keystroke moves many thread receipts at once and the
recovery is bounded. A room-level snooze moves nothing and expires on its own,
so a mistake costs the user a few hours of not seeing one room. The important
restriction is the opposite one: a room snooze must not silently imply a snooze
of the threads in that room, because those threads are separate inbox entries.
See section 9.

## 2. Representation: a virtual unread time

The state for one key is a single timestamp. Nothing else is stored. That one
number does the work that a separate hidden flag and a separate wake mechanism
would otherwise do.

- **Hiding** is "the virtual time is in the future".
- **Waking** is the absence of hiding. No wake event, no scheduler, and no
  re-ordering code exists, because nothing has to happen at the wake instant.
- **Pruning** is free. An expired entry and an absent entry behave identically,
  so a stale entry is harmless and can be dropped whenever the client next
  writes the room.

### Does this survive contact with the sort code?

It does, and more cleanly than expected. This was checked rather than assumed.

`SortFieldRoom::Recent` does not reach into `RoomInfo` for a message timestamp.
It compares `RoomLikeItem::recent_ts` (`src/windows/mod.rs:213`), and every
implementation of `recent_ts` returns `self.unread.latest()` — the `latest`
field of the `UnreadInfo` value that the item already owns
(`src/windows/mod.rs:1180`, `:1388`, `:1513`, `:1628`). `UnreadInfo` is built
once per item at construction, from `info.unreads(settings)` for a room
(`src/windows/mod.rs:1345`) or from `thread_unreads` inside `followed_threads`
for a thread (`src/base.rs:1131`).

So the sort already reads a per-item field that the item constructor fills in.
Substituting a virtual time there is a substitution at exactly the layer the
sort was written against. It is not a hack that fights the sort. Nothing needs
to be threaded through `RoomInfo`, and no lifetime problem arises, because
`recent_ts` returns a reference into a field the item owns.

The rule at construction is: when a wake time exists for the key, the item's
`latest` becomes the later of the real newest-message time and the wake time.
While snoozed, that is the wake time, and the item is filtered out anyway. After
waking, the item carries a timestamp of roughly "now", so under Daniel's
`["favorite", "unread", "recent", "alias", "lowpriority"]` order it lands at the
top of the unread group. That is the intended behaviour: a woken item is a
reminder and belongs above older unread traffic.

`MessageTimeStamp` is an enum of `OriginServer(UInt)` and `LocalEcho`
(`src/message/mod.rs:220`), so a wake time is represented as an `OriginServer`
value holding milliseconds. This is a fabricated server timestamp, which is the
one genuinely uncomfortable part of the unification, and it is why hiding must
not be built on `latest` alone: see the honest caveats below.

### Where the unification does not quite hold

Three things must be stated plainly.

**Hiding cannot ride on `is_unread` without a cost.** The three unread windows
filter on `RoomLikeItem::is_unread` (`src/windows/mod.rs:736`, `:749`, `:755`).
Making `is_unread` return false while snoozed would give hiding for free, but
`is_unread` is also what draws the bold name and the "Unread" label
(`name_and_labels`, `src/windows/mod.rs:137`) and what `SortFieldRoom::Unread`
compares. A snoozed room would then look read in `:rooms` and `:chats`. The
recommendation is to keep `is_unread` truthful and add one predicate,
`is_deferred`, to `RoomLikeItem`, and to filter on `is_unread && !is_deferred`
in the three inbox windows. That is a few lines, and it keeps the honest meaning
of "unread" intact everywhere else in the client.

**The quick switcher also reads `latest`.** `src/windows/switcher.rs:181` ranks
Ctrl-K results by `recency(unread.latest())`. A woken item therefore also ranks
high in the switcher. This is probably desirable and is called out so that it is
a decision rather than a surprise.

**A virtual time is a lie about the message.** `latest` is documented as the
newest message time. After this change it becomes "the time this item wants
attention". The field should be renamed or re-documented in the same change, or
the next reader will treat it as a message timestamp.

None of these defeats the idea. The single stored number stands, the sort
substitution is clean, and the extra predicate is small. This is a better design
than the separate snooze table with its own filter, mainly because waking needs
no code at all.

## 3. Where the state lives

**Recommended: per-room account data.** `Room::set_account_data_raw` exists in
the pinned version at `matrix-sdk-0.18.0/src/room/mod.rs:1655`, with
`Room::account_data_static` at `:1568` to read it back. One event of a private
type, for example `dev.lyte.iamb.snooze`, on each room that has a snooze. The
content holds the room's own wake time, if any, and a small map from thread root
to wake time.

Per-room beats one global event on three counts. A concurrent write from another
device can only clobber a snooze in the *same room at the same instant*, rather
than any snooze anywhere. No single event grows without bound. Pruning is local:
the client rewrites one room's event and drops that room's expired entries,
instead of rewriting the whole table to prune anything.

The properties that made account data the right family in the first place still
hold. The homeserver stores it and pushes it down the sync stream, so a snooze
survives a NixOS rebuild that wipes the cache directory, and it reaches a second
device. Other Matrix clients ignore room account data event types they do not
know, so no other client changes behaviour because of this event.

The costs are unchanged and real. A snooze becomes a network write, so it fails
when the client is offline. The code must keep the in-memory value and report
the failure, so that the snooze works for the session even if it does not
survive a restart.

**Rejected: a local file under the profile directory.** The pattern exists,
because `session.json` lives there (`src/config.rs:1223`), and it never fails
offline. It loses the state on a cache or data wipe and it does not reach a
second device. Given that this fork is pinned into a NixOS configuration and
rebuilt often, a wipe is a real event rather than a theoretical one.

**Rejected: the `matrix-sdk` state store.** Same durability as a local file,
more coupling to SDK internals, no gain.

## 4. Expiry

A snooze holds an absolute wake time in UTC milliseconds, never a duration.
Absolute times are what the comparison needs, and they are what keeps the value
meaningful after a restart.

Expiry is lazy and needs no timer. `IambWindow::refresh` rebuilds list contents
each time a window is filled, and the main loop polls terminal input with a one
second timeout (`src/main.rs:385`), so a redraw happens at least once per second
while the client runs. An item whose wake time has passed simply stops being
filtered out, and carries a recent virtual time, so it appears at the top.

Waking is silent. The item reappears in `:unreadsandthreads` and nothing else
happens. A notification at wake time would be a second, larger feature: it
requires the client to be running at that moment, and it requires a decision
about a wake that fires while the client is closed. The inbox is the
notification.

If another client reads the room while the item is snoozed, the item becomes
read. Unread state is computed fresh from receipts on every rebuild, so the item
never reappears, and its stale wake time is pruned on the next write to that
room. Nothing special is needed. This is a point in favour of deriving
visibility on each rebuild rather than storing a hidden flag on the item.

## 5. Notifications while snoozed

Confirmed: a snoozed item must not produce a desktop notification. An item that
still buzzes is not deferred, and dismissing notifications for an item the user
has deliberately put down teaches the user to ignore notifications generally.

The check belongs in `register_notifications` (`src/notifications.rs:112`),
beside the existing `is_visible_room` suppression. That path already computes
the thread root of the notified event (`event_thread_root`,
`src/notifications.rs:428`) and already builds a `NotificationTarget` of room
plus optional thread root (`src/notifications.rs:52`). That target is the snooze
key, so the suppression is one lookup in code that already holds the right
values.

The clickable-notification and focus-tui path is untouched. Suppression happens
before a notification is created, so no handle is registered, and
`open_notifications` and `notification_jump` never see a snoozed item. There is
no path where a suppressed notification leaves a dangling handle.

One consequence to accept: a direct mention inside a snoozed thread is also
silenced. That is correct for a deferral primitive, and the user can cancel a
snooze. Letting mentions break through is possible later, but it re-introduces
the buzzing that the feature exists to stop.

## 6. Commands

Four commands, all composable in macros the way `:read` and
`:unreadsandthreads` are. Each returns a normal action step from
`src/commands.rs`, so each works inside a macro sequence.

- `:snooze <when>` — snooze the current context. In a list window this acts on
  the selected entry, exactly as `:read` does through the `Readable` trait
  (`src/windows/mod.rs:377`). In a room window it acts on the room, or on the
  thread when a thread is open. With a message selected it resolves as described
  in section 1 and reports the chosen key.
- `:snooze` with no argument — use a configurable default duration.
- `:snoozed` — a list window of everything snoozed, with wake times. Deferral
  the user cannot audit becomes deferral the user does not trust.
- `:unsnooze` — cancel the snooze on the current or selected item, which means
  clearing the one stored number. In the `:snoozed` window it acts on the
  selected entry.

Duration syntax: relative durations first, because they are what a user types in
the middle of work. A plain number with a unit suffix covers it: `30m`, `2h`,
`3d`, `1w`. Add two named times, `tomorrow` and `nextweek`, resolved against
local time with a configurable hour of day. Absolute timestamps are left out of
the first version: they need a date parser, they are rarely what the user wants
at the moment of deferral, and adding them later breaks nothing.

Keybindings are not proposed. Daniel drives everything from macros, and the
binding choice is his.

Configuration: one tunable for the default duration, and one for the hour that
`tomorrow` means. Both fit the `TunableValues` pattern (`src/config.rs:701`).

## 7. Interaction with the existing read model

Snooze writes no receipt and reads no receipt. `set_receipt`, `rewind_receipt`,
`mark_read`, `fully_read` and `record_read` are untouched. `:undoread` does not
interact with snooze, and a snooze never appears on the read-undo stack, because
nothing was read.

The receipt-restore-on-start path in `load_thread_receipts` (`src/worker.rs`,
added in `51d756a`) is not affected. That code asks the client's local store
where each thread receipt sits and feeds the answer through `set_receipt`. It
consults no unread state and no list window. A snoozed-but-unread thread
restores its receipt exactly as any other thread does and stays unread, which is
the intent. The stale-receipt guard in `receipt_is_stale` is likewise
unaffected, because snooze produces no receipt for it to judge.

With the `is_deferred` predicate recommended in section 2, a snoozed item is
still genuinely unread everywhere else in the client: it still shows as unread
in `:rooms` and `:chats`. Only the inbox windows and the notification path
filter on it. Snooze hides an item from the place the user triages, not from the
client.

## 8. Failure modes

**Crash mid-snooze.** The room account data write is the commit point. A crash
before it loses the snooze and the item stays in the inbox. A crash after it
keeps the snooze. Neither outcome corrupts read state.

**Clock change.** Wake times are absolute UTC milliseconds, so a timezone change
does not move them. A large backwards correction extends a snooze and a large
forwards correction wakes items early. Both are acceptable, because the worst
case is an unread item appearing at the wrong time. No monotonic-clock work is
justified.

**Very long snooze.** Nothing breaks. Pruning is by expiry, never by age, so a
far-future wake time survives. The `:snoozed` window makes a forgotten long
snooze findable.

**Deleted or redacted item.** A redacted thread root or a room the user left
leaves an entry that names nothing. Such an entry never matches an item, so it
is inert, and per-room storage means it disappears with the room's account data.
Drop entries for threads that no longer exist on the next write.

**Write failure.** Report it and keep the in-memory value, so the snooze holds
for the session.

## 9. Decisions that belong to Daniel

1. **Does a room snooze cover the threads in that room?** The recommendation is
   no, because threads are independent inbox entries. The cost of "no" is that
   snoozing a busy room does not quiet it, and each noisy thread must be snoozed
   separately. This is a judgement about how his inbox behaves.
2. **Does new activity wake an item early?** The recommendation is no: the wake
   time holds regardless of new messages, because "wake on activity" makes a
   snooze useless in exactly the rooms that need it. The argument for "yes" is
   that a new reply on a deferred thread may be the awaited event.
3. **Does a direct mention break through a snooze?** The recommendation is no,
   for the first version. See section 5.
4. **Should a woken item rank high in the Ctrl-K switcher too?** This falls out
   of the virtual time. It is easy to exclude if unwanted.
5. **Command names.** `:snooze` and `:unsnooze` are proposed. `:defer` and
   `:later` are equally reasonable.
6. **Default duration, and the hour `tomorrow` means.**

## 10. Implementation shape

New state in `ChatStore` (`src/base.rs`): a map from snooze key to wake time,
where the key is the `ReadTarget` shape made hashable. It is a cache of what the
room account data events say. One helper answers "what is the wake time for this
key", and one answers "is that time in the future".

Files that change:

- `src/base.rs` — the cache, the key type, `IambAction::Snooze` and
  `IambAction::Unsnooze` with their sequence-status arms, and one error for an
  unparsable duration.
- `src/windows/mod.rs` — `is_deferred` on `RoomLikeItem` and its
  implementations; the `latest` substitution at the four item constructors and
  in `followed_thread_items`; `&& !is_deferred` on the three inbox filters; a
  `Snoozable` trait mirroring `Readable`; the `:snoozed` window.
- `src/commands.rs` — four command definitions and a duration parser, following
  `iamb_read` and `iamb_undoread`.
- `src/notifications.rs` — one suppression check beside `is_visible_room`.
- `src/worker.rs` — read each room's snooze event at start and on sync, and
  write it on change.
- `src/config.rs` — two tunables.
- `src/windows/palette.rs` and `docs/iamb.1` — document the new commands, as the
  earlier fork features did.

One mechanical note: the item constructors already split borrows of
`ChatStore`, taking `rooms` and `settings` separately
(`src/windows/mod.rs:1342`, and the `let ChatStore { rooms, settings, .. }` in
`followed_thread_items`). Adding the snooze cache as a third field borrow
follows the same pattern and does not fight the borrow checker.

Size estimate: roughly 400 to 600 lines including tests, of which `:snoozed` is
the largest single piece and can be deferred. The virtual-time model is smaller
than the table-plus-filter alternative, because waking needs no code. Risk is
low: the feature is additive, it writes no receipt, and its worst failure
returns the client to today's behaviour. The account data path carries the only
real unknown, because this fork uses no account data today.

Suggested sequencing, one concern per change: the in-memory wake times with
`:snooze`, `:unsnooze`, `is_deferred` and the `latest` substitution; then
notification suppression; then per-room account data persistence; then the
`:snoozed` window.

## 11. Alternatives considered

**A separate snooze record with its own filter and its own wake path.** This was
the first version of this document. It loses to the virtual time because it
needs two mechanisms where one number suffices, and because it must decide what
a wake does, whereas the virtual time makes waking the natural sort result.

**A pin or bookmark that marks the item read.** Rejected: it destroys read
state, which is the problem being solved. A durable bookmark list remains a good
idea as a separate feature, orthogonal to read state and to snooze.

**A per-item hidden flag with no expiry.** Rejected. A hide with no deadline is
a silent way to lose items and needs an unhide the user must remember. The
deadline is what makes deferral safe.

**Snooze as a read plus a scheduled un-read.** Rejected. It moves a real
receipt, other clients see it, and any interruption between the two steps leaves
a permanently wrong receipt. This is the failure mode the design exists to
avoid.

**A background timer firing at each wake time.** Rejected as unnecessary. Lazy
expiry at rebuild time gives the same result, given a redraw at least once per
second.

**One global account data event holding the whole table.** Rejected in favour of
per-room. It concentrates write races across unrelated rooms, grows without
bound, and forces a whole-table rewrite to prune one stale entry.

## 12. What could not be determined from the code

- Whether `matrix-sdk` 0.18 surfaces room account data changes through a handler
  this fork can subscribe to, or whether the worker must check after each sync.
  Both the read and write APIs are confirmed present at the paths cited above;
  the change-notification path was not traced.
- How the events behave across two clients running at once. The write-race
  analysis comes from how account data is defined, not from testing.
- Whether any homeserver limit applies to the size of a room account data event.
  The per-room content is small, but no limit was verified.
