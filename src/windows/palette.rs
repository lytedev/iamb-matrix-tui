//! # Command Palette
//!
//! The `:commands` window lists iamb's own commands together with the keys bound to them, so that
//! keybindings can be discovered and remembered without leaving the client.
//!
//! Both halves of what it shows come from tables that also drive the real thing:
//! [IAMB_COMMANDS][crate::commands::IAMB_COMMANDS] is what registers the commands, and
//! [IAMB_BINDINGS][crate::keybindings::IAMB_BINDINGS] is what installs the keys. Nothing here is a
//! second copy that could drift.
//!
//! Scope: only iamb's own commands and bindings appear. The Vim motions and operators that come
//! from modalkit are not listed, and remain undiscoverable through the palette.
use std::fmt::{self, Display};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
    widgets::StatefulWidget,
};

use modalkit::{
    actions::{
        EditAction,
        Editable,
        EditorAction,
        Jumpable,
        MacroAction,
        PromptAction,
        Promptable,
        Scrollable,
    },
    editing::completion::CompletionList,
    editing::context::Resolve,
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::{
    list::{List, ListCursor, ListItem, ListState},
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
use crate::commands::IAMB_COMMANDS;
use crate::keybindings::{keys_for_command, IAMB_BINDINGS};

/// How many rows at the top of the window the filter bar takes up.
const FILTER_BAR_HEIGHT: u16 = 1;

/// The text shown in front of whatever has been typed to filter the palette.
const FILTER_BAR_PROMPT: &str = "filter: ";

/// The width the `:command` column is padded out to, so that descriptions line up.
const NAME_COLUMN_WIDTH: usize = 22;

/// The width the key column is padded out to.
const KEYS_COLUMN_WIDTH: usize = 12;

pub type CommandListState = ListState<PaletteItem, IambInfo>;

/// One row in the palette: something the user can run, and how to run it.
#[derive(Clone)]
pub struct PaletteItem {
    /// The command with its arguments (`:read [all]`), or the keys for a binding that has no
    /// command of its own.
    label: String,

    /// The key sequence bound to this, if there is one.
    keys: Option<&'static str>,

    /// What it does.
    description: &'static str,

    /// The keys to feed back into the keymap to run it.
    ///
    /// A command that takes arguments cannot just be run, so it is typed into the command bar and
    /// left there for the user to finish; anything else is submitted outright.
    run: String,
}

impl PaletteItem {
    /// Every command iamb registers, paired up with the key bound to it where there is one,
    /// followed by iamb's bindings that have no command.
    fn all() -> Vec<PaletteItem> {
        let commands = IAMB_COMMANDS.iter().map(|cmd| {
            let label = match cmd.args {
                Some(args) => format!(":{} {args}", cmd.name),
                None => format!(":{}", cmd.name),
            };
            let run = match cmd.args {
                Some(_) => format!(":{} ", cmd.name),
                None => format!(":{}<Enter>", cmd.name),
            };

            PaletteItem {
                label,
                keys: keys_for_command(cmd.name),
                description: cmd.description,
                run,
            }
        });

        let keys_only = IAMB_BINDINGS.iter().filter(|b| b.command.is_none()).map(|binding| {
            PaletteItem {
                label: binding.display_keys().to_string(),
                keys: Some(binding.display_keys()),
                description: binding.description,
                run: binding.display_keys().to_string(),
            }
        });

        commands.chain(keys_only).collect()
    }

    /// Whether this entry should be shown when the user has typed `needle`.
    fn matches(&self, needle: &str) -> bool {
        self.label.to_lowercase().contains(needle) ||
            self.description.to_lowercase().contains(needle) ||
            self.keys.is_some_and(|keys| keys.to_lowercase().contains(needle))
    }
}

impl Display for PaletteItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl ListItem<IambInfo> for PaletteItem {
    fn show(
        &self,
        selected: bool,
        _: &ViewportContext<ListCursor>,
        _: &mut ProgramStore,
    ) -> Text<'_> {
        let style = if selected {
            Style::default().add_modifier(StyleModifier::REVERSED)
        } else {
            Style::default()
        };

        let label = format!("{:NAME_COLUMN_WIDTH$} ", self.label);
        let keys = format!("{:KEYS_COLUMN_WIDTH$} ", self.keys.unwrap_or(""));

        let spans = vec![
            Span::styled(label, style.add_modifier(StyleModifier::BOLD)),
            Span::styled(keys, style),
            Span::styled(self.description, style),
        ];

        Text::from(Line::from(spans))
    }

    fn get_word(&self) -> Option<String> {
        Some(self.label.clone())
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for PaletteItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                let run = MacroAction::Run(self.run.clone(), Count::Exact(1));

                Ok(vec![(run.into(), ctx.clone())])
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

/// State for the `:commands` window.
pub struct CommandPaletteState {
    /// What the user has typed to narrow the list down.
    filter: TextBoxState<IambInfo>,

    /// The commands matching the filter.
    list: CommandListState,

    /// The filter text the list was last rebuilt for.
    filtered_by: String,
}

impl CommandPaletteState {
    pub fn new(store: &mut ProgramStore) -> Self {
        let buffer = store.load_buffer(IambBufferId::CommandPaletteFilter);
        let filter = TextBoxState::new(buffer);
        let list = CommandListState::new(IambBufferId::CommandPaletteList, PaletteItem::all());

        CommandPaletteState { filter, list, filtered_by: String::new() }
    }

    /// Rebuild the list if what the user typed has changed since it was last built.
    fn refilter(&mut self) {
        let needle = self.filter.get().to_string().trim().to_lowercase();

        if needle == self.filtered_by {
            return;
        }

        let items = PaletteItem::all()
            .into_iter()
            .filter(|item| item.matches(&needle))
            .collect::<Vec<_>>();

        self.list.set(items);
        self.filtered_by = needle;
    }
}

/// Whether an editing action should move through the list rather than edit the filter text.
///
/// Vertical movement is how a palette is navigated, and there is nothing vertical to do in a
/// one-line filter bar, so line motions belong to the list and everything else to the text box.
fn moves_through_list(act: &EditorAction, ctx: &ProgramContext) -> bool {
    let EditorAction::Edit(action, EditTarget::Motion(motion, _)) = act else {
        return false;
    };

    if !matches!(ctx.resolve(action), EditAction::Motion) {
        return false;
    }

    matches!(motion, MoveType::Line(_) | MoveType::BufferPos(_) | MoveType::ViewportPos(_))
}

impl Editable<ProgramContext, ProgramStore, IambInfo> for CommandPaletteState {
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
        self.refilter();

        res
    }
}

impl Jumpable<ProgramContext, IambInfo> for CommandPaletteState {
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

impl Scrollable<ProgramContext, ProgramStore, IambInfo> for CommandPaletteState {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        self.list.scroll(style, ctx, store)
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for CommandPaletteState {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        self.list.prompt(act, ctx, store)
    }
}

impl TerminalCursor for CommandPaletteState {
    fn get_term_cursor(&self) -> Option<TermOffset> {
        self.filter.get_term_cursor()
    }
}

impl WindowOps<IambInfo> for CommandPaletteState {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        self.refilter();

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
            .empty_message("No commands match that filter")
            .focus(focused)
            .render(below, buf, &mut self.list);
    }

    fn dup(&self, store: &mut ProgramStore) -> Self {
        CommandPaletteState {
            filter: self.filter.dup(store),
            list: self.list.dup(store),
            filtered_by: self.filtered_by.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use modalkit::key::TerminalKey;
    use modalkit::keybindings::InputKey;

    fn find(label: &str) -> PaletteItem {
        let found = PaletteItem::all().into_iter().find(|item| item.label == label);

        assert!(found.is_some(), "{:?} should be listed in the palette", label);

        found.unwrap()
    }

    #[test]
    fn test_everything_is_listed() {
        let items = PaletteItem::all();
        let keys_only = IAMB_BINDINGS.iter().filter(|b| b.command.is_none()).count();

        assert_eq!(items.len(), IAMB_COMMANDS.len() + keys_only);
    }

    #[test]
    fn test_bound_keys_are_shown() {
        assert_eq!(find(":read [all]").keys, Some("<C-W>r"));
        assert_eq!(find(":rooms").keys, None);
    }

    #[test]
    fn test_bindings_without_a_command_are_listed() {
        let zoom = find("<C-W>z");

        assert_eq!(zoom.keys, Some("<C-W>z"));
        assert_eq!(zoom.run, "<C-W>z");
    }

    #[test]
    fn test_filtering() {
        let items = PaletteItem::all();
        let matched = |needle: &str| items.iter().filter(|item| item.matches(needle)).count();

        assert!(matched("read") > 0);
        assert_eq!(matched("nothing here matches this"), 0);

        // Descriptions and keys are searched too, not just names.
        assert!(find(":dms").matches("direct"));
        assert!(find(":read [all]").matches("<c-w>r"));
    }

    #[test]
    fn test_run_keys_parse() {
        for item in PaletteItem::all() {
            let run = item.run;

            assert!(
                TerminalKey::from_macro_str(&run).is_ok(),
                "{:?} should parse as a key sequence",
                run
            );
        }
    }

    #[test]
    fn test_commands_taking_arguments_are_left_for_the_user_to_finish() {
        assert_eq!(find(":read [all]").run, ":read ");
        assert_eq!(find(":rooms").run, ":rooms<Enter>");
    }
}
