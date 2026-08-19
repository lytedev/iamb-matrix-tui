//! # Windows for the User Interface
//!
//! This module contains the logic for rendering windows, and handling UI actions that get
//! delegated to individual windows/UI elements (e.g., typing text or selecting a list item).
//!
//! Additionally, some of the iamb commands delegate behaviour to the current UI element. For
//! example, [sending messages][crate::base::SendAction] delegate to the [room window][RoomState],
//! where we have the message bar and room ID easily accessible and resettable.
use std::cmp::{Ord, Ordering, PartialOrd};
use std::fmt::{self, Display};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::{
    encryption::verification::{format_emojis, SasVerification},
    room::{Room as MatrixRoom, RoomMember},
    ruma::{
        events::room::member::MembershipState,
        events::tag::{TagName, Tags},
        OwnedEventId,
        OwnedRoomAliasId,
        OwnedRoomId,
        RoomAliasId,
        RoomId,
    },
    RoomState as MatrixRoomState,
};

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
    widgets::StatefulWidget,
};

use modalkit::{
    actions::{
        Action,
        Editable,
        EditorAction,
        Jumpable,
        PromptAction,
        Promptable,
        Scrollable,
        WindowAction,
    },
    editing::completion::CompletionList,
    errors::{EditError, EditResult, UIError},
    prelude::*,
};

use modalkit_ratatui::{
    list::{List, ListCursor, ListItem, ListState},
    TermOffset,
    TerminalCursor,
    Window,
    WindowOps,
};

use crate::snooze::{describe, SnoozeKey, WakeTime};
use crate::base::{
    ChatStore,
    IambBufferId,
    IambError,
    IambId,
    IambInfo,
    IambResult,
    ListCounts,
    MessageAction,
    ProgramAction,
    ProgramContext,
    ProgramStore,
    RoomAction,
    SendAction,
    SortColumn,
    SortFieldRoom,
    SortFieldUser,
    SortOrder,
    SpaceAction,
    ThreadSummary,
    UnreadInfo,
};
use crate::windows::room::room_command;

use self::{
    palette::CommandPaletteState,
    room::RoomState,
    search::{Found, MessageSearchState},
    switcher::QuickSwitcherState,
    welcome::WelcomeState,
};
use crate::message::MessageTimeStamp;
use feruca::Collator;

pub mod filtered;
pub mod palette;
pub mod room;
pub mod search;
pub mod switcher;
pub mod welcome;

type MatrixRoomInfo = Arc<(MatrixRoom, Option<Tags>)>;

const MEMBER_FETCH_DEBOUNCE: Duration = Duration::from_secs(5);

#[inline]
fn bold_style() -> Style {
    Style::default().add_modifier(StyleModifier::BOLD)
}

#[inline]
fn bold_span(s: &str) -> Span<'_> {
    Span::styled(s, bold_style())
}

#[inline]
fn bold_spans(s: &str) -> Line<'_> {
    bold_span(s).into()
}

#[inline]
fn selected_style(selected: bool) -> Style {
    if selected {
        Style::default().add_modifier(StyleModifier::REVERSED)
    } else {
        Style::default()
    }
}

#[inline]
fn selected_span(s: &str, selected: bool) -> Span<'_> {
    Span::styled(s, selected_style(selected))
}

#[inline]
fn selected_text(s: &str, selected: bool) -> Text<'_> {
    Text::from(selected_span(s, selected))
}

fn name_and_labels(name: &str, unread: bool, style: Style) -> (Span<'_>, Vec<Vec<Span<'_>>>) {
    let name_style = if unread {
        style.add_modifier(StyleModifier::BOLD)
    } else {
        style
    };

    let name = Span::styled(name, name_style);
    let labels = if unread {
        vec![vec![Span::styled("Unread", style)]]
    } else {
        vec![]
    };

    (name, labels)
}

/// Sort `Some` to be less than `None` so that list items with values come before those without.
#[inline]
fn some_cmp<T, F>(a: Option<T>, b: Option<T>, f: F) -> Ordering
where
    F: Fn(&T, &T) -> Ordering,
{
    match (a, b) {
        (Some(a), Some(b)) => f(&a, &b),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
    }
}

fn user_cmp(a: &MemberItem, b: &MemberItem, field: &SortFieldUser) -> Ordering {
    let a_id = a.member.user_id();
    let b_id = b.member.user_id();

    match field {
        SortFieldUser::UserId => a_id.cmp(b_id),
        SortFieldUser::LocalPart => a_id.localpart().cmp(b_id.localpart()),
        SortFieldUser::Server => a_id.server_name().cmp(b_id.server_name()),
        SortFieldUser::PowerLevel => {
            // Sort higher power levels towards the top of the list.
            b.member.power_level().cmp(&a.member.power_level())
        },
    }
}

fn room_cmp<T: RoomLikeItem>(
    a: &T,
    b: &T,
    field: &SortFieldRoom,
    collator: &mut Collator,
) -> Ordering {
    match field {
        SortFieldRoom::Favorite => {
            let fava = a.has_tag(TagName::Favorite);
            let favb = b.has_tag(TagName::Favorite);

            // If a has Favorite and b doesn't, it should sort earlier in room list.
            favb.cmp(&fava)
        },
        SortFieldRoom::LowPriority => {
            let lowa = a.has_tag(TagName::LowPriority);
            let lowb = b.has_tag(TagName::LowPriority);

            // If a has LowPriority and b doesn't, it should sort later in room list.
            lowa.cmp(&lowb)
        },
        SortFieldRoom::Name => collator.collate(a.name(), b.name()),
        SortFieldRoom::Alias => some_cmp(a.alias(), b.alias(), Ord::cmp),
        SortFieldRoom::RoomId => a.room_id().cmp(b.room_id()),
        SortFieldRoom::Unread => {
            // Sort true (unread) before false (read)
            b.is_unread().cmp(&a.is_unread())
        },
        SortFieldRoom::Recent => {
            // sort larger timestamps towards the top.
            some_cmp(a.recent_ts(), b.recent_ts(), |a, b| b.cmp(a))
        },
        SortFieldRoom::Invite => {
            // sort invites before other rooms.
            b.is_invite().cmp(&a.is_invite())
        },
    }
}

/// Compare two rooms according the configured sort criteria.
fn room_fields_cmp<T: RoomLikeItem>(
    a: &T,
    b: &T,
    fields: &[SortColumn<SortFieldRoom>],
    collator: &mut Collator,
) -> Ordering {
    for SortColumn(field, order) in fields {
        match (room_cmp(a, b, field, collator), order) {
            (Ordering::Equal, _) => continue,
            (o, SortOrder::Ascending) => return o,
            (o, SortOrder::Descending) => return o.reverse(),
        }
    }

    // Break ties on ascending room id.
    room_cmp(a, b, &SortFieldRoom::RoomId, collator)
}

fn user_fields_cmp(
    a: &MemberItem,
    b: &MemberItem,
    fields: &[SortColumn<SortFieldUser>],
) -> Ordering {
    for SortColumn(field, order) in fields {
        match (user_cmp(a, b, field), order) {
            (Ordering::Equal, _) => continue,
            (o, SortOrder::Ascending) => return o,
            (o, SortOrder::Descending) => return o.reverse(),
        }
    }

    // Break ties on ascending user id.
    user_cmp(a, b, &SortFieldUser::UserId)
}

fn tag_to_span(tag: &TagName, style: Style) -> Vec<Span<'_>> {
    match tag {
        TagName::Favorite => vec![Span::styled("Favorite", style)],
        TagName::LowPriority => vec![Span::styled("Low Priority", style)],
        TagName::ServerNotice => vec![Span::styled("Server Notice", style)],
        TagName::User(tag) => {
            vec![
                Span::styled("User Tag: ", style),
                Span::styled(tag.as_ref(), style),
            ]
        },
        tag => vec![Span::styled(format!("{tag:?}"), style)],
    }
}

fn append_tags<'a>(tags: Vec<Vec<Span<'a>>>, spans: &mut Vec<Span<'a>>, style: Style) {
    if tags.is_empty() {
        return;
    }

    spans.push(Span::styled(" (", style));

    for (i, tag) in tags.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(", ", style));
        }

        spans.extend(tag);
    }

    spans.push(Span::styled(")", style));
}

trait RoomLikeItem {
    fn room_id(&self) -> &RoomId;
    fn has_tag(&self, tag: TagName) -> bool;
    fn is_unread(&self) -> bool;
    /// Whether a snooze is hiding this entry from the inbox right now.
    ///
    /// Kept apart from [RoomLikeItem::is_unread], which stays truthful: a deferred entry is still
    /// unread, still bold, and still counted by the unread sort. Only the inbox windows filter on
    /// this, so a snooze hides an entry from the place the user triages and nowhere else.
    fn is_deferred(&self) -> bool;
    fn recent_ts(&self) -> Option<&MessageTimeStamp>;
    fn alias(&self) -> Option<&RoomAliasId>;
    fn name(&self) -> &str;
    fn is_invite(&self) -> bool;
}

#[inline]
fn room_prompt(
    room_id: &RoomId,
    act: &PromptAction,
    ctx: &ProgramContext,
) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
    match act {
        PromptAction::Submit => {
            let room = IambId::Room(room_id.to_owned(), None);
            let open = WindowAction::Switch(OpenTarget::Application(room));
            let acts = vec![(open.into(), ctx.clone())];

            Ok(acts)
        },
        PromptAction::Abort(_) => {
            let msg = "Cannot abort entry inside a list";
            let err = EditError::Failure(msg.into());

            Err(err)
        },
        PromptAction::Recall(..) => {
            let msg = "Cannot recall history inside a list";
            let err = EditError::Failure(msg.into());

            Err(err)
        },
    }
}

#[inline]
fn thread_prompt(
    room_id: &RoomId,
    thread_root: &OwnedEventId,
    act: &PromptAction,
    ctx: &ProgramContext,
) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
    match act {
        PromptAction::Submit => {
            let thread = IambId::Room(room_id.to_owned(), Some(thread_root.clone()));
            let open = WindowAction::Switch(OpenTarget::Application(thread));
            let acts = vec![(open.into(), ctx.clone())];

            Ok(acts)
        },
        PromptAction::Abort(_) => {
            let msg = "Cannot abort entry inside a list";
            let err = EditError::Failure(msg.into());

            Err(err)
        },
        PromptAction::Recall(..) => {
            let msg = "Cannot recall history inside a list";
            let err = EditError::Failure(msg.into());

            Err(err)
        },
    }
}

/// What `:read` should mark read: a room, or one thread within it.
pub struct ReadTarget {
    room_id: OwnedRoomId,
    thread: Option<OwnedEventId>,
}

/// A list entry that `:read` can act on while it is selected.
pub trait Readable {
    fn read_target(&self) -> ReadTarget;
}

/// The room or thread that has gone unread the longest.
///
/// This is the entry that sits at the bottom of `:unreadsandthreads`, found the same way that
/// window finds it, so that walking the inbox with `:nextunread` and walking it by hand agree on
/// what comes next. Deferred entries are left out for the same reason they are left out there.
fn least_recent_unread(store: &mut ProgramStore) -> Option<ReadTarget> {
    let chats = chat_items(store)
        .into_iter()
        .filter(|item| item.is_unread() && !item.is_deferred())
        .map(|item| (item.recent_ts().copied(), item.read_target()));

    let threads = followed_thread_items(store)
        .into_iter()
        .filter(|item| item.is_unread() && !item.is_deferred())
        .map(|item| (item.recent_ts().copied(), item.read_target()));

    // An entry whose latest message is not loaded has no timestamp to place it by. It sorts last
    // here, which is where the inbox windows put it too.
    chats
        .chain(threads)
        .min_by_key(|(ts, _)| (ts.is_none(), *ts))
        .map(|(_, target)| target)
}

/// Mark the entry selected in a list window read.
///
/// This is the list-window counterpart to [RoomState::room_command]'s handling of
/// [RoomAction::MarkRead], which can only ever act on the focused room.
fn mark_selection_read<T>(list: &ListState<T, IambInfo>, store: &mut ProgramStore) -> IambResult<()>
where
    T: ListItem<IambInfo> + Readable,
{
    let Some(item) = list.get() else {
        return Err(IambError::NoSelectedRoom.into());
    };

    let ReadTarget { room_id, thread } = item.read_target();
    let user_id = store.application.settings.profile.user_id.clone();

    store.application.record_read(vec![room_id.clone()], |app| {
        app.rooms.get_or_default(room_id.clone()).mark_read(&user_id, thread);
    });

    // Any notification we showed for this room is now stale.
    store.application.open_notifications.remove(&room_id);

    Ok(())
}

/// Defer or restore the entry selected in a list window.
///
/// The list-window counterpart to [RoomState::room_command]'s handling of [RoomAction::Snooze],
/// which can only act on the focused room. The entry's read target names the room and the thread,
/// which is exactly the key a snooze uses.
fn snooze_selection<T>(
    list: &ListState<T, IambInfo>,
    act: &RoomAction,
    store: &mut ProgramStore,
) -> IambResult<()>
where
    T: ListItem<IambInfo> + Readable,
{
    let Some(item) = list.get() else {
        return Err(IambError::NoSelectedRoom.into());
    };

    let ReadTarget { room_id, thread } = item.read_target();
    let key = SnoozeKey { room_id, thread };

    match act {
        RoomAction::Snooze(when) => {
            let wake_at = store.application.parse_snooze(when)?;

            let room_id = key.room_id.clone();

            store.application.snooze.set(key, wake_at);
            store.application.snooze_dirty.insert(room_id);
        },
        RoomAction::Unsnooze => {
            let room_id = key.room_id.clone();

            if !store.application.snooze.clear(&key) {
                return Err(IambError::NotSnoozed.into());
            }

            store.application.snooze_dirty.insert(room_id);
        },
        _ => return Err(IambError::NoSelectedRoomOrSpace.into()),
    }

    Ok(())
}

macro_rules! delegate {
    ($s: expr, $id: ident => $e: expr) => {
        match $s {
            IambWindow::Room($id) => $e,
            IambWindow::DirectList($id) => $e,
            IambWindow::MemberList($id, _, _) => $e,
            IambWindow::RoomList($id) => $e,
            IambWindow::SpaceList($id) => $e,
            IambWindow::VerifyList($id) => $e,
            IambWindow::Welcome($id) => $e,
            IambWindow::ChatList($id) => $e,
            IambWindow::UnreadList($id) => $e,
            IambWindow::ThreadList($id) => $e,
            IambWindow::SnoozeList($id) => $e,
            IambWindow::UnreadThreadList($id) => $e,
            IambWindow::MentionList($id) => $e,
            IambWindow::CommandPalette($id) => $e,
            IambWindow::QuickSwitcher($id) => $e,
            IambWindow::MessageSearch($id, _) => $e,
        }
    };
}

pub enum IambWindow {
    DirectList(DirectListState),
    MemberList(MemberListState, OwnedRoomId, Option<Instant>),
    Room(RoomState),
    VerifyList(VerifyListState),
    RoomList(RoomListState),
    SpaceList(SpaceListState),
    Welcome(WelcomeState),
    ChatList(ChatListState),
    UnreadList(UnreadListState),
    ThreadList(ThreadListState),
    SnoozeList(SnoozeListState),
    UnreadThreadList(UnreadThreadListState),
    MentionList(MentionListState),
    CommandPalette(CommandPaletteState),
    QuickSwitcher(QuickSwitcherState),

    /// The `:search` window, together with the term it was opened for.
    MessageSearch(MessageSearchState, String),
}

impl IambWindow {
    /// Open or refresh the completion popup, if this window is composing a message.
    pub fn show_completions(&mut self, ctx: &ProgramContext, store: &mut ProgramStore) {
        if let IambWindow::Room(w) = self {
            w.show_completions(ctx, store)
        }
    }

    /// Take the highlighted entry from this window's completion popup, if it has one open.
    ///
    /// Returns whether there was anything to take, so that a key bound to this can fall back to
    /// doing whatever it would otherwise have done.
    pub fn accept_completion(&mut self, ctx: &ProgramContext, store: &mut ProgramStore) -> bool {
        match self {
            IambWindow::Room(w) => w.accept_completion(ctx, store),
            _ => false,
        }
    }

    pub fn focus_toggle(&mut self) {
        if let IambWindow::Room(w) = self {
            w.focus_toggle()
        } else {
            return;
        }
    }

    pub async fn message_command(
        &mut self,
        act: MessageAction,
        ctx: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        if let IambWindow::Room(w) = self {
            w.message_command(act, ctx, store).await
        } else {
            return Err(IambError::NoSelectedRoom.into());
        }
    }

    pub async fn space_command(
        &mut self,
        act: SpaceAction,
        ctx: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        if let IambWindow::Room(w) = self {
            w.space_command(act, ctx, store).await
        } else {
            return Err(IambError::NoSelectedRoom.into());
        }
    }

    pub async fn room_command(
        &mut self,
        act: RoomAction,
        ctx: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<Vec<(Action<IambInfo>, ProgramContext)>> {
        if let IambWindow::Room(w) = self {
            return w.room_command(act, ctx, store).await;
        }

        // The list windows have no focused room, but they do have a selected entry, and marking
        // that entry read is well-defined.
        if let RoomAction::MarkRead = act {
            match self {
                IambWindow::DirectList(l) => mark_selection_read(l, store)?,
                IambWindow::RoomList(l) => mark_selection_read(l, store)?,
                IambWindow::ChatList(l) |
                IambWindow::UnreadList(l) |
                IambWindow::MentionList(l) => mark_selection_read(l, store)?,
                IambWindow::ThreadList(l) => mark_selection_read(l, store)?,
                IambWindow::UnreadThreadList(l) => mark_selection_read(l, store)?,
                _ => return Err(IambError::NoSelectedRoomOrSpace.into()),
            }

            return Ok(vec![]);
        }

        // Snoozing the selected entry is well-defined in the same windows, and for the same
        // reason: the entry names a room and possibly a thread, which is all a snooze needs.
        if let RoomAction::Snooze(_) | RoomAction::Unsnooze = &act {
            match self {
                IambWindow::DirectList(l) => snooze_selection(l, &act, store)?,
                IambWindow::RoomList(l) => snooze_selection(l, &act, store)?,
                IambWindow::ChatList(l) |
                IambWindow::UnreadList(l) |
                IambWindow::MentionList(l) => snooze_selection(l, &act, store)?,
                IambWindow::ThreadList(l) => snooze_selection(l, &act, store)?,
                IambWindow::SnoozeList(l) => snooze_selection(l, &act, store)?,
                IambWindow::UnreadThreadList(l) => snooze_selection(l, &act, store)?,
                _ => return Err(IambError::NoSelectedRoomOrSpace.into()),
            }

            return Ok(vec![]);
        }

        // Every other room command runs against whichever room the selected entry names.
        let id = match self {
            IambWindow::MemberList(_, room_id, _) => Some(&**room_id),

            IambWindow::DirectList(state) => state.get().map(|state| state.room_id()),
            IambWindow::RoomList(state) => state.get().map(|state| state.room_id()),
            IambWindow::SpaceList(state) => state.get().map(|state| state.room_id()),
            IambWindow::ChatList(state) |
            IambWindow::UnreadList(state) |
            IambWindow::MentionList(state) => state.get().map(|state| state.room_id()),

            _ => None,
        };

        if let Some(id) = id {
            room_command(id, act, ctx, store).await
        } else {
            return Err(IambError::NoSelectedRoomOrSpace.into());
        }
    }

    pub async fn send_command(
        &mut self,
        act: SendAction,
        ctx: ProgramContext,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        if let IambWindow::Room(w) = self {
            w.send_command(act, ctx, store).await
        } else {
            return Err(IambError::NoSelectedRoom.into());
        }
    }
}

pub type DirectListState = ListState<DirectItem, IambInfo>;
pub type MemberListState = ListState<MemberItem, IambInfo>;
pub type RoomListState = ListState<RoomItem, IambInfo>;
pub type ChatListState = ListState<GenericChatItem, IambInfo>;
pub type UnreadListState = ListState<GenericChatItem, IambInfo>;
pub type ThreadListState = ListState<ThreadItem, IambInfo>;
pub type SnoozeListState = ListState<SnoozeItem, IambInfo>;
pub type UnreadThreadListState = ListState<UnreadThreadItem, IambInfo>;
pub type MentionListState = ListState<GenericChatItem, IambInfo>;
pub type SpaceListState = ListState<SpaceItem, IambInfo>;
pub type VerifyListState = ListState<VerifyItem, IambInfo>;

impl From<ChatListState> for IambWindow {
    fn from(list: ChatListState) -> Self {
        IambWindow::ChatList(list)
    }
}

impl From<RoomState> for IambWindow {
    fn from(room: RoomState) -> Self {
        IambWindow::Room(room)
    }
}

impl From<VerifyListState> for IambWindow {
    fn from(list: VerifyListState) -> Self {
        IambWindow::VerifyList(list)
    }
}

impl From<DirectListState> for IambWindow {
    fn from(list: DirectListState) -> Self {
        IambWindow::DirectList(list)
    }
}

impl From<RoomListState> for IambWindow {
    fn from(list: RoomListState) -> Self {
        IambWindow::RoomList(list)
    }
}

impl From<SpaceListState> for IambWindow {
    fn from(list: SpaceListState) -> Self {
        IambWindow::SpaceList(list)
    }
}

impl From<WelcomeState> for IambWindow {
    fn from(win: WelcomeState) -> Self {
        IambWindow::Welcome(win)
    }
}

impl Editable<ProgramContext, ProgramStore, IambInfo> for IambWindow {
    fn editor_command(
        &mut self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        delegate!(self, w => w.editor_command(act, ctx, store))
    }
}

impl Jumpable<ProgramContext, IambInfo> for IambWindow {
    fn jump(
        &mut self,
        list: PositionList,
        dir: MoveDir1D,
        count: usize,
        ctx: &ProgramContext,
    ) -> IambResult<usize> {
        delegate!(self, w => w.jump(list, dir, count, ctx))
    }
}

impl Scrollable<ProgramContext, ProgramStore, IambInfo> for IambWindow {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        delegate!(self, w => w.scroll(style, ctx, store))
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for IambWindow {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        delegate!(self, w => w.prompt(act, ctx, store))
    }
}

impl TerminalCursor for IambWindow {
    fn get_term_cursor(&self) -> Option<TermOffset> {
        delegate!(self, w => w.get_term_cursor())
    }
}

/// Draw a list window's entries, or the message that says why it has none.
fn render_list<T: ListItem<IambInfo>>(
    state: &mut ListState<T, IambInfo>,
    empty_message: &'static str,
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    store: &mut ProgramStore,
) {
    List::new(store)
        .empty_message(empty_message)
        .empty_alignment(Alignment::Center)
        .focus(focused)
        .render(area, buf, state);
}

/// Which numbers a list window reports in its title.
#[derive(Clone, Copy)]
enum Counted {
    /// One number, because every entry in the list is the same kind of thing.
    ///
    /// The inbox windows are here: everything in them is unread, so a second number would repeat
    /// the first one.
    Total,

    /// Both numbers, because the list mixes read and unread entries.
    ///
    /// The unread number is the one the user acts on, and the total is what tells them how much
    /// of the list they are not looking at yet.
    UnreadAndTotal,
}

/// Add what a list holds to its title.
///
/// The numbers go in brackets after the name, which keeps the name in the same place in every
/// window and puts the part that changes on every sync at the end.
fn counted_title(name: &str, counted: Counted, counts: Option<ListCounts>) -> Line<'static> {
    let mut spans = vec![Span::styled(name.to_string(), bold_style())];

    let Some(counts) = counts else {
        return Line::from(spans);
    };

    let inner = match counted {
        Counted::Total => counts.total.to_string(),
        // Nothing is unread when the count is zero, and a "0 unread" that is almost always there
        // trains the user to stop reading the brackets.
        Counted::UnreadAndTotal if counts.unread == 0 => counts.total.to_string(),
        Counted::UnreadAndTotal => format!("{} unread / {}", counts.unread, counts.total),
    };

    spans.push(Span::styled(format!(" [{inner}]"), bold_style()));

    if counts.filtered {
        spans.push(filtered_note());
    }

    Line::from(spans)
}

/// The mark that says a count is of a filtered list rather than of everything.
///
/// A number that leaves entries out has to say so, or the user reads it as the size of the whole
/// list and believes rooms have gone missing.
fn filtered_note() -> Span<'static> {
    Span::styled(" (filtered)", Style::default().add_modifier(StyleModifier::DIM))
}

/// An entry that knows whether its traffic is addressed to the user.
trait AddressedItem {
    /// Whether the traffic here is addressed to the user rather than to a group.
    ///
    /// A busy room the user is only a member of says nothing to them in particular, but a
    /// mention names them, and a DM has nobody else it could be for.
    fn is_addressed_to_the_user(&self) -> bool;
}

/// Whether an entry belongs in the `:unreadmentions` window.
///
/// The snooze is honoured here the same way the other inbox windows honour it. A mention the
/// user deliberately postponed has to stay postponed, or the snooze would be worth nothing for
/// exactly the traffic it matters most for.
fn is_unread_mention<I: RoomLikeItem + AddressedItem>(item: &I) -> bool {
    item.is_unread() && !item.is_deferred() && item.is_addressed_to_the_user()
}

/// Build the entries for one of the windows that mixes rooms and DMs.
fn chat_items(store: &mut ProgramStore) -> Vec<GenericChatItem> {
    let sync_info = &store.application.sync_info;
    let rooms = sync_info.rooms.clone();
    let dms = sync_info.dms.clone();

    let mut items = rooms
        .into_iter()
        .map(|room_info| GenericChatItem::new(room_info, store, false))
        .collect::<Vec<_>>();

    items.extend(
        dms.into_iter()
            .map(|room_info| GenericChatItem::new(room_info, store, true)),
    );

    items
}

impl IambWindow {
    /// What this window's list holds, as [IambWindow::refresh] last counted it.
    ///
    /// Nothing is returned when the user has turned the counts off, which leaves every title
    /// exactly as it was before the counts existed.
    fn counts(&self, store: &ProgramStore) -> Option<ListCounts> {
        if !store.application.settings.tunables.list_counts {
            return None;
        }

        store.application.list_counts.get(&self.id()).copied()
    }

    /// This window's title, with what its list holds.
    fn list_title(&self, name: &str, counted: Counted, store: &ProgramStore) -> Line<'static> {
        counted_title(name, counted, self.counts(store))
    }

    /// Put the entries into this window's list.
    ///
    /// Windows are opened empty, so something has to fill them in. This is that something, and it
    /// is called from two places: [Window::open], so that a window has its entries the moment it
    /// exists, and [WindowOps::draw], so that they keep up with the rest of the client.
    ///
    /// Filling in at open is what makes a macro like `:unreadsandthreads<Enter>G<Enter>` work. All
    /// of a macro's keys are consumed in one batch before anything is redrawn, so `G` and `<Enter>`
    /// would otherwise be moving around and selecting from a list that is still empty. There is
    /// nothing to wait for here: every one of these lists is built from state already in the
    /// store, so the only reason they were empty was that nobody had asked for them yet.
    ///
    /// The exception is [IambWindow::MemberList], whose entries come from the worker rather than
    /// the store. It keeps its own debounce, which decides on its own terms whether this actually
    /// asks for anything.
    pub fn refresh(&mut self, store: &mut ProgramStore) {
        let id = self.id();

        /// Leave the counts where [Window::get_win_title] can find them.
        macro_rules! record {
            ($total: expr, $unread: expr, $filtered: expr) => {{
                let counts = ListCounts {
                    total: $total,
                    unread: $unread,
                    filtered: $filtered,
                };

                store.application.list_counts.insert(id.clone(), counts);
            }};
        }

        /// Sort the entries the way the user has configured this kind of list to be sorted, and
        /// put them in the list.
        macro_rules! sorted {
            ($state: expr, $items: expr, $field: ident) => {{
                let mut items = $items;
                let fields = &store.application.settings.tunables.sort.$field;
                let collator = &mut store.application.collator;

                items.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
                record!(items.len(), items.iter().filter(|i| i.is_unread()).count(), false);
                $state.set(items);
            }};
        }

        /// Put the entries in a list that has no sort of its own, and count them.
        macro_rules! unsorted {
            ($state: expr, $items: expr) => {{
                let items = $items;

                record!(items.len(), 0, false);
                $state.set(items);
            }};
        }

        match self {
            // These have no list of their own to fill in.
            IambWindow::Room(_) | IambWindow::Welcome(_) => {},

            // These decide for themselves what belongs in their list, based on what has been
            // typed into their filter bar.
            IambWindow::CommandPalette(state) => {
                state.rebuild(store);
                record!(state.len(), 0, state.is_filtered());
            },
            IambWindow::QuickSwitcher(state) => {
                state.rebuild(store);
                record!(state.len(), 0, state.is_filtered());
            },
            IambWindow::MessageSearch(state, _) => {
                state.rebuild(store);
                record!(state.len(), 0, state.is_filtered());
            },

            IambWindow::DirectList(state) => {
                let items = store.application.sync_info.dms.clone();
                let items = items
                    .into_iter()
                    .map(|room_info| DirectItem::new(room_info, store))
                    .collect::<Vec<_>>();

                sorted!(state, items, dms);
            },
            IambWindow::RoomList(state) => {
                let items = store.application.sync_info.rooms.clone();
                let items = items
                    .into_iter()
                    .map(|room_info| RoomItem::new(room_info, store))
                    .collect::<Vec<_>>();

                sorted!(state, items, rooms);
            },
            IambWindow::SpaceList(state) => {
                let items = store.application.sync_info.spaces.clone();
                let items = items
                    .into_iter()
                    .map(|room| SpaceItem::new(room, store))
                    .collect::<Vec<_>>();

                sorted!(state, items, spaces);
            },
            IambWindow::ChatList(state) => {
                let items = chat_items(store);

                sorted!(state, items, chats);
            },
            IambWindow::UnreadList(state) => {
                let items = chat_items(store)
                    .into_iter()
                    .filter(|i| i.is_unread() && !i.is_deferred())
                    .collect::<Vec<_>>();

                sorted!(state, items, chats);
            },
            IambWindow::ThreadList(state) => {
                let items = followed_thread_items(store);

                sorted!(state, items, chats);
            },
            IambWindow::SnoozeList(state) => {
                // Already ordered soonest first by snoozed_items, and that order is the useful
                // one here, so the room sort is not applied.
                unsorted!(state, snoozed_items(store));
            },
            IambWindow::UnreadThreadList(state) => {
                let mut items = chat_items(store)
                    .into_iter()
                    .filter(|i| i.is_unread() && !i.is_deferred())
                    .map(UnreadThreadItem::Chat)
                    .collect::<Vec<_>>();

                let threads = followed_thread_items(store)
                    .into_iter()
                    .filter(|i| i.is_unread() && !i.is_deferred())
                    .map(UnreadThreadItem::Thread);

                items.extend(threads);

                sorted!(state, items, chats);
            },
            IambWindow::MentionList(state) => {
                let items =
                    chat_items(store).into_iter().filter(is_unread_mention).collect::<Vec<_>>();

                // The same sort as the other inbox windows, so that one room does not sit in a
                // different place here than it does there.
                sorted!(state, items, chats);
            },
            IambWindow::VerifyList(state) => {
                let mut items = store
                    .application
                    .verifications
                    .iter()
                    .map(VerifyItem::from)
                    .collect::<Vec<_>>();

                // Sort the active verifications towards the top.
                items.sort();

                unsorted!(state, items);
            },
            IambWindow::MemberList(state, room_id, last_fetch) => {
                let need_fetch = match last_fetch {
                    Some(i) => i.elapsed() >= MEMBER_FETCH_DEBOUNCE,
                    None => true,
                };

                if !need_fetch {
                    return;
                }

                if let Ok(mems) = store.application.worker.members(room_id.clone()) {
                    let mut items = mems
                        .into_iter()
                        .map(|m| MemberItem::new(m, room_id.clone()))
                        .collect::<Vec<_>>();
                    let fields = &store.application.settings.tunables.sort.members;

                    items.sort_by(|a, b| user_fields_cmp(a, b, fields));
                    state.set(items);
                    *last_fetch = Some(Instant::now());
                }
            },
        }
    }
}

impl WindowOps<IambInfo> for IambWindow {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        // These windows draw themselves, and refresh themselves as they do.
        match self {
            IambWindow::Room(state) => return state.draw(area, buf, focused, store),
            IambWindow::Welcome(state) => return state.draw(area, buf, focused, store),
            IambWindow::CommandPalette(state) => return state.draw(area, buf, focused, store),
            IambWindow::QuickSwitcher(state) => return state.draw(area, buf, focused, store),
            IambWindow::MessageSearch(state, _) => return state.draw(area, buf, focused, store),
            _ => {},
        }

        self.refresh(store);

        match self {
            IambWindow::DirectList(state) => {
                render_list(state, "No direct messages yet!", area, buf, focused, store)
            },
            IambWindow::MemberList(state, _, _) => {
                render_list(state, "No users here yet!", area, buf, focused, store)
            },
            IambWindow::RoomList(state) => {
                render_list(state, "You haven't joined any rooms yet", area, buf, focused, store)
            },
            IambWindow::SpaceList(state) => {
                render_list(state, "You haven't joined any spaces yet", area, buf, focused, store)
            },
            IambWindow::ChatList(state) => {
                render_list(state, "You do not have rooms or dms yet", area, buf, focused, store)
            },
            IambWindow::UnreadList(state) => {
                render_list(state, "You do not have any unreads yet", area, buf, focused, store)
            },
            IambWindow::ThreadList(state) => {
                let empty = "You are not following any threads yet";

                render_list(state, empty, area, buf, focused, store)
            },
            IambWindow::SnoozeList(state) => {
                let empty = "Nothing is snoozed";

                render_list(state, empty, area, buf, focused, store)
            },
            IambWindow::UnreadThreadList(state) => {
                let empty = "You do not have any unread rooms or threads yet";

                render_list(state, empty, area, buf, focused, store)
            },
            IambWindow::MentionList(state) => {
                let empty = "Nothing unread mentions you";

                render_list(state, empty, area, buf, focused, store)
            },
            IambWindow::VerifyList(state) => {
                render_list(state, "No in-progress verifications", area, buf, focused, store)
            },

            // Already drawn above.
            IambWindow::Room(_) |
            IambWindow::Welcome(_) |
            IambWindow::CommandPalette(_) |
            IambWindow::QuickSwitcher(_) |
            IambWindow::MessageSearch(..) => {},
        }
    }

    fn dup(&self, store: &mut ProgramStore) -> Self {
        match self {
            IambWindow::Room(w) => w.dup(store).into(),
            IambWindow::CommandPalette(w) => IambWindow::CommandPalette(w.dup(store)),
            IambWindow::QuickSwitcher(w) => IambWindow::QuickSwitcher(w.dup(store)),
            IambWindow::MessageSearch(w, term) => {
                IambWindow::MessageSearch(w.dup(store), term.clone())
            },
            IambWindow::DirectList(w) => w.dup(store).into(),
            IambWindow::MemberList(w, room_id, last_fetch) => {
                IambWindow::MemberList(w.dup(store), room_id.clone(), *last_fetch)
            },
            IambWindow::RoomList(w) => w.dup(store).into(),
            IambWindow::SpaceList(w) => w.dup(store).into(),
            IambWindow::VerifyList(w) => w.dup(store).into(),
            IambWindow::Welcome(w) => w.dup(store).into(),
            IambWindow::ChatList(w) => w.dup(store).into(),
            IambWindow::UnreadList(w) => w.dup(store).into(),
            IambWindow::ThreadList(w) => IambWindow::ThreadList(w.dup(store)),
            IambWindow::SnoozeList(w) => IambWindow::SnoozeList(w.dup(store)),
            IambWindow::UnreadThreadList(w) => IambWindow::UnreadThreadList(w.dup(store)),
            IambWindow::MentionList(w) => IambWindow::MentionList(w.dup(store)),
        }
    }

    fn close(&mut self, flags: CloseFlags, store: &mut ProgramStore) -> bool {
        delegate!(self, w => w.close(flags, store))
    }

    fn write(
        &mut self,
        path: Option<&str>,
        flags: WriteFlags,
        store: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        delegate!(self, w => w.write(path, flags, store))
    }

    fn get_completions(&self) -> Option<CompletionList> {
        delegate!(self, w => w.get_completions())
    }

    fn get_cursor_word(&self, style: &WordStyle) -> Option<String> {
        delegate!(self, w => w.get_cursor_word(style))
    }

    fn get_selected_word(&self) -> Option<String> {
        delegate!(self, w => w.get_selected_word())
    }
}

/// Build a window with an empty list, which [IambWindow::refresh] then fills in.
/// The title of a `:search` window: the term it was opened for, and what it could not look at.
///
/// The term is what tells two search windows apart, so it belongs in the title rather than only
/// in the window's URL.
///
/// The rooms the search could not look in belong here too, and this is the only place they fit. A
/// row is one message found, so a room that was never indexed has no row to appear on, and its
/// absence would otherwise read as a message that nobody sent.
///
/// The number of results goes here too, because a search says nothing about how much it found
/// until the user scrolls to the end of it.
fn search_title(
    state: &MessageSearchState,
    term: &str,
    counts: Option<ListCounts>,
) -> Line<'static> {
    let mut spans = vec![bold_span("Search: "), Span::raw(term.to_string())];

    if let Some(counts) = counts {
        spans.push(Span::styled(format!(" [{}]", counts.total), bold_style()));

        if counts.filtered {
            spans.push(filtered_note());
        }
    }

    if let Some(note) = state.context().coverage.note() {
        spans.push(Span::styled(
            format!(" ({note})"),
            Style::default().add_modifier(StyleModifier::DIM),
        ));
    }

    Line::from(spans)
}

fn open_empty(id: IambId, store: &mut ProgramStore) -> IambResult<IambWindow> {
    match id {
        IambId::Room(room_id, thread) => {
            let (room, name, tags) = store.application.worker.get_room(room_id)?;
            let room = RoomState::new(room, thread, name, tags, store);

            store.application.need_load.need_members(room.id().to_owned());
            return Ok(room.into());
        },
        IambId::DirectList => {
            let list = DirectListState::new(IambBufferId::DirectList, vec![]);

            return Ok(list.into());
        },
        IambId::MemberList(room_id) => {
            let id = IambBufferId::MemberList(room_id.clone());
            let list = MemberListState::new(id, vec![]);
            let win = IambWindow::MemberList(list, room_id, None);

            return Ok(win);
        },
        IambId::RoomList => {
            let list = RoomListState::new(IambBufferId::RoomList, vec![]);

            return Ok(list.into());
        },
        IambId::SpaceList => {
            let list = SpaceListState::new(IambBufferId::SpaceList, vec![]);

            return Ok(list.into());
        },
        IambId::VerifyList => {
            let list = VerifyListState::new(IambBufferId::VerifyList, vec![]);

            return Ok(list.into());
        },
        IambId::Welcome => {
            let win = WelcomeState::new(store);

            return Ok(win.into());
        },
        IambId::ChatList => {
            let list = ChatListState::new(IambBufferId::ChatList, vec![]);

            Ok(list.into())
        },
        IambId::UnreadList => {
            let list = UnreadListState::new(IambBufferId::UnreadList, vec![]);

            Ok(IambWindow::UnreadList(list))
        },
        IambId::ThreadList => {
            let list = ThreadListState::new(IambBufferId::ThreadList, vec![]);

            Ok(IambWindow::ThreadList(list))
        },
        IambId::SnoozeList => {
            let list = SnoozeListState::new(IambBufferId::SnoozeList, vec![]);

            Ok(IambWindow::SnoozeList(list))
        },
        IambId::CommandPalette => {
            let win = CommandPaletteState::new((), store);

            Ok(IambWindow::CommandPalette(win))
        },
        IambId::QuickSwitcher => {
            let win = QuickSwitcherState::new((), store);

            Ok(IambWindow::QuickSwitcher(win))
        },
        IambId::MessageSearch(term) => {
            // The search runs here, when the window is opened, rather than in the command that
            // opens it. That is what makes a window restored from a saved layout show results
            // rather than an empty list, and it means reopening the same search refreshes it.
            let found = store.application.worker.search_messages(term.clone())?;
            let found = Arc::new(Found::new(found, store));
            let win = MessageSearchState::new(found, store);

            Ok(IambWindow::MessageSearch(win, term))
        },
        IambId::UnreadThreadList => {
            let list = UnreadThreadListState::new(IambBufferId::UnreadThreadList, vec![]);

            Ok(IambWindow::UnreadThreadList(list))
        },
        IambId::MentionList => {
            let list = MentionListState::new(IambBufferId::MentionList, vec![]);

            Ok(IambWindow::MentionList(list))
        },
    }
}

impl Window<IambInfo> for IambWindow {
    fn id(&self) -> IambId {
        match self {
            IambWindow::Room(room) => IambId::Room(room.id().to_owned(), room.thread().cloned()),
            IambWindow::DirectList(_) => IambId::DirectList,
            IambWindow::MemberList(_, room_id, _) => IambId::MemberList(room_id.clone()),
            IambWindow::RoomList(_) => IambId::RoomList,
            IambWindow::SpaceList(_) => IambId::SpaceList,
            IambWindow::VerifyList(_) => IambId::VerifyList,
            IambWindow::Welcome(_) => IambId::Welcome,
            IambWindow::ChatList(_) => IambId::ChatList,
            IambWindow::UnreadList(_) => IambId::UnreadList,
            IambWindow::ThreadList(_) => IambId::ThreadList,
            IambWindow::SnoozeList(_) => IambId::SnoozeList,
            IambWindow::UnreadThreadList(_) => IambId::UnreadThreadList,
            IambWindow::MentionList(_) => IambId::MentionList,
            IambWindow::CommandPalette(_) => IambId::CommandPalette,
            IambWindow::QuickSwitcher(_) => IambId::QuickSwitcher,
            IambWindow::MessageSearch(_, term) => IambId::MessageSearch(term.clone()),
        }
    }

    fn get_tab_title(&self, store: &mut ProgramStore) -> Line<'_> {
        match self {
            IambWindow::DirectList(_) => {
                self.list_title("Direct Messages", Counted::UnreadAndTotal, store)
            },
            IambWindow::RoomList(_) => self.list_title("Rooms", Counted::UnreadAndTotal, store),
            IambWindow::SpaceList(_) => self.list_title("Spaces", Counted::Total, store),
            IambWindow::VerifyList(_) => self.list_title("Verifications", Counted::Total, store),
            IambWindow::Welcome(_) => bold_spans("Welcome to iamb"),
            IambWindow::ChatList(_) => {
                self.list_title("DMs & Rooms", Counted::UnreadAndTotal, store)
            },
            IambWindow::UnreadList(_) => self.list_title("Unread Messages", Counted::Total, store),
            IambWindow::ThreadList(_) => self.list_title("Threads", Counted::UnreadAndTotal, store),
            IambWindow::SnoozeList(_) => self.list_title("Snoozed", Counted::Total, store),
            IambWindow::UnreadThreadList(_) => {
                self.list_title("Unread Rooms & Threads", Counted::Total, store)
            },
            IambWindow::MentionList(_) => {
                self.list_title("Unread Mentions & DMs", Counted::Total, store)
            },
            IambWindow::CommandPalette(_) => self.list_title("Commands", Counted::Total, store),
            IambWindow::QuickSwitcher(_) => self.list_title("Jump to", Counted::Total, store),
            IambWindow::MessageSearch(state, term) => search_title(state, term, self.counts(store)),

            IambWindow::Room(w) => {
                let title = store.application.get_room_title(w.id());

                Line::from(title)
            },
            IambWindow::MemberList(state, room_id, _) => {
                let title = store.application.get_room_title(room_id.as_ref());
                let n = state.len();
                let v = vec![
                    bold_span("Room Members "),
                    Span::styled(format!("({n}): "), bold_style()),
                    title.into(),
                ];
                Line::from(v)
            },
        }
    }

    fn get_win_title(&self, store: &mut ProgramStore) -> Line<'_> {
        match self {
            IambWindow::DirectList(_) => {
                self.list_title("Direct Messages", Counted::UnreadAndTotal, store)
            },
            IambWindow::RoomList(_) => self.list_title("Rooms", Counted::UnreadAndTotal, store),
            IambWindow::SpaceList(_) => self.list_title("Spaces", Counted::Total, store),
            IambWindow::VerifyList(_) => self.list_title("Verifications", Counted::Total, store),
            IambWindow::Welcome(_) => bold_spans("Welcome to iamb"),
            IambWindow::ChatList(_) => {
                self.list_title("DMs & Rooms", Counted::UnreadAndTotal, store)
            },
            IambWindow::UnreadList(_) => self.list_title("Unread Messages", Counted::Total, store),
            IambWindow::ThreadList(_) => self.list_title("Threads", Counted::UnreadAndTotal, store),
            IambWindow::SnoozeList(_) => self.list_title("Snoozed", Counted::Total, store),
            IambWindow::UnreadThreadList(_) => {
                self.list_title("Unread Rooms & Threads", Counted::Total, store)
            },
            IambWindow::MentionList(_) => {
                self.list_title("Unread Mentions & DMs", Counted::Total, store)
            },
            IambWindow::CommandPalette(_) => self.list_title("Commands", Counted::Total, store),
            IambWindow::QuickSwitcher(_) => self.list_title("Jump to", Counted::Total, store),
            IambWindow::MessageSearch(state, term) => search_title(state, term, self.counts(store)),

            IambWindow::Room(w) => w.get_title(store),
            IambWindow::MemberList(state, room_id, _) => {
                let title = store.application.get_room_title(room_id.as_ref());
                let n = state.len();
                let v = vec![
                    bold_span("Room Members "),
                    Span::styled(format!("({n}): "), bold_style()),
                    title.into(),
                ];
                Line::from(v)
            },
        }
    }

    fn open(id: IambId, store: &mut ProgramStore) -> IambResult<Self> {
        let mut win = open_empty(id, store)?;

        // A window is no use to a macro that opens it and immediately acts on it if its list is
        // still empty when the next key arrives, and nothing here has to be waited on.
        win.refresh(store);

        Ok(win)
    }

    fn find(name: String, store: &mut ProgramStore) -> IambResult<Self> {
        let ChatStore { names, worker, .. } = &mut store.application;

        if let Some(room) = names.get_mut(&name) {
            let id = IambId::Room(room.clone(), None);

            IambWindow::open(id, store)
        } else {
            let room_id = worker.join_room(name.clone())?;
            names.insert(name, room_id.clone());

            let (room, name, tags) = store.application.worker.get_room(room_id)?;
            let room = RoomState::new(room, None, name, tags, store);

            store.application.need_load.need_members(room.id().to_owned());
            Ok(room.into())
        }
    }

    fn posn(index: usize, _: &mut ProgramStore) -> IambResult<Self> {
        let msg = format!("Cannot find indexed buffer (index = {index})");
        let err = UIError::Unimplemented(msg);

        Err(err)
    }

    fn unnamed(store: &mut ProgramStore) -> IambResult<Self> {
        Self::open(IambId::RoomList, store)
    }
}

/// Gather the threads the user follows across every joined room and DM.
fn followed_thread_items(store: &mut ProgramStore) -> Vec<ThreadItem> {
    let sync_info = &store.application.sync_info;
    let room_infos = sync_info
        .rooms
        .iter()
        .chain(sync_info.dms.iter())
        .cloned()
        .collect::<Vec<_>>();

    // The snooze cache is a third field borrow beside rooms and settings, which is why it lives on
    // ChatStore rather than on RoomInfo.
    let now = store.application.now_ms();
    let ChatStore { rooms, settings, snooze, .. } = &mut store.application;

    room_infos
        .into_iter()
        .flat_map(|room_info| {
            let room = &room_info.deref().0;
            let alias = room.canonical_alias();
            let info = rooms.get_or_default(room.room_id().to_owned());
            let room_name = info.name.clone().unwrap_or_default();

            info.followed_threads(settings)
                .into_iter()
                .map(|summary| {
                    let wake_at =
                        snooze.wake_at(&room.room_id().to_owned(), Some(&summary.root));

                    ThreadItem::new(
                        room_info.clone(),
                        room_name.clone(),
                        alias.clone(),
                        summary,
                        wake_at,
                        now,
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// An entry in the `:threads` window: one thread that the user follows.
#[derive(Clone)]
pub struct ThreadItem {
    room_info: MatrixRoomInfo,
    room_name: String,
    alias: Option<OwnedRoomAliasId>,
    thread_root: OwnedEventId,
    preview: String,
    unread: UnreadInfo,
    /// True while a snooze on this thread, or on its room, is still running.
    deferred: bool,
}

impl ThreadItem {
    fn new(
        room_info: MatrixRoomInfo,
        room_name: String,
        alias: Option<OwnedRoomAliasId>,
        summary: ThreadSummary,
        wake_at: Option<WakeTime>,
        now: WakeTime,
    ) -> Self {
        let ThreadSummary { root, preview, unread } = summary;

        ThreadItem {
            room_info,
            room_name,
            alias,
            thread_root: root,
            preview,
            unread: unread.with_wake_time(wake_at),
            deferred: wake_at.is_some_and(|w| w > now),
        }
    }

    #[inline]
    fn room(&self) -> &MatrixRoom {
        &self.room_info.deref().0
    }
}

impl RoomLikeItem for ThreadItem {
    fn name(&self) -> &str {
        self.preview.as_str()
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        self.alias.as_deref()
    }

    fn room_id(&self) -> &RoomId {
        self.room().room_id()
    }

    fn has_tag(&self, tag: TagName) -> bool {
        if let Some(tags) = &self.room_info.deref().1 {
            tags.contains_key(&tag)
        } else {
            false
        }
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        self.unread.latest()
    }

    fn is_unread(&self) -> bool {
        self.unread.is_unread()
    }

    fn is_deferred(&self) -> bool {
        self.deferred
    }

    fn is_invite(&self) -> bool {
        // Threads only exist in rooms we've already joined.
        false
    }
}

impl Display for ThreadItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.preview)
    }
}

impl ListItem<IambInfo> for ThreadItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let style = selected_style(selected);
        let (name, mut labels) = name_and_labels(&self.preview, self.unread.is_unread(), style);
        let mut spans = vec![name];

        labels.push(vec![
            Span::styled("Thread in ", style),
            Span::styled(self.room_name.as_str(), style),
        ]);

        append_tags(labels, &mut spans, style);
        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        self.thread_root.to_string().into()
    }
}

impl Readable for ThreadItem {
    fn read_target(&self) -> ReadTarget {
        ReadTarget {
            room_id: self.room_id().to_owned(),
            thread: Some(self.thread_root.clone()),
        }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for ThreadItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        thread_prompt(self.room_id(), &self.thread_root, act, ctx)
    }
}

/// An entry in the `:snoozed` window: one deferred room or thread, and when it returns.
#[derive(Clone)]
pub struct SnoozeItem {
    room_id: OwnedRoomId,
    room_name: String,
    thread: Option<OwnedEventId>,
    /// A word describing the thread, so that two threads in one room are distinguishable.
    preview: String,
    wake_at: WakeTime,
}

impl SnoozeItem {
    fn name(&self) -> &str {
        match &self.thread {
            None => self.room_name.as_str(),
            Some(_) => self.preview.as_str(),
        }
    }
}

impl Display for SnoozeItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl ListItem<IambInfo> for SnoozeItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let style = selected_style(selected);
        let mut spans = vec![Span::styled(self.name().to_string(), style)];
        let mut labels = vec![];

        if self.thread.is_some() {
            labels.push(vec![
                Span::styled("Thread in ", style),
                Span::styled(self.room_name.as_str(), style),
            ]);
        }

        // The wake time is the reason this window exists, so it is always shown.
        labels.push(vec![
            Span::styled("Until ", style),
            Span::styled(describe(self.wake_at), style),
        ]);

        append_tags(labels, &mut spans, style);
        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        self.room_id.to_string().into()
    }
}

impl Readable for SnoozeItem {
    fn read_target(&self) -> ReadTarget {
        ReadTarget {
            room_id: self.room_id.clone(),
            thread: self.thread.clone(),
        }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for SnoozeItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        // Opening an entry goes where the entry points, which is the room or the thread. A snooze
        // does not have to be cancelled to look at what is waiting.
        match &self.thread {
            None => room_prompt(&self.room_id, act, ctx),
            Some(root) => thread_prompt(&self.room_id, root, act, ctx),
        }
    }
}

/// The rooms and threads that are deferred right now.
///
/// Expired entries are left out, because they are already back in the inbox.
fn snoozed_items(store: &mut ProgramStore) -> Vec<SnoozeItem> {
    let now = store.application.now_ms();
    let ChatStore { rooms, snooze, .. } = &mut store.application;

    let mut items = snooze
        .entries()
        .filter(|(_, wake_at)| **wake_at > now)
        .map(|(key, wake_at)| (key.clone(), *wake_at))
        .collect::<Vec<_>>();

    // Soonest first, so the next thing to come back is at the top.
    items.sort_by_key(|(_, wake_at)| *wake_at);

    items
        .into_iter()
        .map(|(key, wake_at)| {
            let info = rooms.get_or_default(key.room_id.clone());
            let room_name = info.name.clone().unwrap_or_else(|| key.room_id.to_string());
            let preview = match &key.thread {
                None => String::new(),
                // The same preview the :threads window shows, so one thread reads the same in
                // both places.
                Some(root) => info.thread_preview(root),
            };

            SnoozeItem {
                room_id: key.room_id,
                room_name,
                thread: key.thread,
                preview,
                wake_at,
            }
        })
        .collect()
}

/// An entry in the `:unreads-and-threads` window, which intermixes rooms and threads.
#[derive(Clone)]
pub enum UnreadThreadItem {
    Chat(GenericChatItem),
    Thread(ThreadItem),
}

macro_rules! delegate_unread_thread {
    ($s: expr, $item: ident => $e: expr) => {
        match $s {
            UnreadThreadItem::Chat($item) => $e,
            UnreadThreadItem::Thread($item) => $e,
        }
    };
}

impl RoomLikeItem for UnreadThreadItem {
    fn name(&self) -> &str {
        delegate_unread_thread!(self, item => item.name())
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        delegate_unread_thread!(self, item => item.alias())
    }

    fn room_id(&self) -> &RoomId {
        delegate_unread_thread!(self, item => item.room_id())
    }

    fn has_tag(&self, tag: TagName) -> bool {
        delegate_unread_thread!(self, item => item.has_tag(tag))
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        delegate_unread_thread!(self, item => item.recent_ts())
    }

    fn is_unread(&self) -> bool {
        delegate_unread_thread!(self, item => item.is_unread())
    }

    fn is_deferred(&self) -> bool {
        delegate_unread_thread!(self, item => item.is_deferred())
    }

    fn is_invite(&self) -> bool {
        delegate_unread_thread!(self, item => item.is_invite())
    }
}

impl Display for UnreadThreadItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        delegate_unread_thread!(self, item => Display::fmt(item, f))
    }
}

impl ListItem<IambInfo> for UnreadThreadItem {
    fn show(
        &self,
        selected: bool,
        vctx: &ViewportContext<ListCursor>,
        store: &mut ProgramStore,
    ) -> Text<'_> {
        delegate_unread_thread!(self, item => item.show(selected, vctx, store))
    }

    fn get_word(&self) -> Option<String> {
        delegate_unread_thread!(self, item => item.get_word())
    }
}

impl Readable for UnreadThreadItem {
    fn read_target(&self) -> ReadTarget {
        delegate_unread_thread!(self, item => item.read_target())
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for UnreadThreadItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        delegate_unread_thread!(self, item => item.prompt(act, ctx, store))
    }
}

#[derive(Clone)]
pub struct GenericChatItem {
    room_info: MatrixRoomInfo,
    name: String,
    alias: Option<OwnedRoomAliasId>,
    unread: UnreadInfo,
    is_dm: bool,
    /// True while a snooze on this room, or on the room the entry belongs to, is still running.
    deferred: bool,
    /// How many unread messages in this room mention the user.
    ///
    /// Matrix counts this, so iamb does not read message bodies to find it. See
    /// [unread_mentions] for which of the two counts the server and the client keep is used.
    mentions: u64,
}

/// How many unread messages in a room mention the user.
///
/// Two counts exist and neither one is correct everywhere, so the larger is taken. The server
/// count is the only one a room that the client has not calculated receipts for yet has. The
/// client count is the only correct one for an encrypted room, because the server cannot read
/// the messages and therefore cannot see a mention in them.
fn unread_mentions(room: &MatrixRoom) -> u64 {
    let server_side = room.unread_notification_counts().highlight_count;
    let client_side = room.num_unread_mentions();

    server_side.max(client_side)
}

impl GenericChatItem {
    fn new(room_info: MatrixRoomInfo, store: &mut ProgramStore, is_dm: bool) -> Self {
        let room = &room_info.deref().0;
        let room_id = room.room_id();

        // Read the snooze state before borrowing the room, so the two borrows do not overlap.
        let now = store.application.now_ms();
        let wake_at = store.application.snooze.wake_at(&room_id.to_owned(), None);
        let deferred = wake_at.is_some_and(|w| w > now);
        let mentions = unread_mentions(room);

        let info = store.application.rooms.get_or_default(room_id.to_owned());
        let name = info.name.clone().unwrap_or_default();
        let alias = room.canonical_alias();
        let unread = info
            .unreads(room.is_marked_unread(), &store.application.settings)
            .with_wake_time(wake_at);
        info.tags.clone_from(&room_info.deref().1);

        if let Some(alias) = &alias {
            store.application.names.insert(alias.to_string(), room_id.to_owned());
        }

        GenericChatItem {
            room_info,
            name,
            alias,
            is_dm,
            unread,
            deferred,
            mentions,
        }
    }

    #[inline]
    fn room(&self) -> &MatrixRoom {
        &self.room_info.deref().0
    }

    #[inline]
    fn tags(&self) -> &Option<Tags> {
        &self.room_info.deref().1
    }
}

impl AddressedItem for GenericChatItem {
    fn is_addressed_to_the_user(&self) -> bool {
        self.is_dm || self.mentions > 0
    }
}

impl RoomLikeItem for GenericChatItem {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        self.alias.as_deref()
    }

    fn room_id(&self) -> &RoomId {
        self.room().room_id()
    }

    fn has_tag(&self, tag: TagName) -> bool {
        if let Some(tags) = &self.room_info.deref().1 {
            tags.contains_key(&tag)
        } else {
            false
        }
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        self.unread.latest()
    }

    fn is_unread(&self) -> bool {
        self.unread.is_unread()
    }

    fn is_deferred(&self) -> bool {
        self.deferred
    }

    fn is_invite(&self) -> bool {
        self.room().state() == MatrixRoomState::Invited
    }
}

impl Display for GenericChatItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl ListItem<IambInfo> for GenericChatItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let unread = self.unread.is_unread();
        let style = selected_style(selected);
        let (name, mut labels) = name_and_labels(&self.name, unread, style);
        let mut spans = vec![name];

        labels.push(if self.is_dm {
            vec![Span::styled("DM", style)]
        } else {
            vec![Span::styled("Room", style)]
        });

        if let Some(tags) = &self.tags() {
            labels.extend(tags.keys().map(|t| tag_to_span(t, style)));
        }

        append_tags(labels, &mut spans, style);
        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        self.room_id().to_string().into()
    }
}

impl Readable for GenericChatItem {
    fn read_target(&self) -> ReadTarget {
        ReadTarget { room_id: self.room_id().to_owned(), thread: None }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for GenericChatItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        room_prompt(self.room_id(), act, ctx)
    }
}

#[derive(Clone)]
pub struct RoomItem {
    room_info: MatrixRoomInfo,
    name: String,
    alias: Option<OwnedRoomAliasId>,
    unread: UnreadInfo,
}

impl RoomItem {
    fn new(room_info: MatrixRoomInfo, store: &mut ProgramStore) -> Self {
        let room = &room_info.deref().0;
        let room_id = room.room_id();

        let info = store.application.rooms.get_or_default(room_id.to_owned());
        let name = info.name.clone().unwrap_or_default();
        let alias = room.canonical_alias();
        let unread = info.unreads(room.is_marked_unread(), &store.application.settings);
        info.tags.clone_from(&room_info.deref().1);

        if let Some(alias) = &alias {
            store.application.names.insert(alias.to_string(), room_id.to_owned());
        }

        RoomItem { room_info, name, alias, unread }
    }

    #[inline]
    fn room(&self) -> &MatrixRoom {
        &self.room_info.deref().0
    }

    #[inline]
    fn tags(&self) -> &Option<Tags> {
        &self.room_info.deref().1
    }
}

impl RoomLikeItem for RoomItem {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        self.alias.as_deref()
    }

    fn room_id(&self) -> &RoomId {
        self.room().room_id()
    }

    fn has_tag(&self, tag: TagName) -> bool {
        if let Some(tags) = &self.room_info.deref().1 {
            tags.contains_key(&tag)
        } else {
            false
        }
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        self.unread.latest()
    }

    fn is_unread(&self) -> bool {
        self.unread.is_unread()
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn is_invite(&self) -> bool {
        self.room().state() == MatrixRoomState::Invited
    }
}

impl Display for RoomItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl ListItem<IambInfo> for RoomItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let unread = self.unread.is_unread();
        let style = selected_style(selected);
        let (name, mut labels) = name_and_labels(&self.name, unread, style);
        let mut spans = vec![name];

        if let Some(tags) = &self.tags() {
            labels.extend(tags.keys().map(|t| tag_to_span(t, style)));
        }

        append_tags(labels, &mut spans, style);

        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        self.room_id().to_string().into()
    }
}

impl Readable for RoomItem {
    fn read_target(&self) -> ReadTarget {
        ReadTarget { room_id: self.room_id().to_owned(), thread: None }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for RoomItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        room_prompt(self.room_id(), act, ctx)
    }
}

#[derive(Clone)]
pub struct DirectItem {
    room_info: MatrixRoomInfo,
    name: String,
    alias: Option<OwnedRoomAliasId>,
    unread: UnreadInfo,
}

impl DirectItem {
    fn new(room_info: MatrixRoomInfo, store: &mut ProgramStore) -> Self {
        let room_id = room_info.0.room_id().to_owned();
        let alias = room_info.0.canonical_alias();

        let info = store.application.rooms.get_or_default(room_id);
        let name = info.name.clone().unwrap_or_default();
        let unread = info.unreads(room_info.0.is_marked_unread(), &store.application.settings);
        info.tags.clone_from(&room_info.deref().1);

        DirectItem { room_info, name, alias, unread }
    }

    #[inline]
    fn room(&self) -> &MatrixRoom {
        &self.room_info.deref().0
    }

    #[inline]
    fn tags(&self) -> &Option<Tags> {
        &self.room_info.deref().1
    }
}

impl RoomLikeItem for DirectItem {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        self.alias.as_deref()
    }

    fn has_tag(&self, tag: TagName) -> bool {
        if let Some(tags) = &self.room_info.deref().1 {
            tags.contains_key(&tag)
        } else {
            false
        }
    }

    fn room_id(&self) -> &RoomId {
        self.room().room_id()
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        self.unread.latest()
    }

    fn is_unread(&self) -> bool {
        self.unread.is_unread()
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn is_invite(&self) -> bool {
        self.room().state() == MatrixRoomState::Invited
    }
}

impl Display for DirectItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, ":verify request {}", self.name)
    }
}

impl ListItem<IambInfo> for DirectItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let unread = self.unread.is_unread();
        let style = selected_style(selected);
        let (name, mut labels) = name_and_labels(&self.name, unread, style);
        let mut spans = vec![name];

        if let Some(tags) = &self.tags() {
            labels.extend(tags.keys().map(|t| tag_to_span(t, style)));
        }

        append_tags(labels, &mut spans, style);

        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        self.room_id().to_string().into()
    }
}

impl Readable for DirectItem {
    fn read_target(&self) -> ReadTarget {
        ReadTarget { room_id: self.room_id().to_owned(), thread: None }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for DirectItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        room_prompt(self.room_id(), act, ctx)
    }
}

#[derive(Clone)]
pub struct SpaceItem {
    room_info: MatrixRoomInfo,
    name: String,
    alias: Option<OwnedRoomAliasId>,
}

impl SpaceItem {
    fn new(room_info: MatrixRoomInfo, store: &mut ProgramStore) -> Self {
        let room_id = room_info.0.room_id();
        let name = store
            .application
            .get_room_info(room_id.to_owned())
            .name
            .clone()
            .unwrap_or_default();
        let alias = room_info.0.canonical_alias();

        if let Some(alias) = &alias {
            store.application.names.insert(alias.to_string(), room_id.to_owned());
        }

        SpaceItem { room_info, name, alias }
    }

    #[inline]
    fn room(&self) -> &MatrixRoom {
        &self.room_info.deref().0
    }
}

impl RoomLikeItem for SpaceItem {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn room_id(&self) -> &RoomId {
        self.room().room_id()
    }

    fn alias(&self) -> Option<&RoomAliasId> {
        self.alias.as_deref()
    }

    fn has_tag(&self, _: TagName) -> bool {
        // I think that spaces can technically have tags, but afaik no client
        // exposes them, so we'll just always return false here for now.
        false
    }

    fn recent_ts(&self) -> Option<&MessageTimeStamp> {
        // XXX: this needs to determine the room with most recent message and return its timestamp.
        None
    }

    fn is_unread(&self) -> bool {
        // XXX: this needs to check whether the space contains rooms with unread messages
        false
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn is_invite(&self) -> bool {
        self.room().state() == MatrixRoomState::Invited
    }
}

impl Display for SpaceItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl ListItem<IambInfo> for SpaceItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        selected_text(self.name.as_str(), selected)
    }

    fn get_word(&self) -> Option<String> {
        self.room_id().to_string().into()
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for SpaceItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        room_prompt(self.room_id(), act, ctx)
    }
}

#[derive(Clone)]
pub struct VerifyItem {
    user_dev: String,
    sasv1: SasVerification,
}

impl VerifyItem {
    fn new(user_dev: String, sasv1: SasVerification) -> Self {
        VerifyItem { user_dev, sasv1 }
    }

    fn show_item(&self) -> String {
        let state = if self.sasv1.is_done() {
            "done"
        } else if self.sasv1.is_cancelled() {
            "cancelled"
        } else if self.sasv1.emoji().is_some() {
            "accepted"
        } else {
            "not accepted"
        };

        if self.sasv1.is_self_verification() {
            let device = self.sasv1.other_device();

            if let Some(display_name) = device.display_name() {
                format!("Device verification with {display_name} ({state})")
            } else {
                format!("Device verification with device {} ({})", device.device_id(), state)
            }
        } else {
            format!("User Verification with {} ({})", self.sasv1.other_user_id(), state)
        }
    }
}

impl PartialEq for VerifyItem {
    fn eq(&self, other: &Self) -> bool {
        self.user_dev == other.user_dev
    }
}

impl Eq for VerifyItem {}

impl Ord for VerifyItem {
    fn cmp(&self, other: &Self) -> Ordering {
        fn state_val(sas: &SasVerification) -> usize {
            if sas.is_done() {
                return 3;
            } else if sas.is_cancelled() {
                return 2;
            } else {
                return 1;
            }
        }

        fn device_val(sas: &SasVerification) -> usize {
            if sas.is_self_verification() {
                return 1;
            } else {
                return 2;
            }
        }

        let state1 = state_val(&self.sasv1);
        let state2 = state_val(&other.sasv1);

        let dev1 = device_val(&self.sasv1);
        let dev2 = device_val(&other.sasv1);

        let scmp = state1.cmp(&state2);
        let dcmp = dev1.cmp(&dev2);

        scmp.then(dcmp).then_with(|| {
            let did1 = self.sasv1.other_device().device_id();
            let did2 = other.sasv1.other_device().device_id();

            did1.cmp(did2)
        })
    }
}

impl PartialOrd for VerifyItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<(&String, &SasVerification)> for VerifyItem {
    fn from((user_dev, sasv1): (&String, &SasVerification)) -> Self {
        VerifyItem::new(user_dev.clone(), sasv1.clone())
    }
}

impl Display for VerifyItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.sasv1.is_done() {
            return Ok(());
        }

        if self.sasv1.is_cancelled() {
            write!(f, ":verify request {}", self.sasv1.other_user_id())
        } else if self.sasv1.emoji().is_some() {
            write!(f, ":verify confirm {}", self.user_dev)
        } else {
            write!(f, ":verify accept {}", self.user_dev)
        }
    }
}

impl ListItem<IambInfo> for VerifyItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let mut lines = vec![];

        let bold = Style::default().add_modifier(StyleModifier::BOLD);
        let item = Span::styled(self.show_item(), selected_style(selected));
        lines.push(Line::from(item));

        if self.sasv1.is_done() {
            // Print nothing.
        } else if self.sasv1.is_cancelled() {
            if let Some(info) = self.sasv1.cancel_info() {
                lines.push(Line::from(format!("    Cancelled: {}", info.reason())));
                lines.push(Line::from(""));
            }

            lines.push(Line::from("    You can start a new verification request with:"));
        } else if let Some(emoji) = self.sasv1.emoji() {
            lines.push(Line::from(
                "    Both devices should see the following Emoji sequence:".to_string(),
            ));
            lines.push(Line::from(""));

            for line in format_emojis(emoji).lines() {
                lines.push(Line::from(format!("    {line}")));
            }

            lines.push(Line::from(""));
            lines.push(Line::from("    If they don't match, run:"));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(":verify mismatch {}", self.user_dev),
                bold,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("    If everything looks right, you can confirm with:"));
        } else {
            lines.push(Line::from("    To accept this request, run:"));
        }

        let cmd = self.to_string();

        if !cmd.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::from("        "), Span::styled(cmd, bold)]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::from("You can copy the above command with "),
                Span::styled("yy", bold),
                Span::from(" and then execute it with "),
                Span::styled("@\"", bold),
            ]));
        }

        Text::from(lines)
    }

    fn get_word(&self) -> Option<String> {
        None
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for VerifyItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => Ok(vec![]),
            PromptAction::Abort(_) => {
                let msg = "Cannot abort entry inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
            PromptAction::Recall(..) => {
                let msg = "Cannot recall history inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
        }
    }
}

#[derive(Clone)]
pub struct MemberItem {
    member: RoomMember,
    room_id: OwnedRoomId,
}

impl MemberItem {
    fn new(member: RoomMember, room_id: OwnedRoomId) -> Self {
        Self { member, room_id }
    }
}

impl Display for MemberItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.member.user_id())
    }
}

impl ListItem<IambInfo> for MemberItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        store: &mut ProgramStore,
    ) -> Text<'_> {
        let info = store.application.rooms.get_or_default(self.room_id.clone());
        let user_id = self.member.user_id();

        let (color, name) = store.application.settings.get_user_overrides(self.member.user_id());
        let color = color.unwrap_or_else(|| super::config::user_color(user_id.as_str()));
        let mut style = super::config::user_style_from_color(color);

        if selected {
            style = style.add_modifier(StyleModifier::REVERSED);
        }

        let mut spans = vec![];
        let mut parens = false;

        if let Some(name) = name {
            spans.push(Span::styled(name, style));
            parens = true;
        } else if let Some(display) = info.display_names.get(user_id) {
            spans.push(Span::styled(display.into_owned(), style));
            parens = true;
        }

        spans.extend(parens.then_some(Span::styled(" (", style)));
        spans.push(Span::styled(user_id.as_str(), style));
        spans.extend(parens.then_some(Span::styled(")", style)));

        let state = match self.member.membership() {
            MembershipState::Ban => Span::raw(" (banned)").into(),
            MembershipState::Invite => Span::raw(" (invited)").into(),
            MembershipState::Knock => Span::raw(" (wants to join)").into(),
            MembershipState::Leave => Span::raw(" (left)").into(),
            MembershipState::Join => None,
            _ => None,
        };

        spans.extend(state);

        return Line::from(spans).into();
    }

    fn get_word(&self) -> Option<String> {
        self.member.user_id().to_string().into()
    }

    fn matches(&self, needle: &regex::Regex) -> bool {
        needle.is_match(self.member.name()) || needle.is_match(self.member.user_id().as_str())
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for MemberItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        _: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => Ok(vec![]),
            PromptAction::Abort(_) => {
                let msg = "Cannot abort entry inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
            PromptAction::Recall(..) => {
                let msg = "Cannot recall history inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{mock_store, TEST_ROOM1_ID};
    use matrix_sdk::ruma::{room_alias_id, server_name};

    /// Opening a window has to leave its list populated, not empty until the first redraw.
    ///
    /// A macro like `:unreadsandthreads<Enter>G<Enter>` has all of its keys consumed in one batch
    /// before anything is drawn, so `G` and `<Enter>` would otherwise be acting on an empty list
    /// and failing with "No item currently selected".
    ///
    /// The switcher is what this can be checked with: its entries include the windows that iamb's
    /// commands open, which are there regardless of whether a sync has happened. The room lists
    /// need a real Matrix client to have anything in them, so they cannot be checked here.
    #[tokio::test]
    async fn test_opening_a_window_fills_in_its_list() {
        let mut store = mock_store().await;

        let IambWindow::QuickSwitcher(switcher) =
            IambWindow::open(IambId::QuickSwitcher, &mut store).unwrap()
        else {
            panic!("opening iamb://switch should give back a switcher");
        };

        assert!(switcher.len() > 0, "the switcher should have entries before it is ever drawn");
    }

    /// Everything the title says, as one string.
    fn title_of(win: &mut IambWindow, store: &mut ProgramStore) -> String {
        win.get_win_title(store)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[tokio::test]
    async fn test_a_list_title_reports_how_much_the_list_holds() {
        let mut store = mock_store().await;
        let mut win = IambWindow::open(IambId::QuickSwitcher, &mut store).unwrap();

        let IambWindow::QuickSwitcher(switcher) = &win else {
            panic!("opening iamb://switch should give back a switcher");
        };
        let entries = switcher.len();

        assert_eq!(title_of(&mut win, &mut store), format!("Jump to [{entries}]"));
    }

    /// A count of a filtered list must not read as the size of the whole list.
    #[tokio::test]
    async fn test_a_filtered_list_says_that_its_count_leaves_entries_out() {
        let mut store = mock_store().await;
        let mut win = IambWindow::open(IambId::QuickSwitcher, &mut store).unwrap();

        let IambWindow::QuickSwitcher(switcher) = &mut win else {
            panic!("opening iamb://switch should give back a switcher");
        };
        switcher.set_filter("rooms");
        win.refresh(&mut store);

        let IambWindow::QuickSwitcher(switcher) = &win else {
            unreachable!("the window does not change kind");
        };
        let entries = switcher.len();
        let title = title_of(&mut win, &mut store);

        assert_eq!(title, format!("Jump to [{entries}] (filtered)"));
        assert!(entries > 0, "the filter should still match something");
    }

    #[tokio::test]
    async fn test_the_counts_can_be_turned_off() {
        let mut store = mock_store().await;
        store.application.settings.tunables.list_counts = false;

        let mut win = IambWindow::open(IambId::QuickSwitcher, &mut store).unwrap();

        assert_eq!(title_of(&mut win, &mut store), "Jump to");
    }

    #[test]
    fn test_a_mixed_list_reports_the_unread_entries_and_the_total() {
        let counts = ListCounts { total: 450, unread: 12, filtered: false };
        let title = counted_title("Rooms", Counted::UnreadAndTotal, Some(counts));

        assert_eq!(title.to_string(), "Rooms [12 unread / 450]");
    }

    /// A "0 unread" that is there most of the time teaches the user to skip the brackets.
    #[test]
    fn test_a_mixed_list_with_nothing_unread_reports_only_the_total() {
        let counts = ListCounts { total: 450, unread: 0, filtered: false };
        let title = counted_title("Rooms", Counted::UnreadAndTotal, Some(counts));

        assert_eq!(title.to_string(), "Rooms [450]");
    }

    /// Everything in an inbox is unread, so a second number would say the same thing twice.
    #[test]
    fn test_an_inbox_reports_one_number() {
        let counts = ListCounts { total: 47, unread: 47, filtered: false };
        let title = counted_title("Unread Rooms & Threads", Counted::Total, Some(counts));

        assert_eq!(title.to_string(), "Unread Rooms & Threads [47]");
    }

    #[tokio::test]
    async fn test_opening_a_window_that_has_nothing_to_fill_in() {
        let mut store = mock_store().await;

        // Nothing has synced, so these are empty rather than broken, and opening them still works.
        for id in [
            IambId::RoomList,
            IambId::DirectList,
            IambId::UnreadThreadList,
            IambId::MentionList,
        ] {
            assert!(IambWindow::open(id, &mut store).is_ok());
        }
    }

    /// An entry as the `:unreadmentions` filter sees it.
    struct TestMentionItem {
        unread: bool,
        deferred: bool,
        is_dm: bool,
        mentions: u64,
    }

    impl TestMentionItem {
        /// An unread room with traffic that names nobody.
        fn unread_room() -> Self {
            TestMentionItem {
                unread: true,
                deferred: false,
                is_dm: false,
                mentions: 0,
            }
        }
    }

    impl AddressedItem for TestMentionItem {
        fn is_addressed_to_the_user(&self) -> bool {
            self.is_dm || self.mentions > 0
        }
    }

    impl RoomLikeItem for TestMentionItem {
        fn is_unread(&self) -> bool {
            self.unread
        }

        fn is_deferred(&self) -> bool {
            self.deferred
        }

        fn room_id(&self) -> &RoomId {
            TEST_ROOM1_ID.as_ref()
        }

        fn name(&self) -> &str {
            "test"
        }

        fn alias(&self) -> Option<&RoomAliasId> {
            None
        }

        fn has_tag(&self, _: TagName) -> bool {
            false
        }

        fn recent_ts(&self) -> Option<&MessageTimeStamp> {
            None
        }

        fn is_invite(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_an_unread_room_that_mentions_the_user_is_kept() {
        let item = TestMentionItem { mentions: 1, ..TestMentionItem::unread_room() };

        assert!(is_unread_mention(&item));
    }

    #[test]
    fn test_an_unread_dm_is_kept() {
        let item = TestMentionItem { is_dm: true, ..TestMentionItem::unread_room() };

        assert!(is_unread_mention(&item));
    }

    /// This is the whole point of the window: it is narrower than `:unreads`.
    #[test]
    fn test_an_unread_room_that_mentions_nobody_is_left_out() {
        assert!(!is_unread_mention(&TestMentionItem::unread_room()));
    }

    /// A mention the user postponed has to stay postponed, or the snooze is worth nothing here.
    #[test]
    fn test_a_snoozed_mention_is_left_out() {
        let item = TestMentionItem {
            mentions: 1,
            deferred: true,
            ..TestMentionItem::unread_room()
        };

        assert!(!is_unread_mention(&item));
    }

    /// Nothing that is already read belongs in an inbox, however directly it named the user.
    #[test]
    fn test_a_read_mention_is_left_out() {
        let item = TestMentionItem {
            mentions: 1,
            unread: false,
            ..TestMentionItem::unread_room()
        };

        assert!(!is_unread_mention(&item));
    }

    #[derive(Debug, Eq, PartialEq)]
    struct TestRoomItem {
        room_id: OwnedRoomId,
        tags: Vec<TagName>,
        alias: Option<OwnedRoomAliasId>,
        name: &'static str,
        unread: UnreadInfo,
        invite: bool,
    }

    impl RoomLikeItem for &TestRoomItem {
        fn room_id(&self) -> &RoomId {
            self.room_id.as_ref()
        }

        fn is_deferred(&self) -> bool {
            false
        }

        fn has_tag(&self, tag: TagName) -> bool {
            self.tags.contains(&tag)
        }

        fn alias(&self) -> Option<&RoomAliasId> {
            self.alias.as_deref()
        }

        fn name(&self) -> &str {
            self.name
        }

        fn recent_ts(&self) -> Option<&MessageTimeStamp> {
            self.unread.latest()
        }

        fn is_unread(&self) -> bool {
            self.unread.is_unread()
        }

        fn is_invite(&self) -> bool {
            self.invite
        }
    }

    #[test]
    fn test_sort_rooms() {
        let mut collator = Collator::default();
        let collator = &mut collator;
        let server = server_name!("example.com");

        let room1 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![TagName::Favorite],
            alias: Some(room_alias_id!("#room1:example.com").to_owned()),
            name: "Z",
            unread: UnreadInfo::default(),
            invite: false,
        };

        let room2 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: Some(room_alias_id!("#a:example.com").to_owned()),
            name: "Unnamed Room",
            unread: UnreadInfo::default(),
            invite: false,
        };

        let room3 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Cool Room",
            unread: UnreadInfo::default(),
            invite: false,
        };

        // Sort by Name ascending.
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[SortColumn(SortFieldRoom::Name, SortOrder::Ascending)];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room3, &room2, &room1]);

        // Sort by Name descending.
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[SortColumn(SortFieldRoom::Name, SortOrder::Descending)];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room1, &room2, &room3]);

        // Sort by Favorite and Alias before Name to show order matters.
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[
            SortColumn(SortFieldRoom::Favorite, SortOrder::Ascending),
            SortColumn(SortFieldRoom::Alias, SortOrder::Ascending),
            SortColumn(SortFieldRoom::Name, SortOrder::Ascending),
        ];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room1, &room2, &room3]);

        // Now flip order of Favorite with Descending
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[
            SortColumn(SortFieldRoom::Favorite, SortOrder::Descending),
            SortColumn(SortFieldRoom::Alias, SortOrder::Ascending),
            SortColumn(SortFieldRoom::Name, SortOrder::Ascending),
        ];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room2, &room3, &room1]);
    }

    #[test]
    fn test_sort_room_recents() {
        let mut collator = Collator::default();
        let collator = &mut collator;
        let server = server_name!("example.com");

        let room1 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Room 1",
            unread: UnreadInfo { unread: false, latest: None },
            invite: false,
        };

        let room2 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Room 2",
            unread: UnreadInfo {
                unread: false,
                latest: Some(MessageTimeStamp::OriginServer(40u32.into())),
            },
            invite: false,
        };

        let room3 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Room 3",
            unread: UnreadInfo {
                unread: false,
                latest: Some(MessageTimeStamp::OriginServer(20u32.into())),
            },
            invite: false,
        };

        // Sort by Recent ascending.
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[SortColumn(SortFieldRoom::Recent, SortOrder::Ascending)];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room2, &room3, &room1]);

        // Sort by Recent descending.
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[SortColumn(SortFieldRoom::Recent, SortOrder::Descending)];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room1, &room3, &room2]);
    }

    #[test]
    fn test_sort_room_invites() {
        let mut collator = Collator::default();
        let collator = &mut collator;
        let server = server_name!("example.com");

        let room1 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Old room 1",
            unread: UnreadInfo::default(),
            invite: false,
        };

        let room2 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "Old room 2",
            unread: UnreadInfo::default(),
            invite: false,
        };

        let room3 = TestRoomItem {
            room_id: RoomId::new_v1(server).to_owned(),
            tags: vec![],
            alias: None,
            name: "New Fancy Room",
            unread: UnreadInfo::default(),
            invite: true,
        };

        // Sort invites first
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[
            SortColumn(SortFieldRoom::Invite, SortOrder::Ascending),
            SortColumn(SortFieldRoom::Name, SortOrder::Ascending),
        ];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room3, &room1, &room2]);

        // Sort invites after
        let mut rooms = vec![&room1, &room2, &room3];
        let fields = &[
            SortColumn(SortFieldRoom::Invite, SortOrder::Descending),
            SortColumn(SortFieldRoom::Name, SortOrder::Ascending),
        ];
        rooms.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));
        assert_eq!(rooms, vec![&room1, &room2, &room3]);
    }
}
