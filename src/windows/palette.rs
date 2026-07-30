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
    style::{Modifier as StyleModifier, Style},
    text::{Line, Span, Text},
};

use modalkit::{
    actions::{MacroAction, PromptAction, Promptable},
    errors::{EditError, EditResult},
    prelude::*,
};

use modalkit_ratatui::list::{ListCursor, ListItem};

use crate::base::{IambBufferId, IambInfo, ProgramAction, ProgramContext, ProgramStore};
use crate::commands::{CommandForm, IAMB_COMMANDS};
use crate::config::Keys;
use crate::keybindings::{keys_for_command, IAMB_BINDINGS};
use crate::message::compose::SLASH_COMMANDS;
use crate::windows::filtered::{FilteredItem, FilteredListState};

/// The width the what-you-type column is padded out to, so that the rest lines up.
const LABEL_COLUMN_WIDTH: usize = 32;

/// The width the key column is padded out to.
const KEYS_COLUMN_WIDTH: usize = 12;

/// The width the description column is padded out to.
const DESCRIPTION_COLUMN_WIDTH: usize = 52;

/// The `:commands` window.
pub type CommandPaletteState = FilteredListState<PaletteItem>;

/// Where a palette row came from, which is also what its keys mean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaletteKind {
    /// A command typed in the command bar.
    Command,

    /// A keybinding that has no command of its own.
    Binding,

    /// A keybinding from the user's own configuration.
    UserMacro,

    /// Markup typed at the start of a message, which the palette can only describe.
    Slash,
}

impl PaletteKind {
    /// The label shown in the palette's rightmost column.
    fn label(&self) -> &'static str {
        match self {
            PaletteKind::Command => "command",
            PaletteKind::Binding => "key",
            PaletteKind::UserMacro => "your config",
            PaletteKind::Slash => "message",
        }
    }
}

/// One row in the palette.
#[derive(Clone)]
pub struct PaletteItem {
    /// What you type: the command with its arguments (`:room notify set <level>`), a key
    /// sequence, or a slash command.
    label: String,

    /// The key sequence bound to this, if there is one.
    keys: Option<String>,

    /// What it does.
    description: String,

    /// Where this row came from.
    kind: PaletteKind,

    /// The keys to feed back into the keymap to run it, if it can be run from here.
    ///
    /// A form that still needs an argument is typed into the command bar and left there for the
    /// user to finish rather than being submitted with a placeholder in it.
    run: Option<String>,
}

/// Everything from `<` or `[` onwards is for the user to fill in, so it cannot be typed for them.
const PLACEHOLDER_START: [char; 2] = ['<', '['];

/// Split what follows a command name into the part that can be typed and whether any of it was a
/// placeholder the user still has to fill in.
fn literal_prefix(args: &str) -> (&str, bool) {
    match args.find(PLACEHOLDER_START) {
        Some(at) => (args[..at].trim_end(), true),
        None => (args, false),
    }
}

impl PaletteItem {
    /// Every row the palette shows, in the order it shows them.
    fn all(store: &ProgramStore) -> Vec<PaletteItem> {
        let mut items = PaletteItem::builtins();

        items.extend(PaletteItem::user_macros(store));

        items
    }

    /// The rows that come from iamb itself, independent of the user's configuration.
    fn builtins() -> Vec<PaletteItem> {
        let mut items = Vec::new();

        for cmd in IAMB_COMMANDS {
            for form in cmd.forms {
                items.push(PaletteItem::from_command(cmd.name, form));
            }
        }

        for binding in IAMB_BINDINGS.iter().filter(|b| b.command.is_none()) {
            let keys = binding.display_keys();

            items.push(PaletteItem {
                label: keys.to_string(),
                keys: Some(keys.to_string()),
                description: binding.description.to_string(),
                kind: PaletteKind::Binding,
                run: Some(keys.to_string()),
            });
        }

        for slash in SLASH_COMMANDS {
            let label = match slash.aliases {
                [] => slash.trigger.to_string(),
                aliases => format!("{} ({})", slash.trigger, aliases.join(", ")),
            };

            items.push(PaletteItem {
                label,
                keys: None,
                description: slash.description.to_string(),
                kind: PaletteKind::Slash,
                run: None,
            });
        }

        items
    }

    /// The rows that come from keybindings in the user's own configuration.
    fn user_macros(store: &ProgramStore) -> Vec<PaletteItem> {
        let mut items = Vec::new();

        for (modes, keys) in &store.application.settings.macros {
            for (Keys(_, input), Keys(_, run)) in keys {
                let modes = modes
                    .0
                    .iter()
                    .map(|mode| format!("{mode:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");

                items.push(PaletteItem {
                    label: input.clone(),
                    keys: Some(input.clone()),
                    description: format!("Your macro ({modes} mode): types {run}"),
                    kind: PaletteKind::UserMacro,
                    run: Some(run.clone()),
                });
            }
        }

        items
    }

    /// Build the row for one form of one command.
    fn from_command(name: &'static str, form: &'static CommandForm) -> PaletteItem {
        let (label, run) = match form.args {
            None => (format!(":{name}"), format!(":{name}<Enter>")),
            Some(args) => {
                let (literal, needs_input) = literal_prefix(args);
                let typed = format!(":{name} {literal}");
                let run = if needs_input {
                    // Leave the command bar open on the part the user still has to fill in.
                    format!("{}<Space>", typed.trim_end())
                } else {
                    format!("{typed}<Enter>")
                };

                (format!(":{name} {args}"), run)
            },
        };

        PaletteItem {
            label,
            keys: keys_for_command(name).map(str::to_string),
            description: form.description.to_string(),
            kind: PaletteKind::Command,
            run: Some(run),
        }
    }

    /// Whether this row should be shown when the user has typed `needle`.
    fn matches(&self, needle: &str) -> bool {
        self.label.to_lowercase().contains(needle) ||
            self.description.to_lowercase().contains(needle) ||
            self.keys.as_ref().is_some_and(|keys| keys.to_lowercase().contains(needle))
    }
}

impl Display for PaletteItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl FilteredItem for PaletteItem {
    fn filter_buffer() -> IambBufferId {
        IambBufferId::CommandPaletteFilter
    }

    fn list_buffer() -> IambBufferId {
        IambBufferId::CommandPaletteList
    }

    fn matching(needle: &str, store: &mut ProgramStore) -> Vec<PaletteItem> {
        let needle = needle.to_lowercase();

        PaletteItem::all(store)
            .into_iter()
            .filter(|item| item.matches(&needle))
            .collect()
    }

    fn empty_message() -> &'static str {
        "No commands match that filter"
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

        let label = format!("{:LABEL_COLUMN_WIDTH$} ", self.label);
        let keys = format!("{:KEYS_COLUMN_WIDTH$} ", self.keys.as_deref().unwrap_or(""));
        let description = format!("{:DESCRIPTION_COLUMN_WIDTH$} ", self.description);

        let spans = vec![
            Span::styled(label, style.add_modifier(StyleModifier::BOLD)),
            Span::styled(keys, style),
            Span::styled(description, style),
            Span::styled(self.kind.label(), style.add_modifier(StyleModifier::DIM)),
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
                let Some(run) = self.run.clone() else {
                    let msg = format!("{} is typed at the start of a message", self.label);

                    return Err(EditError::Failure(msg));
                };

                let run = MacroAction::Run(run, Count::Exact(1));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::setup_commands;
    use modalkit::key::TerminalKey;
    use modalkit::keybindings::InputKey;

    fn find(label: &str) -> PaletteItem {
        let found = PaletteItem::builtins().into_iter().find(|item| item.label == label);

        assert!(found.is_some(), "{:?} should be listed in the palette", label);

        found.unwrap()
    }

    #[test]
    fn test_every_command_form_is_listed() {
        let forms = IAMB_COMMANDS.iter().map(|cmd| cmd.forms.len()).sum::<usize>();
        let keys_only = IAMB_BINDINGS.iter().filter(|b| b.command.is_none()).count();
        let expected = forms + keys_only + SLASH_COMMANDS.len();

        assert_eq!(PaletteItem::builtins().len(), expected);
    }

    #[test]
    fn test_every_listed_command_is_registered() {
        let cmds = setup_commands();
        let registered = cmds.complete_name("");

        for cmd in IAMB_COMMANDS {
            assert!(
                registered.contains(&cmd.name.to_string()),
                "{:?} is listed in the palette but not registered",
                cmd.name
            );
        }
    }

    #[test]
    fn test_subcommands_are_listed_individually() {
        // The whole point of the rewrite: `:room` has many forms, not one.
        for label in [
            ":room notify set <level>",
            ":room notify show",
            ":room kick <user> <reason>",
            ":verify confirm <key>",
            ":invite accept",
            ":keys export <path> <passphrase>",
            ":read all",
            ":unreads clear",
        ] {
            find(label);
        }
    }

    #[test]
    fn test_bound_keys_are_shown() {
        assert_eq!(find(":read").keys.as_deref(), Some("<C-W>r"));
        assert_eq!(find(":rooms").keys, None);
    }

    #[test]
    fn test_bindings_without_a_command_are_listed() {
        let zoom = find("<C-W>z");

        assert_eq!(zoom.keys.as_deref(), Some("<C-W>z"));
        assert_eq!(zoom.run.as_deref(), Some("<C-W>z"));
    }

    #[test]
    fn test_slash_commands_are_listed_but_not_runnable() {
        let me = find("/me");

        assert_eq!(me.kind, PaletteKind::Slash);
        assert_eq!(me.run, None);
    }

    #[test]
    fn test_filtering() {
        let items = PaletteItem::builtins();
        let matched = |needle: &str| items.iter().filter(|item| item.matches(needle)).count();

        assert!(matched("read") > 0);
        assert_eq!(matched("nothing here matches this"), 0);

        // Descriptions and keys are searched too, not just names.
        assert!(find(":dms").matches("direct"));
        assert!(find(":read").matches("<c-w>r"));
    }

    #[test]
    fn test_run_keys_parse() {
        for item in PaletteItem::builtins() {
            let Some(run) = item.run else {
                continue;
            };

            assert!(
                TerminalKey::from_macro_str(&run).is_ok(),
                "{:?} should parse as a key sequence",
                run
            );
        }
    }

    #[test]
    fn test_forms_are_typed_up_to_the_first_placeholder() {
        // Complete on its own, so it gets submitted.
        assert_eq!(find(":room notify show").run.as_deref(), Some(":room notify show<Enter>"));

        // Still needs a level, so the literal part is typed and the bar left open.
        let notify_set = find(":room notify set <level>");

        assert_eq!(notify_set.run.as_deref(), Some(":room notify set<Space>"));

        // Nothing literal to type beyond the command name.
        assert_eq!(find(":join <room>").run.as_deref(), Some(":join<Space>"));
    }

    #[test]
    fn test_literal_prefix() {
        assert_eq!(literal_prefix("notify show"), ("notify show", false));
        assert_eq!(literal_prefix("notify set <level>"), ("notify set", true));
        assert_eq!(literal_prefix("<room>"), ("", true));
        assert_eq!(literal_prefix("[all]"), ("", true));
    }
}
