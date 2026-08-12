//! # Filtered List Windows
//!
//! Some windows are the same shape: a one-line filter bar, and underneath it a list that narrows
//! down to whatever the user has typed. The [`:commands` palette][crate::windows::palette] and the
//! [quick switcher][crate::windows::switcher] are both that shape, and both live in
//! [FilteredListState].
//!
//! All that differs between them is what goes in the list, so that is the whole of
//! [FilteredItem]: given what the user typed, hand back the rows to show, best first. Everything
//! else -- routing keys between the filter bar and the list, drawing, and submitting the selected
//! row -- is shared.
//!
//! Rows can also come from something that only this one window knows, such as the results a
//! `:search` fetched. [FilteredItem::Context] is that: a value the window is opened with and
//! keeps, and that [FilteredItem::matching] is given every time it rebuilds. A window whose rows
//! all come from the store, as both of the above do, uses `()` and ignores it.
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};

use modalkit::{
    actions::{
        EditAction,
        Editable,
        EditorAction,
        Jumpable,
        PromptAction,
        Promptable,
        Scrollable,
    },
    editing::completion::CompletionList,
    editing::context::Resolve,
    errors::EditResult,
    prelude::*,
};

use modalkit_ratatui::{
    list::{List, ListItem, ListState},
    textbox::{TextBox, TextBoxState},
    TermOffset,
    TerminalCursor,
    WindowOps,
};

use crate::base::{
    IambBufferId,
    IambInfo,
    IambResult,
    ProgramAction,
    ProgramContext,
    ProgramStore,
};

/// How many rows at the top of the window the filter bar takes up.
const FILTER_BAR_HEIGHT: u16 = 1;

/// The text shown in front of whatever has been typed to filter the list.
const FILTER_BAR_PROMPT: &str = "filter: ";

/// One kind of row that a [FilteredListState] can show.
pub trait FilteredItem:
    ListItem<IambInfo> + Promptable<ProgramContext, ProgramStore, IambInfo> + Clone
{
    /// Whatever the rows come from that the store does not already hold.
    ///
    /// It is cloned when the window is split, so each half keeps its own.
    type Context: Clone;

    /// The buffer that backs the filter bar.
    fn filter_buffer() -> IambBufferId;

    /// The buffer that backs the list.
    fn list_buffer() -> IambBufferId;

    /// Every row to show for what the user has typed, in the order to show them.
    ///
    /// `needle` has had surrounding whitespace stripped, but is otherwise exactly what was typed;
    /// it is empty when nothing has been. Implementations decide for themselves what matching and
    /// ordering mean, and are responsible for putting the best row first, since that is the one
    /// that starts out selected.
    fn matching(context: &Self::Context, needle: &str, store: &mut ProgramStore) -> Vec<Self>;

    /// What to show in place of the list when nothing matches.
    fn empty_message() -> &'static str;
}

/// A filter bar with a list of [FilteredItem]s underneath it.
pub struct FilteredListState<T: FilteredItem> {
    /// What the user has typed to narrow the list down.
    filter: TextBoxState<IambInfo>,

    /// The rows matching the filter.
    list: ListState<T, IambInfo>,

    /// What the rows are built from, beyond the store and the filter.
    context: T::Context,
}

impl<T: FilteredItem> FilteredListState<T> {
    pub fn new(context: T::Context, store: &mut ProgramStore) -> Self {
        let buffer = store.load_buffer(T::filter_buffer());
        let filter = TextBoxState::new(buffer);
        let list = ListState::new(T::list_buffer(), vec![]);

        FilteredListState { filter, list, context }
    }

    /// What the rows were built from.
    ///
    /// The window title needs it: a title is drawn from the window and not from a row, and some
    /// of what the user has to be told about a result set is true of the set rather than of any
    /// row in it.
    pub fn context(&self) -> &T::Context {
        &self.context
    }

    /// How many rows the list currently holds.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Rebuild the list for what has been typed.
    ///
    /// This happens when the window is opened, on every edit, and on every draw, rather than only
    /// when the typed text changes. Opening is what lets a macro that opens the window and acts on
    /// it in one go find anything there; redrawing is what keeps rows that depend on more than the
    /// filter -- which rooms are unread, say -- up to date with the rest of the client.
    pub fn rebuild(&mut self, store: &mut ProgramStore) {
        let needle = self.filter.get().to_string().trim().to_string();

        self.list.set(T::matching(&self.context, &needle, store));
    }
}

/// Whether an editing action should move through the list rather than edit the filter text.
///
/// Vertical movement is how one of these windows is navigated, and there is nothing vertical to do
/// in a one-line filter bar, so line motions belong to the list and everything else to the text box.
fn moves_through_list(act: &EditorAction, ctx: &ProgramContext) -> bool {
    let EditorAction::Edit(action, EditTarget::Motion(motion, _)) = act else {
        return false;
    };

    if !matches!(ctx.resolve(action), EditAction::Motion) {
        return false;
    }

    matches!(motion, MoveType::Line(_) | MoveType::BufferPos(_) | MoveType::ViewportPos(_))
}

impl<T: FilteredItem> Editable<ProgramContext, ProgramStore, IambInfo> for FilteredListState<T> {
    fn editor_command(
        &mut self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        if moves_through_list(act, ctx) {
            return self.list.editor_command(act, ctx, store);
        }

        let res = self.filter.editor_command(act, ctx, store);
        self.rebuild(store);

        res
    }
}

impl<T: FilteredItem> Jumpable<ProgramContext, IambInfo> for FilteredListState<T> {
    fn jump(
        &mut self,
        list: PositionList,
        dir: MoveDir1D,
        count: usize,
        ctx: &ProgramContext,
    ) -> IambResult<usize> {
        self.list.jump(list, dir, count, ctx)
    }
}

impl<T: FilteredItem> Scrollable<ProgramContext, ProgramStore, IambInfo> for FilteredListState<T> {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        self.list.scroll(style, ctx, store)
    }
}

impl<T: FilteredItem> Promptable<ProgramContext, ProgramStore, IambInfo> for FilteredListState<T> {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        self.list.prompt(act, ctx, store)
    }
}

impl<T: FilteredItem> TerminalCursor for FilteredListState<T> {
    fn get_term_cursor(&self) -> Option<TermOffset> {
        self.filter.get_term_cursor()
    }
}

impl<T: FilteredItem> WindowOps<IambInfo> for FilteredListState<T> {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        self.rebuild(store);

        let prompt_width = FILTER_BAR_PROMPT.len() as u16;
        let bar = Rect { height: area.height.min(FILTER_BAR_HEIGHT), ..area };
        let text = Rect {
            x: bar.x.saturating_add(prompt_width),
            width: bar.width.saturating_sub(prompt_width),
            ..bar
        };
        let below = Rect {
            y: area.y.saturating_add(bar.height),
            height: area.height.saturating_sub(bar.height),
            ..area
        };

        buf.set_string(bar.x, bar.y, FILTER_BAR_PROMPT, Style::default());
        TextBox::new().render(text, buf, &mut self.filter);

        List::new(store)
            .empty_message(T::empty_message())
            .focus(focused)
            .render(below, buf, &mut self.list);
    }

    fn dup(&self, store: &mut ProgramStore) -> Self {
        FilteredListState {
            filter: self.filter.dup(store),
            list: self.list.dup(store),
            context: self.context.clone(),
        }
    }

    fn close(&mut self, flags: CloseFlags, store: &mut ProgramStore) -> bool {
        self.list.close(flags, store)
    }

    fn write(
        &mut self,
        _: Option<&str>,
        _: WriteFlags,
        _: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        Ok(None)
    }

    fn get_completions(&self) -> Option<CompletionList> {
        self.filter.get_completions()
    }

    fn get_cursor_word(&self, style: &WordStyle) -> Option<String> {
        self.list.get_cursor_word(style)
    }

    fn get_selected_word(&self) -> Option<String> {
        self.list.get_selected_word()
    }
}
