//! Message scrollback
use ratatui_image::Image;
use regex::Regex;

use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId};

use modalkit_ratatui::{ScrollActions, TerminalCursor, WindowOps};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, StatefulWidget, Widget},
};

use modalkit::actions::{
    Action,
    CursorAction,
    EditAction,
    Editable,
    EditorAction,
    EditorActions,
    HistoryAction,
    InsertTextAction,
    Jumpable,
    PromptAction,
    Promptable,
    Scrollable,
    Searchable,
    SelectionAction,
    WindowAction,
};
use modalkit::editing::{
    completion::CompletionList,
    context::Resolve,
    cursor::{CursorGroup, CursorState},
    history::HistoryList,
    rope::EditRope,
    store::{RegisterCell, RegisterPutFlags},
};
use modalkit::errors::{EditError, EditResult, UIError, UIResult};
use modalkit::prelude::*;

use crate::{
    base::{
        IambBufferId,
        IambId,
        IambInfo,
        IambResult,
        ProgramContext,
        ProgramStore,
        RoomFetchStatus,
        RoomFocus,
        RoomInfo,
    },
    config::ApplicationSettings,
    message::{Message, MessageCursor, MessageKey, Messages, ThreadRange, ThreadView},
    preview::PreviewManager,
};

fn no_msgs() -> EditError<IambInfo> {
    let msg = "No messages to select.";
    EditError::Failure(msg.to_string())
}

fn nth_key_before(pos: MessageKey, n: usize, thread: &ThreadView) -> MessageKey {
    let mut end = &pos;
    let iter = thread.range(..=&pos).rev().enumerate();

    for (i, (key, _)) in iter {
        end = key;

        if i >= n {
            break;
        }
    }

    end.clone()
}

fn nth_before(pos: MessageKey, n: usize, thread: &ThreadView) -> MessageCursor {
    let key = nth_key_before(pos, n, thread);

    if matches!(thread.last_key_value(), Some((last, _)) if &key == last) {
        MessageCursor::latest()
    } else {
        MessageCursor::from(key)
    }
}

fn nth_key_after(pos: MessageKey, n: usize, thread: &ThreadView) -> Option<MessageKey> {
    let mut end = &pos;
    let mut iter = thread.range(&pos..).enumerate();

    for (i, (key, _)) in iter.by_ref() {
        end = key;

        if i >= n {
            break;
        }
    }

    // Avoid returning the key if it's at the end.
    iter.next().map(|_| end.clone())
}

fn nth_after(pos: MessageKey, n: usize, thread: &ThreadView) -> MessageCursor {
    nth_key_after(pos, n, thread).map(MessageCursor::from).unwrap_or_default()
}

fn prevmsg<'a>(key: &MessageKey, thread: &ThreadView<'a>) -> Option<&'a Message> {
    thread.range(..key).next_back().map(|(_, v)| v)
}

pub struct ScrollbackState {
    /// The room identifier.
    room_id: OwnedRoomId,

    /// The buffer identifier used for saving marks, etc.
    id: IambBufferId,

    /// The currently focused thread in this room.
    thread: Option<OwnedEventId>,

    /// The currently selected message in the scrollback.
    cursor: MessageCursor,

    /// Contextual info about the viewport used during rendering.
    viewctx: ViewportContext<MessageCursor>,

    /// The jumplist of visited messages.
    jumped: HistoryList<MessageCursor>,

    /// Whether the full message should be drawn during the next render() call.
    ///
    /// This is used to ensure that ^E/^Y work nicely when the cursor is currently
    /// on a multiline message.
    show_full_on_redraw: bool,

    /// A message that should be selected as soon as it has been loaded.
    pending_selection: Option<PendingSelection>,

    /// Where a visual selection started.
    ///
    /// The selection covers every message between this message and the cursor. It is `None`
    /// whenever the user is not in visual mode.
    selection_anchor: Option<MessageCursor>,
}

/// A message that was asked for before it was loaded, to be selected once it arrives.
///
/// Clicking a notification can name a message that this scrollback has not backfilled yet, so the
/// selection has to wait for the load to finish rather than block the main loop on it.
#[derive(Clone)]
struct PendingSelection {
    /// The message to select once it is loaded.
    event_id: OwnedEventId,

    /// Where the cursor was when the selection was requested.
    ///
    /// If the cursor has moved since then, the user has started reading something else, and a late
    /// arrival should no longer yank them away from it.
    cursor: MessageCursor,
}

impl ScrollbackState {
    pub fn new(room_id: OwnedRoomId, thread: Option<OwnedEventId>) -> ScrollbackState {
        let id = IambBufferId::Room(room_id.to_owned(), thread.clone(), RoomFocus::Scrollback);
        let cursor = MessageCursor::default();
        let viewctx = ViewportContext::default();
        let jumped = HistoryList::default();
        let show_full_on_redraw = false;

        ScrollbackState {
            room_id,
            id,
            thread,
            cursor,
            viewctx,
            jumped,
            show_full_on_redraw,
            pending_selection: None,
            selection_anchor: None,
        }
    }

    pub fn is_latest(&self) -> bool {
        self.cursor.timestamp.is_none()
    }

    pub fn goto_latest(&mut self) {
        self.cursor = MessageCursor::latest();
    }

    pub fn goto_message(&mut self, target: MessageKey) {
        let mut cursor = MessageCursor::new(target, 0);
        std::mem::swap(&mut cursor, &mut self.cursor);
        self.jumped.push(cursor);

        // The render stops as soon as it has the first row of the selected message and a full
        // pane. A jump can land on a message whose first row is only a date or a sender, and the
        // user then sees no part of what they asked for. A motion sets this for the same reason.
        self.show_full_on_redraw = true;
    }

    /// Move the cursor onto `event_id`, if that message is loaded in this scrollback.
    ///
    /// Returns whether the message was there to select. A message can be known to the room but
    /// absent here, because a thread's scrollback only holds that thread's replies.
    pub fn goto_event(&mut self, event_id: &EventId, info: &RoomInfo) -> bool {
        let Some(key) = info.get_message_key(event_id) else {
            return false;
        };

        let Some(thread) = self.get_thread(info) else {
            return false;
        };

        if !thread.contains_key(key) {
            return false;
        }

        let key = key.clone();
        self.goto_message(key);

        true
    }

    /// Select `event_id` once it has been loaded.
    ///
    /// Loading it is the caller's job; this only watches for it to show up.
    pub fn select_when_loaded(&mut self, event_id: OwnedEventId) {
        self.pending_selection = Some(PendingSelection { event_id, cursor: self.cursor.clone() });
    }

    /// Select the message requested by [ScrollbackState::select_when_loaded], if it has arrived.
    fn take_pending_selection(&mut self, info: &RoomInfo) {
        let Some(pending) = self.pending_selection.take() else {
            return;
        };

        if pending.cursor != self.cursor {
            // The user has moved on; stop waiting.
            return;
        }

        if !self.goto_event(&pending.event_id, info) {
            self.pending_selection = Some(pending);
        }
    }

    /// Set the dimensions and placement within the terminal window for this list.
    pub fn set_term_info(&mut self, area: Rect) {
        self.viewctx.dimensions = (area.width as usize, area.height as usize);
    }

    pub fn get_key(&self, info: &mut RoomInfo) -> Option<MessageKey> {
        self.cursor
            .timestamp
            .clone()
            .or_else(|| self.get_thread(info)?.last_key_value().map(|kv| kv.0.clone()))
    }

    pub fn get_mut<'a>(&mut self, info: &'a mut RoomInfo) -> Option<&'a mut Message> {
        let key = self.get_key(info)?;

        if let Some(root) = &self.thread {
            if &key.1 == root {
                // The message a thread is about belongs to the main scrollback, so a change to it
                // goes through the room rather than through the thread's own map.
                return info.get_event_mut(root);
            }
        }

        self.get_thread_mut(info).get_mut(&key)
    }

    pub fn thread(&self) -> Option<&OwnedEventId> {
        self.thread.as_ref()
    }

    /// The messages this scrollback shows.
    ///
    /// In a thread this is the root followed by the replies. The root is a message like any other
    /// here: the cursor can land on it, and it scrolls out of view.
    pub fn get_thread<'a>(&self, info: &'a RoomInfo) -> Option<ThreadView<'a>> {
        let replies = info.get_thread(self.thread.as_deref());

        let Some(root) = self.thread.as_deref() else {
            return Some(ThreadView::from(replies?));
        };

        let root = info.get_message_key(root).zip(info.get_event(root));

        if replies.is_none() && root.is_none() {
            // Neither the replies nor the message the thread is about have been loaded.
            return None;
        }

        Some(ThreadView::with_root(replies, root))
    }

    pub fn get_thread_mut<'a>(&self, info: &'a mut RoomInfo) -> &'a mut Messages {
        info.get_thread_mut(self.thread.clone())
    }

    pub fn messages<'a>(
        &self,
        range: EditRange<MessageCursor>,
        info: &'a RoomInfo,
    ) -> ThreadRange<'a> {
        let Some(thread) = self.get_thread(info) else {
            return Default::default();
        };

        let start = range.start.to_key(&thread);
        let end = range.end.to_key(&thread);

        let (start, end) = if let (Some(start), Some(end)) = (start, end) {
            (start, end)
        } else if let Some((last, _)) = thread.last_key_value() {
            (last, last)
        } else {
            return thread.range(..);
        };

        if range.inclusive {
            thread.range(start..=end)
        } else {
            thread.range(start..end)
        }
    }

    fn need_more_messages(&self, info: &RoomInfo) -> bool {
        match info.fetch_id {
            // Don't fetch if we've already hit the end of history.
            RoomFetchStatus::Done => return false,
            // Fetch at least once if we're viewing a room.
            RoomFetchStatus::NotStarted => return true,
            _ => {},
        }

        let first_key = self.get_thread(info).and_then(|t| t.first_key_value()).map(|(k, _)| k);
        let at_top = first_key == self.viewctx.corner.timestamp.as_ref();

        match (at_top, self.thread.as_ref()) {
            (false, _) => {
                // Not scrolled to top, don't fetch.
                false
            },
            (true, None) => {
                // Scrolled to top in non-thread, fetch.
                true
            },
            (true, Some(thread_root)) => {
                // Scrolled to top in thread, fetch until we have the thread root.
                //
                // Typically, if the user has entered a thread view, we should already have fetched
                // all the way back to the thread root, but it is technically possible via :threads
                // or when restoring a thread view in the layout at startup to not have the message
                // yet.
                !info.keys.contains_key(thread_root)
            },
        }
    }

    fn scrollview(
        &mut self,
        idx: MessageKey,
        pos: MovePosition,
        info: &RoomInfo,
        settings: &ApplicationSettings,
        previews: &PreviewManager,
    ) {
        let Some(thread) = self.get_thread(info) else {
            return;
        };

        let selidx = if let Some(key) = self.cursor.to_key(&thread) {
            key
        } else {
            return;
        };

        match pos {
            MovePosition::Beginning => {
                self.viewctx.corner = idx.into();
            },
            MovePosition::Middle => {
                let mut lines = 0;
                let target = self.viewctx.get_height() / 2;

                for (key, item) in thread.range(..=&idx).rev() {
                    let sel = selidx == key;
                    let prev = prevmsg(key, &thread);
                    let len =
                        item.show(prev, sel, &self.viewctx, info, settings, previews).lines.len();

                    if key == &idx {
                        lines += len / 2;
                    } else {
                        lines += len;
                    }

                    if lines >= target {
                        // We've moved back far enough.
                        self.viewctx.corner.timestamp = key.clone().into();
                        self.viewctx.corner.text_row = lines - target;
                        break;
                    }
                }
            },
            MovePosition::End => {
                let mut lines = 0;
                let target = self.viewctx.get_height();

                for (key, item) in thread.range(..=&idx).rev() {
                    let sel = key == selidx;
                    let prev = prevmsg(key, &thread);
                    let len =
                        item.show(prev, sel, &self.viewctx, info, settings, previews).lines.len();

                    lines += len;

                    if lines >= target {
                        // We've moved back far enough.
                        self.viewctx.corner.timestamp = key.clone().into();
                        self.viewctx.corner.text_row = lines - target;
                        break;
                    }
                }
            },
        }
    }

    fn jump_changed(&mut self) -> bool {
        self.jumped.current() != &self.cursor
    }

    fn push_jump(&mut self) {
        self.jumped.push(self.cursor.clone());
    }

    fn shift_cursor(
        &mut self,
        info: &RoomInfo,
        settings: &ApplicationSettings,
        previews: &PreviewManager,
    ) {
        let Some(thread) = self.get_thread(info) else {
            return;
        };

        let last_key = if let Some(k) = thread.last_key_value() {
            k.0
        } else {
            return;
        };

        let corner_key = self.viewctx.corner.timestamp.as_ref().unwrap_or(last_key);

        if self.cursor < self.viewctx.corner {
            // Cursor is above the viewport; move it inside.
            self.cursor = corner_key.clone().into();
        }

        // Check whether the cursor is below the viewport.
        let mut lines = 0;

        let cursor_key = self.cursor.timestamp.as_ref().unwrap_or(last_key);
        let mut prev = prevmsg(cursor_key, &thread);

        for (idx, item) in thread.range(corner_key.clone()..) {
            if idx == cursor_key {
                // Cursor is already within the viewport.
                break;
            }

            lines += item
                .show(prev, false, &self.viewctx, info, settings, previews)
                .height()
                .max(1);

            if lines >= self.viewctx.get_height() {
                // We've reached the end of the viewport; move cursor into it.
                self.cursor = idx.clone().into();
                break;
            }

            prev = Some(item);
        }
    }

    fn _range_to(&self, cursor: MessageCursor) -> EditRange<MessageCursor> {
        EditRange::inclusive(self.cursor.clone(), cursor, TargetShape::LineWise)
    }

    /// Start or end a visual selection to match the mode the user is in.
    ///
    /// Visual mode is the only mode that sets a target shape, so the shape says whether a
    /// selection is running. Leaving visual mode sends no action to the scrollback, so the end of
    /// a selection can only be seen on the action that follows it.
    ///
    /// The anchor holds a resolved key rather than the cursor itself, because the cursor of an
    /// unscrolled scrollback names no message yet.
    fn track_selection(&mut self, key: &MessageKey, ctx: &ProgramContext) {
        if ctx.get_target_shape().is_none() {
            self.selection_anchor = None;
        } else if self.selection_anchor.is_none() {
            self.selection_anchor = Some(key.clone().into());
        }
    }

    /// The messages covered by the visual selection, or only the message under the cursor.
    fn selection_range(&self, key: MessageKey) -> EditRange<MessageCursor> {
        match &self.selection_anchor {
            Some(anchor) => {
                EditRange::inclusive(anchor.clone(), key.into(), TargetShape::LineWise)
            },
            None => self._range_to(key.into()),
        }
    }

    /// The rendered text of the visual selection, or of the message under the cursor.
    ///
    /// This is the text that a yank puts into a register, so that `:pipe` and `y` cannot drift
    /// apart. It is `None` when the scrollback holds no message to take.
    pub fn selected_text(&self, info: &RoomInfo, settings: &ApplicationSettings) -> Option<String> {
        let thread = self.get_thread(info)?;
        let key = self.cursor.to_key(&thread)?.clone();
        let range = self.selection_range(key);
        let msgs = self.messages(range, info).map(|(_, msg)| msg);

        Some(crate::message::yank::show_messages(msgs, info, settings))
    }

    /// Whether `key` is one of the messages covered by the visual selection.
    fn selection_contains(&self, key: &MessageKey, cursor_key: &MessageKey) -> bool {
        let Some(anchor) = self.selection_anchor.as_ref().and_then(|a| a.timestamp.as_ref()) else {
            return false;
        };

        let (start, end) =
            if anchor <= cursor_key { (anchor, cursor_key) } else { (cursor_key, anchor) };

        start <= key && key <= end
    }

    fn movement(
        &self,
        pos: MessageKey,
        movement: &MoveType,
        count: &Count,
        ctx: &ProgramContext,
        info: &RoomInfo,
    ) -> Option<MessageCursor> {
        let count = ctx.resolve(count);

        match movement {
            // These movements don't map meaningfully onto the scrollback history.
            MoveType::BufferByteOffset => None,
            MoveType::Column(_, _) => None,
            MoveType::ItemMatch => None,
            MoveType::LineColumnOffset => None,
            MoveType::LinePercent => None,
            MoveType::LinePos(_) => None,
            MoveType::SentenceBegin(_) => None,
            MoveType::ScreenFirstWord(_) => None,
            MoveType::ScreenLinePos(_) => None,
            MoveType::WordBegin(_, _) => None,
            MoveType::WordEnd(_, _) => None,

            MoveType::BufferLineOffset => None,
            MoveType::BufferLinePercent => None,
            MoveType::BufferPos(MovePosition::Beginning) => {
                let start = self.get_thread(info)?.first_key_value()?.0.clone();

                Some(start.into())
            },
            MoveType::BufferPos(MovePosition::Middle) => None,
            MoveType::BufferPos(MovePosition::End) => Some(MessageCursor::latest()),
            MoveType::FinalNonBlank(dir) |
            MoveType::FirstWord(dir) |
            MoveType::Line(dir) |
            MoveType::ScreenLine(dir) |
            MoveType::ParagraphBegin(dir) |
            MoveType::SectionBegin(dir) |
            MoveType::SectionEnd(dir) => {
                let thread = self.get_thread(info)?;

                match dir {
                    MoveDir1D::Previous => nth_before(pos, count, &thread).into(),
                    MoveDir1D::Next => nth_after(pos, count, &thread).into(),
                }
            },
            MoveType::ViewportPos(MovePosition::Beginning) => {
                return self.viewctx.corner.timestamp.as_ref().map(|k| k.clone().into());
            },
            MoveType::ViewportPos(MovePosition::Middle) => {
                // XXX: Need to calculate an accurate middle position.
                return None;
            },
            MoveType::ViewportPos(MovePosition::End) => {
                // XXX: Need store to calculate an accurate end position.
                return None;
            },

            _ => None,
        }
    }

    fn range_of_movement(
        &self,
        pos: MessageKey,
        movement: &MoveType,
        count: &Count,
        ctx: &ProgramContext,
        info: &RoomInfo,
    ) -> Option<EditRange<MessageCursor>> {
        let other = self.movement(pos.clone(), movement, count, ctx, info)?;

        Some(EditRange::inclusive(pos.into(), other, TargetShape::LineWise))
    }

    fn range(
        &self,
        pos: MessageKey,
        range: &RangeType,
        _: bool,
        count: &Count,
        ctx: &ProgramContext,
        info: &RoomInfo,
    ) -> Option<EditRange<MessageCursor>> {
        match range {
            RangeType::Bracketed(_, _) => None,
            RangeType::Item => None,
            RangeType::Quote(_) => None,
            RangeType::Word(_) => None,
            RangeType::XmlTag => None,

            RangeType::Buffer => {
                let thread = self.get_thread(info)?;
                let start = thread.first_key_value()?.0.clone();
                let end = thread.last_key_value()?.0.clone();

                Some(EditRange::inclusive(start.into(), end.into(), TargetShape::LineWise))
            },
            RangeType::Line | RangeType::Paragraph | RangeType::Sentence => {
                let thread = self.get_thread(info)?;
                let count = ctx.resolve(count);

                if count == 0 {
                    return None;
                }

                let mut end = &pos;

                for (i, (key, _)) in thread.range(&pos..).enumerate() {
                    if i >= count {
                        break;
                    }

                    end = key;
                }

                let end = end.clone().into();
                let start = pos.into();

                Some(EditRange::inclusive(start, end, TargetShape::LineWise))
            },

            _ => None,
        }
    }

    fn find_message_next(
        &self,
        start: MessageKey,
        needle: &Regex,
        mut count: usize,
        info: &RoomInfo,
    ) -> Option<MessageCursor> {
        let thread = self.get_thread(info)?;
        let mut mc = None;

        for (key, msg) in thread.range(&start..) {
            if count == 0 {
                break;
            }

            if key == &start {
                continue;
            }

            if needle.is_match(msg.event.body().as_ref()) {
                mc = MessageCursor::from(key.clone()).into();
                count -= 1;
            }
        }

        return mc;
    }

    fn find_message_prev(
        &self,
        end: MessageKey,
        needle: &Regex,
        mut count: usize,
        info: &RoomInfo,
    ) -> (Option<MessageCursor>, bool) {
        let mut mc = None;

        let Some(thread) = self.get_thread(info) else {
            return (None, false);
        };

        for (key, msg) in thread.range(..&end).rev() {
            if count == 0 {
                break;
            }

            if needle.is_match(msg.event.body().as_ref()) {
                mc = MessageCursor::from(key.clone()).into();
                count -= 1;
            }
        }

        return (mc, count > 0);
    }

    fn find_message(
        &self,
        key: MessageKey,
        dir: MoveDir1D,
        needle: &Regex,
        count: usize,
        info: &RoomInfo,
    ) -> (Option<MessageCursor>, bool) {
        match dir {
            MoveDir1D::Next => (self.find_message_next(key, needle, count, info), false),
            MoveDir1D::Previous => self.find_message_prev(key, needle, count, info),
        }
    }
}

impl WindowOps<IambInfo> for ScrollbackState {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        Scrollback::new(store).focus(focused).render(area, buf, self)
    }

    fn dup(&self, _: &mut ProgramStore) -> Self {
        ScrollbackState {
            room_id: self.room_id.clone(),
            id: self.id.clone(),
            thread: self.thread.clone(),
            cursor: self.cursor.clone(),
            viewctx: self.viewctx.clone(),
            jumped: self.jumped.clone(),
            show_full_on_redraw: false,
            pending_selection: self.pending_selection.clone(),
            selection_anchor: self.selection_anchor.clone(),
        }
    }

    fn close(&mut self, _: CloseFlags, _: &mut ProgramStore) -> bool {
        // XXX: what's the right closing behaviour for a room?
        // Should write send a message?
        true
    }

    fn write(
        &mut self,
        _: Option<&str>,
        flags: WriteFlags,
        _: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        if flags.contains(WriteFlags::FORCE) {
            Ok(None)
        } else {
            Err(EditError::ReadOnly.into())
        }
    }

    fn get_completions(&self) -> Option<CompletionList> {
        None
    }

    fn get_cursor_word(&self, _: &WordStyle) -> Option<String> {
        None
    }

    fn get_selected_word(&self) -> Option<String> {
        None
    }
}

impl EditorActions<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn edit(
        &mut self,
        operation: &EditAction,
        motion: &EditTarget,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let info = store.application.rooms.get_or_default(self.room_id.clone());
        let thread = self.get_thread(info).ok_or_else(no_msgs)?;
        let key = self.cursor.to_key(&thread).ok_or_else(no_msgs)?.clone();

        self.track_selection(&key, ctx);

        match operation {
            EditAction::Motion => {
                if motion.is_jumping() {
                    self.push_jump();
                }

                let pos = match motion {
                    EditTarget::CurrentPosition | EditTarget::Selection => {
                        return Ok(None);
                    },
                    EditTarget::Boundary(rt, inc, term, count) => {
                        self.range(key, rt, *inc, count, ctx, info).map(|r| {
                            match term {
                                MoveTerminus::Beginning => r.start,
                                MoveTerminus::End => r.end,
                            }
                        })
                    },
                    EditTarget::CharJump(mark) | EditTarget::LineJump(mark) => {
                        let mark = ctx.resolve(mark);
                        let cursor = store.cursors.get_mark(self.id.clone(), mark)?;

                        if let Some(mc) = MessageCursor::from_cursor(&cursor, &thread) {
                            Some(mc)
                        } else {
                            let msg = "Failed to restore mark";
                            let err = EditError::Failure(msg.into());

                            return Err(err);
                        }
                    },
                    EditTarget::Motion(mt, count) => self.movement(key, mt, count, ctx, info),
                    EditTarget::Range(_, _, _) => {
                        return Err(EditError::Failure("Cannot use ranges in a list".to_string()));
                    },
                    EditTarget::Search(SearchType::Char(_), _, _) => {
                        let msg = "Cannot perform character search in a list";
                        let err = EditError::Failure(msg.into());

                        return Err(err);
                    },
                    EditTarget::Search(SearchType::Regex, flip, count) => {
                        let count = ctx.resolve(count);

                        let dir = ctx.get_search_regex_dir();
                        let dir = flip.resolve(&dir);

                        let lsearch = store.registers.get_last_search().to_string();
                        let needle = Regex::new(lsearch.as_ref())?;

                        let (mc, needs_load) = self.find_message(key, dir, &needle, count, info);
                        if needs_load {
                            store.application.need_load.need_messages(self.room_id.clone());
                        }
                        mc
                    },
                    EditTarget::Search(SearchType::Word(_, _), _, _) => {
                        let msg = "Cannot perform word search in a list";
                        let err = EditError::Failure(msg.into());

                        return Err(err);
                    },

                    _ => {
                        let msg = format!("Unknown editing target: {motion:?}");
                        let err = EditError::Unimplemented(msg);

                        return Err(err);
                    },
                };

                if let Some(pos) = pos {
                    self.cursor = pos;
                }

                self.show_full_on_redraw = true;

                return Ok(None);
            },
            EditAction::Yank => {
                let range = match motion {
                    EditTarget::CurrentPosition | EditTarget::Selection => {
                        Some(self.selection_range(key))
                    },
                    EditTarget::Boundary(rt, inc, term, count) => {
                        self.range(key, rt, *inc, count, ctx, info).map(|r| {
                            self._range_to(match term {
                                MoveTerminus::Beginning => r.start,
                                MoveTerminus::End => r.end,
                            })
                        })
                    },
                    EditTarget::CharJump(mark) | EditTarget::LineJump(mark) => {
                        let mark = ctx.resolve(mark);
                        let cursor = store.cursors.get_mark(self.id.clone(), mark)?;

                        if let Some(c) = MessageCursor::from_cursor(&cursor, &thread) {
                            self._range_to(c).into()
                        } else {
                            let msg = "Failed to restore mark";
                            let err = EditError::Failure(msg.into());

                            return Err(err);
                        }
                    },
                    EditTarget::Motion(mt, count) => {
                        self.range_of_movement(key, mt, count, ctx, info)
                    },
                    EditTarget::Range(rt, inc, count) => {
                        self.range(key, rt, *inc, count, ctx, info)
                    },
                    EditTarget::Search(SearchType::Char(_), _, _) => {
                        let msg = "Cannot perform character search in a list";
                        let err = EditError::Failure(msg.into());

                        return Err(err);
                    },
                    EditTarget::Search(SearchType::Regex, flip, count) => {
                        let count = ctx.resolve(count);

                        let dir = ctx.get_search_regex_dir();
                        let dir = flip.resolve(&dir);

                        let lsearch = store.registers.get_last_search().to_string();
                        let needle = Regex::new(lsearch.as_ref())?;

                        let (mc, needs_load) = self.find_message(key, dir, &needle, count, info);
                        if needs_load {
                            store.application.need_load.need_messages(self.room_id.to_owned());
                        }

                        mc.map(|c| self._range_to(c))
                    },
                    EditTarget::Search(SearchType::Word(_, _), _, _) => {
                        let msg = "Cannot perform word search in a list";
                        let err = EditError::Failure(msg.into());

                        return Err(err);
                    },

                    _ => {
                        let msg = format!("Unknown motion: {motion:?}");
                        let err = EditError::Unimplemented(msg);

                        return Err(err);
                    },
                };

                if let Some(range) = range {
                    let msgs = self.messages(range, info).map(|(_, msg)| msg);
                    let text = crate::message::yank::show_messages(
                        msgs,
                        info,
                        &store.application.settings,
                    );

                    // A linewise register needs the closing newline, or a put runs the last
                    // message into whatever follows it.
                    let yanked = EditRope::from(text + "\n");

                    let cell = RegisterCell::new(TargetShape::LineWise, yanked);
                    let register = ctx.get_register().unwrap_or(Register::Unnamed);
                    let mut flags = RegisterPutFlags::NONE;

                    if ctx.get_register_append() {
                        flags |= RegisterPutFlags::APPEND;
                    }

                    store.registers.put(&register, cell, flags)?;
                }

                // A yank consumes the selection, the same way it does in vim.
                self.selection_anchor = None;

                return Ok(None);
            },

            // Everything else is a modifying action.
            EditAction::ChangeCase(_) => Err(EditError::ReadOnly),
            EditAction::ChangeNumber(_, _) => Err(EditError::ReadOnly),
            EditAction::Delete => Err(EditError::ReadOnly),
            EditAction::Format => Err(EditError::ReadOnly),
            EditAction::Indent(_) => Err(EditError::ReadOnly),
            EditAction::Join(_) => Err(EditError::ReadOnly),
            EditAction::Replace(_) => Err(EditError::ReadOnly),
        }
    }

    fn mark(
        &mut self,
        name: Mark,
        _: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let info = store.application.get_room_info(self.room_id.clone());
        let thread = self.get_thread(info).ok_or_else(no_msgs)?;
        let cursor = self.cursor.to_cursor(&thread).ok_or_else(no_msgs)?;
        store.cursors.set_mark(self.id.clone(), name, cursor);

        Ok(None)
    }

    fn complete(
        &mut self,
        _: &CompletionStyle,
        _: &CompletionType,
        _: &CompletionDisplay,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        Err(EditError::ReadOnly)
    }

    fn insert_text(
        &mut self,
        _: &InsertTextAction,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        Err(EditError::ReadOnly)
    }

    fn selection_command(
        &mut self,
        act: &SelectionAction,
        _: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        match act {
            // A message list has one selection, and it is linewise, so both of the swaps that vim
            // offers do the same thing here: move the cursor to the other end of the selection.
            SelectionAction::CursorSet(
                SelectionCursorChange::SwapAnchor | SelectionCursorChange::SwapSide,
            ) => {
                let Some(anchor) = self.selection_anchor.take() else {
                    return Ok(None);
                };

                let info = store.application.get_room_info(self.room_id.clone());
                let thread = self.get_thread(info).ok_or_else(no_msgs)?;
                let key = self.cursor.to_key(&thread).ok_or_else(no_msgs)?.clone();

                self.selection_anchor = Some(key.into());
                self.cursor = anchor;

                Ok(None)
            },
            _ => Err(EditError::Failure("Cannot perform selection actions in a list".into())),
        }
    }

    fn history_command(
        &mut self,
        act: &HistoryAction,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        match act {
            HistoryAction::Checkpoint => Ok(None),
            HistoryAction::Undo(_) => Err(EditError::Failure("Nothing to undo".into())),
            HistoryAction::Redo(_) => Err(EditError::Failure("Nothing to redo".into())),
        }
    }

    fn cursor_command(
        &mut self,
        act: &CursorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let info = store.application.get_room_info(self.room_id.clone());
        let thread = self.get_thread(info).ok_or_else(no_msgs)?;

        match act {
            CursorAction::Close(_) => Ok(None),
            CursorAction::Rotate(_, _) => Ok(None),
            CursorAction::Split(_) => Ok(None),

            CursorAction::Restore(_) => {
                let reg = ctx.get_register().unwrap_or(Register::UnnamedCursorGroup);

                // Get saved group.
                let ngroup = store.cursors.get_group(self.id.clone(), &reg)?;

                // Lists don't have groups; override current position.
                if self.jump_changed() {
                    self.push_jump();
                }

                if let Some(mc) = MessageCursor::from_cursor(ngroup.leader.cursor(), &thread) {
                    self.cursor = mc;

                    Ok(None)
                } else {
                    let msg = "Cannot restore position in message history";
                    let err = EditError::Failure(msg.into());

                    Err(err)
                }
            },
            CursorAction::Save(_) => {
                let reg = ctx.get_register().unwrap_or(Register::UnnamedCursorGroup);

                // Lists don't have groups; override any previously saved group.
                let cursor = self.cursor.to_cursor(&thread).ok_or_else(|| {
                    let msg = "Cannot save position in message history";
                    EditError::Failure(msg.into())
                })?;

                let group = CursorGroup {
                    leader: CursorState::Location(cursor),
                    members: vec![],
                };

                store.cursors.set_group(self.id.clone(), reg, group)?;

                Ok(None)
            },
            _ => Err(EditError::Unimplemented(format!("Unknown action: {act:?}"))),
        }
    }
}

impl Editable<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn editor_command(
        &mut self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        match act {
            EditorAction::Cursor(act) => self.cursor_command(act, ctx, store),
            EditorAction::Edit(ea, et) => self.edit(&ctx.resolve(ea), et, ctx, store),
            EditorAction::History(act) => self.history_command(act, ctx, store),
            EditorAction::InsertText(act) => self.insert_text(act, ctx, store),
            EditorAction::Mark(name) => self.mark(ctx.resolve(name), ctx, store),
            EditorAction::Selection(act) => self.selection_command(act, ctx, store),

            EditorAction::Complete(_, _, _) => {
                let msg = "Nothing to complete in message scrollback";
                let err = EditError::Failure(msg.into());

                Err(err)
            },

            _ => Err(EditError::Unimplemented(format!("Unknown action: {act:?}"))),
        }
    }
}

impl Jumpable<ProgramContext, IambInfo> for ScrollbackState {
    fn jump(
        &mut self,
        list: PositionList,
        dir: MoveDir1D,
        count: usize,
        _: &ProgramContext,
    ) -> UIResult<usize, IambInfo> {
        match list {
            PositionList::ChangeList => {
                let msg = "No changes to jump to within the list";
                let err = UIError::Failure(msg.into());

                Err(err)
            },
            PositionList::JumpList => {
                let (len, pos) = match dir {
                    MoveDir1D::Previous => {
                        if self.jumped.future_len() == 0 && self.jump_changed() {
                            // Push current position if this is the first jump backwards.
                            self.push_jump();
                        }

                        let plen = self.jumped.past_len();
                        let pos = self.jumped.prev(count);

                        (plen, pos)
                    },
                    MoveDir1D::Next => {
                        let flen = self.jumped.future_len();
                        let pos = self.jumped.next(count);

                        (flen, pos)
                    },
                };

                if len > 0 {
                    self.cursor = pos.clone();
                }

                Ok(count.saturating_sub(len))
            },
        }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(Action<IambInfo>, ProgramContext)>, IambInfo> {
        let info = store.application.get_room_info(self.room_id.clone());
        let thread = self.get_thread(info).ok_or_else(no_msgs)?;

        let Some(key) = self.cursor.to_key(&thread) else {
            let msg = "No message currently selected";
            let err = EditError::Failure(msg.into());
            return Err(err);
        };

        match act {
            PromptAction::Submit => {
                if self.thread.is_some() {
                    let msg =
                        "You are already in a thread. Use :reply to reply to a specific message.";
                    let err = EditError::Failure(msg.into());
                    Err(err)
                } else {
                    let root = key.1.clone();
                    let room_id = self.room_id.clone();
                    let id = IambId::Room(room_id, Some(root));
                    let open = WindowAction::Switch(OpenTarget::Application(id));
                    Ok(vec![(open.into(), ctx.clone())])
                }
            },
            PromptAction::Abort(..) => {
                let msg = "Cannot abort a message.";
                let err = EditError::Failure(msg.into());
                Err(err)
            },
            PromptAction::Recall(..) => {
                let msg = "Cannot recall previous messages.";
                let err = EditError::Failure(msg.into());
                Err(err)
            },
        }
    }
}

impl ScrollActions<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn dirscroll(
        &mut self,
        dir: MoveDir2D,
        size: ScrollSize,
        count: &Count,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let info = store.application.rooms.get_or_default(self.room_id.clone());
        let settings = &store.application.settings;
        let previews = &store.application.previews;
        let mut corner = self.viewctx.corner.clone();
        let thread = self.get_thread(info).ok_or_else(no_msgs)?;

        let last_key = if let Some(k) = thread.last_key_value() {
            k.0
        } else {
            return Ok(None);
        };

        let corner_key = corner.timestamp.as_ref().unwrap_or(last_key).clone();
        let cursor_key = self.cursor.timestamp.as_ref().unwrap_or(last_key);

        let count = ctx.resolve(count);
        let height = self.viewctx.get_height();
        let mut rows = match size {
            ScrollSize::Cell => count,
            ScrollSize::HalfPage => count.saturating_mul(height) / 2,
            ScrollSize::Page => count.saturating_mul(height),
        };

        match dir {
            MoveDir2D::Up => {
                let first_key = thread.first_key_value().map(|f| f.0.clone());

                for (key, item) in thread.range(..=&corner_key).rev() {
                    let sel = key == cursor_key;
                    let prev = prevmsg(key, &thread);
                    let txt = item.show(prev, sel, &self.viewctx, info, settings, previews);
                    let len = txt.height().max(1);
                    let max = len.saturating_sub(1);

                    if key != &corner_key {
                        corner.text_row = max;
                    }

                    corner.timestamp = key.clone().into();

                    if rows == 0 {
                        break;
                    } else if corner.text_row >= rows {
                        corner.text_row -= rows;
                        break;
                    } else if corner.timestamp == first_key {
                        corner.text_row = 0;
                        break;
                    }

                    rows -= corner.text_row + 1;
                }
            },
            MoveDir2D::Down => {
                let mut prev = prevmsg(&corner_key, &thread);

                for (key, item) in thread.range(&corner_key..) {
                    let sel = key == cursor_key;
                    let txt = item.show(prev, sel, &self.viewctx, info, settings, previews);
                    let len = txt.height().max(1);
                    let max = len.saturating_sub(1);

                    prev = Some(item);

                    if key != &corner_key {
                        corner.text_row = 0;
                    }

                    corner.timestamp = key.clone().into();

                    if rows == 0 {
                        break;
                    } else if key == last_key {
                        corner.text_row = corner.text_row.saturating_add(rows).min(max);
                        break;
                    } else if corner.text_row >= max {
                        rows -= 1;
                        continue;
                    } else if corner.text_row + rows <= max {
                        corner.text_row += rows;
                        break;
                    } else {
                        rows -= len - corner.text_row;
                        continue;
                    }
                }
            },
            MoveDir2D::Left | MoveDir2D::Right => {
                let msg = "Cannot scroll vertically in message scrollback";
                let err = EditError::Failure(msg.into());

                return Err(err);
            },
        }

        self.viewctx.corner = corner;
        self.shift_cursor(info, settings, previews);

        Ok(None)
    }

    fn cursorpos(
        &mut self,
        pos: MovePosition,
        axis: Axis,
        _: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        match axis {
            Axis::Horizontal => {
                let msg = "Cannot scroll vertically in message scrollback";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
            Axis::Vertical => {
                let info = store.application.rooms.get_or_default(self.room_id.clone());
                let settings = &store.application.settings;
                let previews = &store.application.previews;
                let thread = self.get_thread(info).ok_or_else(no_msgs)?;

                if let Some(key) = self.cursor.to_key(&thread).cloned() {
                    self.scrollview(key, pos, info, settings, previews);
                }

                Ok(None)
            },
        }
    }

    fn linepos(
        &mut self,
        _: MovePosition,
        _: &Count,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let msg = "Cannot scroll in message scrollback using line numbers";
        let err = EditError::Failure(msg.into());

        Err(err)
    }
}

impl Scrollable<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        match style {
            ScrollStyle::Direction2D(dir, size, count) => {
                return self.dirscroll(*dir, *size, count, ctx, store);
            },
            ScrollStyle::CursorPos(pos, axis) => {
                return self.cursorpos(*pos, *axis, ctx, store);
            },
            ScrollStyle::LinePos(pos, count) => {
                return self.linepos(*pos, count, ctx, store);
            },
        }
    }
}

impl Searchable<ProgramContext, ProgramStore, IambInfo> for ScrollbackState {
    fn search(
        &mut self,
        dir: MoveDirMod,
        count: Count,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> UIResult<EditInfo, IambInfo> {
        let search = EditTarget::Search(SearchType::Regex, dir, count);

        Ok(self.edit(&EditAction::Motion, &search, ctx, store)?)
    }
}

impl TerminalCursor for ScrollbackState {
    fn get_term_cursor(&self) -> Option<(u16, u16)> {
        None
    }
}

fn render_jump_to_recent(area: Rect, buf: &mut Buffer, focused: bool) -> Rect {
    if area.height <= 5 || area.width <= 20 {
        return area;
    }

    let top = Rect::new(area.x, area.y, area.width, area.height - 1);
    let bar = Rect::new(area.x, area.y + top.height, area.width, 1);
    let msg = vec![
        Span::raw("Use "),
        Span::styled("G", Style::default().add_modifier(StyleModifier::BOLD)),
        Span::raw(if focused { "" } else { " in scrollback" }),
        Span::raw(" to jump to latest message"),
    ];

    Paragraph::new(Line::from(msg))
        .alignment(Alignment::Center)
        .render(bar, buf);

    return top;
}

/// Tell the user that the scrollback goes on above the pane.
///
/// This is the counterpart of [render_jump_to_recent], which says the same about the bottom.
fn render_more_above(bar: Rect, buf: &mut Buffer, focused: bool) {
    let msg = vec![
        Span::raw("Use "),
        Span::styled("k", Style::default().add_modifier(StyleModifier::BOLD)),
        Span::raw(if focused { "" } else { " in scrollback" }),
        Span::raw(" to scroll up to earlier messages"),
    ];

    Paragraph::new(Line::from(msg))
        .alignment(Alignment::Center)
        .render(bar, buf);
}

pub struct Scrollback<'a> {
    room_focused: bool,
    focused: bool,
    store: &'a mut ProgramStore,
}

impl<'a> Scrollback<'a> {
    pub fn new(store: &'a mut ProgramStore) -> Self {
        Scrollback { room_focused: false, focused: false, store }
    }

    /// Indicate whether the room window is currently focused, regardless of whether the scrollback
    /// also is.
    pub fn room_focus(mut self, focused: bool) -> Self {
        self.room_focused = focused;
        self
    }

    /// Indicate whether the scrollback is currently focused.
    pub fn focus(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
}

impl StatefulWidget for Scrollback<'_> {
    type State = ScrollbackState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let info = self.store.application.rooms.get_or_default(state.room_id.clone());
        state.take_pending_selection(info);

        let settings = &self.store.application.settings;
        let area = if state.cursor.timestamp.is_some() {
            render_jump_to_recent(area, buf, self.focused)
        } else {
            info.render_typing(area, buf, &self.store.application.settings)
        };

        state.set_term_info(area);

        let height = state.viewctx.get_height();

        if height == 0 {
            return;
        }

        let Some(thread) = state.get_thread(info) else {
            return;
        };

        if state.cursor.timestamp < state.viewctx.corner.timestamp {
            state.viewctx.corner = state.cursor.clone();
        }

        let cursor = &state.cursor;
        let cursor_key = if let Some(k) = cursor.to_key(&thread) {
            k
        } else {
            if state.need_more_messages(info) {
                self.store.application.need_load.need_messages(state.room_id.to_owned());
            }
            return;
        };

        let corner = &state.viewctx.corner;
        let corner_key = if let Some(k) = &corner.timestamp {
            k.clone()
        } else {
            nth_key_before(cursor_key.clone(), height, &thread)
        };

        let foc = self.focused || cursor.timestamp.is_some();
        let full = std::mem::take(&mut state.show_full_on_redraw) || cursor.timestamp.is_none();
        let mut lines = vec![];
        let mut sawit = false;
        let mut prev = prevmsg(&corner_key, &thread);

        // load image previews
        for (_, item) in thread.range(&corner_key..).rev() {
            if let Some(source) = &item.image_preview {
                self.store
                    .application
                    .previews
                    .load(source, &self.store.application.worker);
            }
            let reply = item
                .reply_to()
                .or_else(|| item.thread_root())
                .and_then(|e| info.get_event(&e))
                .and_then(|msg| msg.image_preview.as_ref());
            if let Some(source) = reply {
                self.store
                    .application
                    .previews
                    .load(source, &self.store.application.worker);
            }
        }

        let previews = &self.store.application.previews;
        for (key, item) in thread.range(&corner_key..) {
            let sel = key == cursor_key;

            // The cursor drives the scrolling, but every message in a visual selection is drawn
            // as picked out, so that the user can see what a yank would take.
            let picked = sel || state.selection_contains(key, cursor_key);
            let (txt, [mut msg_preview, mut reply_preview]) = item.show_with_preview(
                prev,
                foc && picked,
                &state.viewctx,
                info,
                settings,
                previews,
            );

            let incomplete_ok = !full || !sel;

            for (row, line) in txt.lines.into_iter().enumerate() {
                if sawit && lines.len() >= height && incomplete_ok {
                    // Check whether we've seen the first line of the
                    // selected message and can fill the screen.
                    break;
                }

                if key == &corner_key && row < corner.text_row {
                    // Skip rows above the viewport corner.
                    continue;
                }

                // Only take the preview into the matching row number.
                // `reply` and `msg` previews are on rows,
                // so an `or` works to pick the one that matches (if any)
                let line_preview = match msg_preview {
                    Some((_, _, y)) if y as usize == row => msg_preview.take(),
                    _ => None,
                }
                .or(match reply_preview {
                    Some((_, _, y)) if y as usize == row => reply_preview.take(),
                    _ => None,
                });

                lines.push((key, row, line, line_preview));
                sawit |= sel;
            }

            prev = Some(item);
        }

        if lines.len() > height {
            let n = lines.len() - height;
            let _ = lines.drain(..n);
        }

        // Whether the scrollback goes on above the pane.
        //
        // The check reads the lines that the pane takes rather than the viewport corner, because
        // the corner is set from them below and is still the corner of the frame before this one.
        let more_above = match (lines.first(), thread.first_key_value()) {
            (Some((key, row, _, _)), Some((first, _))) => *key != first || *row > 0,
            _ => false,
        };

        // The hint takes the top row from the message it interrupts, so that the newest messages
        // keep the bottom of the pane. That message continues above, so the hint stays true.
        let hint_above = more_above && area.height > 5 && area.width > 20;

        // The corner must keep the row that the hint covers. A corner that skips that row makes the
        // next render start one row lower, and the pane then walks the messages up one row per
        // frame until it holds almost nothing.
        if let Some(((ts, event_id), row, _, _)) = lines.first() {
            state.viewctx.corner.timestamp = Some((*ts, event_id.clone()));
            state.viewctx.corner.text_row = *row;
        }

        if hint_above {
            let _ = lines.remove(0);
        }

        let mut y = area.top();
        let x = area.left();

        if hint_above {
            render_more_above(Rect::new(x, y, area.width, 1), buf, self.focused);
            y += 1;
        }

        let mut image_previews = vec![];
        for ((_, _), _, txt, line_preview) in lines.into_iter() {
            let _ = buf.set_line(x, y, &txt, area.width);
            if let Some((backend, msg_x, _)) = line_preview {
                image_previews.push((x + msg_x, y, backend));
            }

            y += 1;
        }
        // Render image previews after all text lines have been drawn, as the render might draw below the current
        // line.
        for (x, y, backend) in image_previews {
            let image_widget = Image::new(backend);
            let mut rect = backend.area();
            rect.x = x;
            rect.y = y;
            // Don't render outside of scrollback area
            if rect.bottom() <= area.bottom() && rect.right() <= area.right() {
                image_widget.render(rect, buf);
            }
        }

        if self.room_focused &&
            settings.tunables.read_receipt_send &&
            !settings.tunables.read_receipt_manual &&
            state.cursor.timestamp.is_none()
        {
            // If the cursor is at the last message, then update the read marker. When
            // `read_receipt_manual` is set, viewing never does this, and the marker only moves
            // when the user runs `:read`.
            //
            // The replies alone carry the marker. The message a thread is about belongs to the
            // main scrollback, which counts it already, so a thread with no replies must not move
            // a receipt.
            if let Some((replies, (k, _))) =
                thread.replies().zip(thread.replies().and_then(|r| r.last_key_value()))
            {
                info.set_receipt(replies.1.clone(), settings.profile.user_id.clone(), k.1.clone());
            }
        }

        // Check whether we should load older messages for this room.
        if state.need_more_messages(info) {
            // If the top of the screen is the older message, load more.
            self.store.application.need_load.need_messages(state.room_id.to_owned());
        }

        info.draw_last = self.store.application.draw_curr;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        base::{EventLocation, Need},
        message::MessageTimeStamp,
        tests::*,
    };
    use matrix_sdk::ruma::{events::room::message::RoomMessageEventContent, server_name, UInt};

    /// Give the room a thread that MSG2 started, with `replies` replies in it.
    ///
    /// MSG2 stays in the main scrollback and out of the thread's own map, the way the homeserver
    /// gives a thread to us.
    fn mock_thread(info: &mut RoomInfo, replies: usize) -> Vec<MessageKey> {
        let mut keys = vec![];

        for i in 0..replies {
            let event_id = EventId::new_v1(server_name!("example.com"));
            let ts = UInt::new(9 + i as u64).unwrap();
            let key: MessageKey = (MessageTimeStamp::OriginServer(ts), event_id.clone());

            let location = EventLocation::Message(Some(MSG2_EVID.clone()), key.clone());
            info.keys.insert(event_id, location);

            let content = RoomMessageEventContent::text_plain(format!("reply {i}"));
            let msg = mock_room1_message(content, TEST_USER1.clone(), key.clone());
            info.get_thread_mut(Some(MSG2_EVID.clone())).insert(key.clone(), msg);

            keys.push(key);
        }

        keys
    }

    #[tokio::test]
    async fn test_thread_puts_the_root_before_the_replies() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let info = store.application.rooms.get_or_default(room_id.clone());
        let replies = mock_thread(info, 2);

        let scrollback = ScrollbackState::new(room_id, Some(MSG2_EVID.clone()));
        let thread = scrollback.get_thread(info).unwrap();
        let keys: Vec<_> = thread.range(..).map(|(key, _)| key.clone()).collect();

        assert_eq!(keys, [vec![MSG2_KEY.clone()], replies].concat());
    }

    #[tokio::test]
    async fn test_thread_cursor_can_select_the_root() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let info = store.application.rooms.get_or_default(room_id.clone());
        mock_thread(info, 2);

        let mut scrollback = ScrollbackState::new(room_id, Some(MSG2_EVID.clone()));

        assert!(scrollback.goto_event(&MSG2_EVID, info));
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());
    }

    #[tokio::test]
    async fn test_thread_without_replies_still_shows_the_root() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let info = store.application.rooms.get_or_default(room_id.clone());

        let scrollback = ScrollbackState::new(room_id, Some(MSG2_EVID.clone()));
        let thread = scrollback.get_thread(info).unwrap();

        assert_eq!(thread.first_key_value().map(|(key, _)| key), Some(&*MSG2_KEY));
        assert_eq!(thread.last_key_value().map(|(key, _)| key), Some(&*MSG2_KEY));

        // An unscrolled cursor lands on the root, so a command acts on the message the thread is
        // about rather than on nothing.
        assert_eq!(MessageCursor::latest().to_key(&thread), Some(&*MSG2_KEY));
    }

    #[tokio::test]
    async fn test_thread_root_scrolls_out_of_view() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let info = store.application.rooms.get_or_default(room_id.clone());
        mock_thread(info, 4);

        let mut scrollback = ScrollbackState::new(room_id, Some(MSG2_EVID.clone()));
        let ctx = ProgramContext::default();

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        // The thread is a date divider, the root and four replies, so it is taller than the pane.
        let area = Rect::new(0, 0, 60, 3);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);

        // The newest replies fill the pane, and the root is above it.
        assert_ne!(scrollback.viewctx.corner.timestamp, Some(MSG2_KEY.clone()));

        // Scrolling up brings the root back, and it is the top of the thread.
        for _ in 0..4 {
            scrollback
                .dirscroll(MoveDir2D::Up, ScrollSize::Page, &1.into(), &ctx, &mut store)
                .unwrap();
        }

        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 0));
    }

    #[tokio::test]
    async fn test_goto_event_selects_a_loaded_message() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let info = store.application.rooms.get_or_default(room_id);

        assert_eq!(scrollback.cursor, MessageCursor::latest());
        assert!(scrollback.goto_event(&MSG2_EVID, info));
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());
    }

    #[tokio::test]
    async fn test_goto_event_ignores_an_unloaded_message() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let info = store.application.rooms.get_or_default(room_id);
        let unloaded = EventId::new_v1(server_name!("example.com"));

        assert!(!scrollback.goto_event(&unloaded, info));
        assert_eq!(scrollback.cursor, MessageCursor::latest());
    }

    #[tokio::test]
    async fn test_pending_selection_waits_for_the_message() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let unloaded = EventId::new_v1(server_name!("example.com"));

        // Nothing to select yet, so the cursor stays where it is and the request is kept.
        scrollback.select_when_loaded(unloaded);
        let info = store.application.rooms.get_or_default(room_id.clone());
        scrollback.take_pending_selection(info);
        assert_eq!(scrollback.cursor, MessageCursor::latest());
        assert!(scrollback.pending_selection.is_some());

        // Once the message is there, it gets selected and the request is done with.
        scrollback.select_when_loaded(MSG2_EVID.clone());
        let info = store.application.rooms.get_or_default(room_id);
        scrollback.take_pending_selection(info);
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());
        assert!(scrollback.pending_selection.is_none());
    }

    #[tokio::test]
    async fn test_pending_selection_yields_to_the_user() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let unloaded = EventId::new_v1(server_name!("example.com"));

        scrollback.select_when_loaded(unloaded);

        // The user selected something themselves while the message was still loading, so the late
        // arrival must not move them.
        scrollback.goto_message(MSG4_KEY.clone());

        let info = store.application.rooms.get_or_default(room_id);
        scrollback.take_pending_selection(info);
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());
        assert!(scrollback.pending_selection.is_none());
    }

    #[tokio::test]
    async fn test_search_messages() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let ctx = ProgramContext::default();

        let next = MoveDirMod::Exact(MoveDir1D::Next);
        let prev = MoveDirMod::Exact(MoveDir1D::Previous);

        // Search through the messages:
        //
        // MSG2: "helium"
        // MSG3: "this\nis\na\nmultiline\nmessage"
        // MSG4: "help"
        // MSG5: "character"
        // MSG1: "writhe"
        store.registers.set_last_search("he");

        assert_eq!(scrollback.cursor, MessageCursor::latest());

        // Search backwards to MSG4.
        scrollback.search(prev, 1.into(), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());

        // Search backwards to MSG2.
        scrollback.search(prev, 1.into(), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());
        assert_eq!(
            std::mem::take(&mut store.application.need_load)
                .into_iter()
                .collect::<Vec<(OwnedRoomId, Need)>>()
                .is_empty(),
            true,
        );

        // Can't go any further; need_load now contains the room ID.
        scrollback.search(prev, 1.into(), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());
        assert_eq!(
            std::mem::take(&mut store.application.need_load)
                .into_iter()
                .collect::<Vec<(OwnedRoomId, Need)>>(),
            vec![(room_id.clone(), Need { messages: Some(Vec::new()), members: false })]
        );

        // Search forward twice to MSG1.
        scrollback.search(next, 2.into(), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG1_KEY.clone().into());

        // Can't go any further.
        scrollback.search(next, 2.into(), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG1_KEY.clone().into());
    }

    #[tokio::test]
    async fn test_movement() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        let next = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Next), n.into());

        assert_eq!(scrollback.cursor, MessageCursor::latest());

        scrollback.edit(&EditAction::Motion, &prev(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG5_KEY.clone().into());

        scrollback.edit(&EditAction::Motion, &prev(2), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG3_KEY.clone().into());

        scrollback.edit(&EditAction::Motion, &prev(5), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());

        scrollback.edit(&EditAction::Motion, &next(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG3_KEY.clone().into());

        scrollback.edit(&EditAction::Motion, &next(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());

        scrollback.edit(&EditAction::Motion, &next(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG5_KEY.clone().into());

        // And one more becomes "latest" cursor:
        scrollback.edit(&EditAction::Motion, &next(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MessageCursor::latest());
    }

    /// Read back what a yank put into the unnamed register.
    fn yanked(store: &mut ProgramStore) -> String {
        store.registers.get(&Register::Unnamed).unwrap().value.to_string()
    }

    #[tokio::test]
    async fn test_yank_takes_the_selected_message() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        // MSG2 is the "helium" message.
        scrollback.edit(&EditAction::Motion, &prev(1), &ctx, &mut store).unwrap();
        scrollback.edit(&EditAction::Motion, &prev(3), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG2_KEY.clone().into());

        scrollback
            .edit(&EditAction::Yank, &EditTarget::CurrentPosition, &ctx, &mut store)
            .unwrap();

        let text = yanked(&mut store);
        assert!(text.ends_with("@user2:example.com: helium\n"));
        assert_eq!(text.lines().count(), 1);
    }

    #[tokio::test]
    async fn test_yank_keeps_every_line_of_a_multiline_message() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        // MSG3 is the multiline message.
        scrollback.edit(&EditAction::Motion, &prev(3), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG3_KEY.clone().into());

        scrollback
            .edit(&EditAction::Yank, &EditTarget::CurrentPosition, &ctx, &mut store)
            .unwrap();

        let text = yanked(&mut store);
        assert!(text.ends_with("@user2:example.com:\nthis\nis\na\nmultiline\nmessage\n"));
    }

    /// The context that visual mode gives to the scrollback: a linewise target shape.
    fn visual_ctx() -> ProgramContext {
        modalkit::editing::context::EditContextBuilder::default()
            .target_shape(Some(TargetShape::LineWise))
            .build()
    }

    #[tokio::test]
    async fn test_visual_selection_yanks_every_message_in_the_range() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();
        let visual = visual_ctx();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        // Land on MSG5, the newest message.
        scrollback.edit(&EditAction::Motion, &prev(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG5_KEY.clone().into());

        // "V" starts the selection where the cursor already is.
        scrollback
            .edit(&EditAction::Motion, &EditTarget::CurrentPosition, &visual, &mut store)
            .unwrap();
        assert_eq!(scrollback.selection_anchor, Some(MSG5_KEY.clone().into()));

        // Two motions upwards grow the selection to cover MSG3, MSG4 and MSG5.
        scrollback.edit(&EditAction::Motion, &prev(2), &visual, &mut store).unwrap();
        assert_eq!(scrollback.cursor, MSG3_KEY.clone().into());

        scrollback
            .edit(&EditAction::Yank, &EditTarget::Selection, &visual, &mut store)
            .unwrap();

        let text = yanked(&mut store);
        // Blank lines are the separator between messages, so they are not body lines.
        let bodies: Vec<_> =
            text.lines().filter(|l| !l.starts_with('[') && !l.is_empty()).collect();
        assert_eq!(bodies, vec!["this", "is", "a", "multiline", "message"]);
        assert_eq!(text.matches('[').count(), 3);
        assert!(text.ends_with("@user2:example.com: character\n"));

        // The yank consumes the selection.
        assert_eq!(scrollback.selection_anchor, None);
    }

    #[tokio::test]
    async fn test_leaving_visual_mode_drops_the_selection() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();
        let visual = visual_ctx();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        scrollback.edit(&EditAction::Motion, &prev(1), &visual, &mut store).unwrap();
        assert!(scrollback.selection_anchor.is_some());

        // No target shape means the user is back in normal mode.
        scrollback.edit(&EditAction::Motion, &prev(1), &ctx, &mut store).unwrap();
        assert_eq!(scrollback.selection_anchor, None);
    }

    #[tokio::test]
    async fn test_swapping_the_anchor_moves_the_cursor_to_the_other_end() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();
        let visual = visual_ctx();

        let prev = |n: usize| EditTarget::Motion(MoveType::Line(MoveDir1D::Previous), n.into());

        scrollback.edit(&EditAction::Motion, &prev(1), &ctx, &mut store).unwrap();
        scrollback
            .edit(&EditAction::Motion, &EditTarget::CurrentPosition, &visual, &mut store)
            .unwrap();
        scrollback.edit(&EditAction::Motion, &prev(2), &visual, &mut store).unwrap();

        let swap = SelectionAction::CursorSet(SelectionCursorChange::SwapAnchor);
        scrollback.selection_command(&swap, &visual, &mut store).unwrap();

        assert_eq!(scrollback.cursor, MSG5_KEY.clone().into());
        assert_eq!(scrollback.selection_anchor, Some(MSG3_KEY.clone().into()));
    }

    #[tokio::test]
    async fn test_dirscroll() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();

        let prev = MoveDir2D::Up;
        let next = MoveDir2D::Down;

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        assert_eq!(scrollback.cursor, MessageCursor::latest());
        assert_eq!(scrollback.viewctx.dimensions, (0, 0));
        assert_eq!(scrollback.viewctx.corner, MessageCursor::latest());

        // Set a terminal width of 60, and height of 4, rendering in scrollback as:
        //
        //       |------------------------------------------------------------|
        // MSG2: |                Wednesday, December 31 1969                 |
        //       |           @user2:example.com  helium                       |
        // MSG3: |           @user2:example.com  this                         |
        //       |                               is                           |
        //       |                               a                            |
        //       |                               multiline                    |
        //       |                               message                      |
        // MSG4: |           @user1:example.com  help                         |
        // MSG5: |           @user2:example.com  character                    |
        // MSG1: |                   XXXday, Month NN 20XX                    |
        //       |           @user1:example.com  writhe                       |
        //       |------------------------------------------------------------|
        let area = Rect::new(0, 0, 60, 5);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);

        assert_eq!(scrollback.cursor, MessageCursor::latest());
        assert_eq!(scrollback.viewctx.dimensions, (60, 4));
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG4_KEY.clone(), 0));

        // Scroll up a line at a time until we hit the first message.
        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 4));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 3));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 2));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 1));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 0));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 1));

        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 0));

        // Cannot scroll any further.
        scrollback
            .dirscroll(prev, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 0));

        // Now scroll back down one line at a time.
        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 1));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 0));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 1));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 2));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 3));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 4));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG4_KEY.clone(), 0));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG5_KEY.clone(), 0));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG1_KEY.clone(), 0));

        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG1_KEY.clone(), 1));

        // Cannot scroll down any further.
        scrollback
            .dirscroll(next, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG1_KEY.clone(), 1));

        // Scroll up two Pages (eight lines).
        scrollback
            .dirscroll(prev, ScrollSize::Page, &2.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 0));

        // Scroll down two HalfPages (four lines).
        scrollback
            .dirscroll(next, ScrollSize::HalfPage, &2.into(), &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 4));
    }

    /// Everything the pane drew, as one string.
    fn drawn(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    #[tokio::test]
    async fn test_the_pane_says_when_messages_are_above_it() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        // The room draws eleven lines, so some of them are above a pane of this height.
        let area = Rect::new(0, 0, 60, 8);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert!(drawn(&buffer).contains("to scroll up to earlier messages"));

        // A pane that holds the whole room has nothing above it, so it says nothing.
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG2_KEY.clone(), 0));
        assert!(!drawn(&buffer).contains("to scroll up to earlier messages"));
    }

    /// A redraw that gets no new message must not move the pane.
    ///
    /// The hint about the messages above the pane covers the top row. If the corner moves past that
    /// row, then each redraw drops one more message row and the pane empties from the bottom.
    #[tokio::test]
    async fn test_a_second_render_of_the_same_state_draws_the_same_pane() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        // The room draws more lines than this pane holds, so the pane shows the hint.
        let area = Rect::new(0, 0, 60, 8);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);

        // Scroll away from the newest message. The pane then keeps its own corner, instead of
        // taking a fresh one from the cursor on every render.
        let ctx = ProgramContext::default();
        scrollback
            .dirscroll(MoveDir2D::Up, ScrollSize::Cell, &1.into(), &ctx, &mut store)
            .unwrap();
        assert!(scrollback.cursor.timestamp.is_some());

        let mut first = Buffer::empty(area);
        scrollback.draw(area, &mut first, true, &mut store);
        let corner = scrollback.viewctx.corner.clone();
        assert!(drawn(&first).contains("to scroll up to earlier messages"));

        let mut second = Buffer::empty(area);
        scrollback.draw(area, &mut second, true, &mut store);

        assert_eq!(scrollback.viewctx.corner, corner);
        assert_eq!(drawn(&second), drawn(&first));
    }

    /// A jump must put the message it selects on the screen.
    ///
    /// The render stops when it holds the first row of the selected message and a full pane. That
    /// row is often a date or a sender, so a jump that does not ask for the whole message can
    /// leave the user with no part of what they selected. Every jump goes through
    /// [ScrollbackState::goto_message], so `:context`, a clicked notification and a `:search`
    /// result all need this.
    #[tokio::test]
    async fn test_a_jump_brings_the_selected_message_into_view() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let ctx = ProgramContext::default();

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        // The room draws more lines than this pane holds, so a jump has somewhere to hide.
        let area = Rect::new(0, 0, 60, 6);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);

        // Scroll to the top, so that the newest message is below the pane.
        for _ in 0..10 {
            scrollback
                .dirscroll(MoveDir2D::Up, ScrollSize::Cell, &1.into(), &ctx, &mut store)
                .unwrap();
        }
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert!(!drawn(&buffer).contains("writhe"));

        // A jump down to it shows it, and not only the date above it.
        let info = store.application.rooms.get_or_default(room_id.clone());
        assert!(scrollback.goto_event(&MSG1_EVID, info));
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert!(drawn(&buffer).contains("writhe"), "got: {}", drawn(&buffer));

        // The pane must still hold still afterwards, as it does after any other render.
        let corner = scrollback.viewctx.corner.clone();
        let mut again = Buffer::empty(area);
        scrollback.draw(area, &mut again, true, &mut store);
        assert_eq!(scrollback.viewctx.corner, corner);
        assert_eq!(drawn(&again), drawn(&buffer));

        // A jump back up shows the older message as well.
        let info = store.application.rooms.get_or_default(room_id.clone());
        assert!(scrollback.goto_event(&MSG2_EVID, info));
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert!(drawn(&buffer).contains("helium"), "got: {}", drawn(&buffer));
    }

    /// The same, for a message that arrives only after the jump asked for it.
    ///
    /// A notification and a search result often name a message that the scrollback has not loaded.
    #[tokio::test]
    async fn test_a_jump_to_a_late_message_brings_it_into_view() {
        let room_id = TEST_ROOM1_ID.clone();
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(room_id.clone(), None);
        let ctx = ProgramContext::default();

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        let area = Rect::new(0, 0, 60, 6);
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);

        for _ in 0..10 {
            scrollback
                .dirscroll(MoveDir2D::Up, ScrollSize::Cell, &1.into(), &ctx, &mut store)
                .unwrap();
        }

        // The render takes the waiting selection, so the message shows on the same frame.
        scrollback.select_when_loaded(MSG1_EVID.clone());
        let mut buffer = Buffer::empty(area);
        scrollback.draw(area, &mut buffer, true, &mut store);
        assert!(scrollback.pending_selection.is_none());
        assert!(drawn(&buffer).contains("writhe"), "got: {}", drawn(&buffer));
    }

    #[tokio::test]
    async fn test_cursorpos() {
        let mut store = mock_store().await;
        let mut scrollback = ScrollbackState::new(TEST_ROOM1_ID.clone(), None);
        let ctx = ProgramContext::default();

        // Skip rendering typing notices.
        store.application.settings.tunables.typing_notice_display = false;

        // Set a terminal width of 60, and height of 3, rendering in scrollback as:
        //
        //       |------------------------------------------------------------|
        // MSG2: |                Wednesday, December 31 1969                 |
        //       |           @user2:example.com  helium                       |
        // MSG3: |           @user2:example.com  this                         |
        //       |                               is                           |
        //       |                               a                            |
        //       |                               multiline                    |
        //       |                               message                      |
        // MSG4: |           @user1:example.com  help                         |
        // MSG5: |           @user2:example.com  character                    |
        // MSG1: |                   XXXday, Month NN 20XX                    |
        //       |           @user1:example.com  writhe                       |
        //       |------------------------------------------------------------|
        let area = Rect::new(0, 0, 60, 3);
        let mut buffer = Buffer::empty(area);
        scrollback.cursor = MSG4_KEY.clone().into();
        scrollback.draw(area, &mut buffer, true, &mut store);

        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());
        assert_eq!(scrollback.viewctx.dimensions, (60, 3));
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 3));

        // Scroll so that the cursor is at the top of the screen.
        scrollback
            .cursorpos(MovePosition::Beginning, Axis::Vertical, &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG4_KEY.clone(), 0));

        // Scroll so that the cursor is at the bottom of the screen.
        scrollback
            .cursorpos(MovePosition::End, Axis::Vertical, &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 3));

        // Scroll so that the cursor is in the middle of the screen.
        scrollback
            .cursorpos(MovePosition::Middle, Axis::Vertical, &ctx, &mut store)
            .unwrap();
        assert_eq!(scrollback.cursor, MSG4_KEY.clone().into());
        assert_eq!(scrollback.viewctx.corner, MessageCursor::new(MSG3_KEY.clone(), 4));
    }
}
