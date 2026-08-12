//! # Default Keybindings
//!
//! The keybindings are set up here. We define some iamb-specific keybindings, but the default Vim
//! keys come from [modalkit::env::vim::keybindings].
//!
//! iamb's own bindings live in a single table, [IAMB_BINDINGS], which is both what installs them
//! into the keymap and what the [command palette][crate::windows::CommandPaletteState] reads to
//! show users which key runs which command. modalkit's keymap is write-only -- there is no way to
//! ask it which keys map to an action -- so the table has to be the source of truth for both.
//!
//! Only iamb's own bindings are listed. The Vim motions and operators that modalkit provides are
//! not in the table, and so are not discoverable through the palette.
use modalkit::{
    actions::{Action, InsertTextAction, MacroAction, WindowAction},
    env::vim::keybindings::{InputStep, VimBindings},
    env::vim::VimMode,
    env::CommonKeyClass,
    key::TerminalKey,
    keybindings::{EdgeEvent, EdgeRepeat, InputBindings, InputKey},
    prelude::*,
};

use crate::base::{IambAction, IambId, IambInfo, Keybindings, RoomAction, MATRIX_ID_WORD};
use crate::config::{ApplicationSettings, Keys, SplitDirection};

pub type IambStep = InputStep<IambInfo>;

fn once(key: &TerminalKey) -> (EdgeRepeat, EdgeEvent<TerminalKey, CommonKeyClass>) {
    (EdgeRepeat::Once, EdgeEvent::Key(*key))
}

/// One of iamb's own keybindings.
pub struct IambBinding {
    /// The key sequences that run this binding, in the notation [TerminalKey::from_macro_str]
    /// parses. The first is the one shown to users; the rest are equivalent aliases.
    pub keys: &'static [&'static str],

    /// The modes the binding is active in.
    pub modes: &'static [VimMode],

    /// The mode to switch to after running the binding, if it should change.
    pub goto: Option<VimMode>,

    /// The name of the iamb command that does the same thing, if there is one. This is how the
    /// command palette pairs a command up with its key.
    pub command: Option<&'static str>,

    /// What the binding does, for the command palette.
    pub description: &'static str,

    /// The actions to run. This is a function because [Action] values cannot be built in a
    /// constant.
    pub actions: fn() -> Vec<Action<IambInfo>>,
}

impl IambBinding {
    /// The key sequence to show users for this binding.
    pub fn display_keys(&self) -> &'static str {
        self.keys[0]
    }
}

/// The modes that iamb's window-management bindings are available in.
const WINDOW_MODES: &[VimMode] = &[VimMode::Normal, VimMode::Visual];

/// The modes that the newline binding is available in.
const TYPING_MODES: &[VimMode] = &[VimMode::Insert];

/// Every keybinding that iamb itself defines.
pub const IAMB_BINDINGS: &[IambBinding] = &[
    IambBinding {
        keys: &["<C-W>z", "<C-W><C-Z>"],
        modes: WINDOW_MODES,
        goto: Some(VimMode::Normal),
        command: None,
        description: "Toggle zooming in on the focused window",
        actions: || vec![WindowAction::ZoomToggle.into()],
    },
    IambBinding {
        keys: &["<C-W>m", "<C-W><C-M>"],
        modes: WINDOW_MODES,
        goto: Some(VimMode::Normal),
        command: None,
        description: "Toggle focus between the message bar and the scrollback",
        actions: || vec![IambAction::ToggleScrollbackFocus.into()],
    },
    IambBinding {
        keys: &["<C-W>r", "<C-W><C-R>"],
        modes: WINDOW_MODES,
        goto: Some(VimMode::Normal),
        command: Some("read"),
        description: "Mark the focused room, thread, or selected list entry as read",
        actions: || vec![IambAction::Room(RoomAction::MarkRead).into()],
    },
    IambBinding {
        keys: &["<C-W>u", "<C-W><C-U>"],
        modes: WINDOW_MODES,
        goto: Some(VimMode::Normal),
        command: Some("undoread"),
        description: "Undo the most recent read, restoring the previous read markers",
        actions: || vec![IambAction::UndoRead.into()],
    },
    IambBinding {
        // Vim leaves <C-K> alone outside of insert and command mode, where it starts a digraph;
        // this only takes the normal and visual mode key, so digraphs still work while typing.
        keys: &["<C-K>"],
        modes: WINDOW_MODES,
        goto: Some(VimMode::Normal),
        command: Some("switch"),
        description: "Jump to a room, DM, space, or window",
        actions: || {
            let switcher = OpenTarget::Application(IambId::QuickSwitcher);

            vec![WindowAction::Switch(switcher).into()]
        },
    },
    IambBinding {
        keys: &["<Tab>"],
        modes: TYPING_MODES,
        goto: None,
        command: None,
        description: "Take the highlighted completion, or type a tab when nothing is offered",
        actions: || vec![IambAction::AcceptCompletion.into()],
    },
    IambBinding {
        keys: &["<C-W>m", "<S-Enter>"],
        modes: TYPING_MODES,
        goto: None,
        command: None,
        description: "Insert a newline without sending the message",
        actions: || {
            vec![InsertTextAction::Type(
                Char::Single('\n').into(),
                MoveDir1D::Previous,
                1.into(),
            )
            .into()]
        },
    },
];

/// Look up the key sequence to show for an iamb command, if one is bound to it.
pub fn keys_for_command(name: &str) -> Option<&'static str> {
    IAMB_BINDINGS
        .iter()
        .find(|binding| binding.command == Some(name))
        .map(IambBinding::display_keys)
}

/// Initialize the default keybinding state.
pub fn setup_keybindings() -> Keybindings {
    let mut ism = Keybindings::empty();

    let vim = VimBindings::default()
        .submit_on_enter()
        .cursor_open(MATRIX_ID_WORD.clone());

    vim.setup(&mut ism);

    for binding in IAMB_BINDINGS {
        let mut step = IambStep::new().actions((binding.actions)());

        if let Some(mode) = binding.goto {
            step = step.goto(mode);
        }

        for keys in binding.keys {
            let keys = TerminalKey::from_macro_str(keys)
                .expect("iamb's own keybindings should always parse");
            let input = keys.iter().map(once).collect::<Vec<_>>();

            for mode in binding.modes {
                ism.add_mapping(*mode, &input, &step);
            }
        }
    }

    ism
}

impl InputBindings<TerminalKey, IambStep> for ApplicationSettings {
    fn setup(&self, bindings: &mut Keybindings) {
        for (modes, keys) in &self.macros {
            for (Keys(input, _), Keys(_, run)) in keys {
                let act = MacroAction::Run(run.clone(), Count::Contextual);
                let step = IambStep::new().actions(vec![act.into()]);
                let input = input.iter().map(once).collect::<Vec<_>>();

                for mode in &modes.0 {
                    bindings.add_mapping(*mode, &input, &step);
                }
            }
        }

        if self.tunables.default_split == SplitDirection::Vertical {
            let ctrl_w = "<C-W>".parse::<TerminalKey>().unwrap();
            let key_f = "f".parse::<TerminalKey>().unwrap();
            let ctrl_f = "<C-F>".parse::<TerminalKey>().unwrap();

            let vsplit_open = IambStep::new()
                .actions(vec![WindowAction::Split(
                    OpenTarget::Cursor(MATRIX_ID_WORD.clone()),
                    Axis::Vertical,
                    MoveDir1D::Next,
                    1.into(),
                )
                .into()])
                .goto(VimMode::Normal);

            let cwf = vec![once(&ctrl_w), once(&key_f)];
            let cwcf = vec![once(&ctrl_w), once(&ctrl_f)];

            bindings.add_mapping(VimMode::Normal, &cwf, &vsplit_open);
            bindings.add_mapping(VimMode::Normal, &cwcf, &vsplit_open);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binding_keys_parse() {
        for binding in IAMB_BINDINGS {
            for keys in binding.keys {
                let parsed = TerminalKey::from_macro_str(keys);

                assert!(parsed.is_ok(), "{:?} should parse as a key sequence", keys);
                assert!(!parsed.unwrap().is_empty(), "{:?} should not be empty", keys);
            }
        }
    }

    #[test]
    fn test_keys_for_command() {
        assert_eq!(keys_for_command("read"), Some("<C-W>r"));
        assert_eq!(keys_for_command("switch"), Some("<C-K>"));
        assert_eq!(keys_for_command("rooms"), None);
    }
}
