//! # Common types and utilities
//!
//! The types defined here get used throughout iamb.
use std::borrow::Cow;
use std::collections::hash_map::{Entry, IntoIter};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt::{self, Display};
use std::hash::Hash;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::ruma::events::receipt::ReceiptThread;
use matrix_sdk::ruma::events::sticker::StickerEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use ratatui_image::picker::{Picker, ProtocolType};
use serde::{
    de::Error as SerdeError,
    de::Visitor,
    Deserialize,
    Deserializer,
    Serialize,
    Serializer,
};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use matrix_sdk::{
    encryption::verification::SasVerification,
    room::Room as MatrixRoom,
    ruma::{
        events::{
            reaction::ReactionEvent,
            relation::{Replacement, Thread},
            room::encrypted::RoomEncryptedEvent,
            room::member::MembershipState,
            room::message::{
                OriginalRoomMessageEvent,
                Relation,
                RoomMessageEvent,
                RoomMessageEventContent,
                RoomMessageEventContentWithoutRelation,
            },
            room::redaction::{OriginalSyncRoomRedactionEvent, SyncRoomRedactionEvent},
            tag::{TagName, Tags},
            AnySyncStateEvent,
            MessageLikeEvent,
        },
        presence::PresenceState,
        EventId,
        OwnedEventId,
        OwnedRoomId,
        OwnedUserId,
        RoomId,
        UserId,
    },
    RoomState as MatrixRoomState,
};

use modalkit::{
    actions::Action,
    editing::{
        application::{
            ApplicationAction,
            ApplicationContentId,
            ApplicationError,
            ApplicationInfo,
            ApplicationStore,
            ApplicationWindowId,
        },
        completion::{complete_path, Completer, CompletionMap},
        context::EditContext,
        cursor::Cursor,
        rope::EditRope,
        store::Store,
    },
    env::vim::{
        command::{CommandContext, CommandDescription, VimCommand, VimCommandMachine},
        keybindings::VimMachine,
    },
    errors::{UIError, UIResult},
    key::TerminalKey,
    keybindings::SequenceStatus,
    prelude::{CommandType, WordStyle},
};

use crate::config::ImagePreviewProtocolValues;
use crate::message::emoji::{complete_emoji_names, complete_emojis, EMOJI_SIGIL};
use crate::message::mention::{complete_mentions, MentionCandidate, MENTION_SIGIL};
use crate::message::ImageStatus;
use crate::notifications::NotificationHandle;
use matrix_sdk::ruma::UInt;

use crate::snooze::{parse_when, SnoozeKey, SnoozeStore, WakeTime};
use crate::preview::{source_from_event, spawn_insert_preview};
use crate::{
    message::{Message, MessageEvent, MessageKey, MessageTimeStamp, Messages},
    worker::Requester,
    ApplicationSettings,
};

/// The set of characters used in different Matrix IDs.
pub const MATRIX_ID_WORD: WordStyle = WordStyle::CharSet(is_mxid_char);

/// Find the boundaries for a Matrix username, room alias, or room ID.
///
/// Technically "[" and "]" should be here since IPv6 addresses are allowed
/// in the server name, but in practice that should be uncommon, and people
/// can just use `gf` and friends in Visual mode instead.
fn is_mxid_char(c: char) -> bool {
    return c >= 'a' && c <= 'z' ||
        c >= 'A' && c <= 'Z' ||
        c >= '0' && c <= '9' ||
        ":-./@_#!".contains(c);
}

const ROOM_FETCH_DEBOUNCE: Duration = Duration::from_secs(2);

/// Empty type used solely to implement [ApplicationInfo].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IambInfo {}

/// An action taken against an ongoing verification request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyAction {
    /// Accept a verification request.
    Accept,

    /// Cancel an in-progress verification.
    Cancel,

    /// Confirm an in-progress verification.
    Confirm,

    /// Reject an in-progress verification due to mismatched Emoji.
    Mismatch,
}

/// An action taken against the currently selected message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageAction {
    /// Cance the current reply or edit.
    ///
    /// The [bool] argument indicates whether to skip confirmation for clearing the message bar.
    Cancel(bool),

    /// Download an attachment to the given path.
    ///
    /// The second argument controls whether to overwrite any already existing file at the
    /// destination path, or to open the attachment after downloading.
    Download(Option<String>, DownloadFlags),

    /// Edit a sent message.
    Edit,

    /// React to a message with an Emoji.
    ///
    /// `:react` will by default try to convert the [String] argument to an Emoji, and error when
    /// it doesn't recognize it. The second [bool] argument forces it to be interpreted literally
    /// when it is `true`.
    React(String, bool),

    /// Redact a message, with an optional reason.
    ///
    /// The [bool] argument indicates whether to skip confirmation.
    Redact(Option<String>, bool),

    /// Reply to a message.
    Reply,

    /// Go to the message the hovered message replied to.
    Replied,

    /// Unreact to a message.
    ///
    /// If no specific Emoji to remove to is specified, then all reactions from the user on the
    /// message are removed.
    ///
    /// Like `:react`, `:unreact` will by default try to convert the [String] argument to an Emoji,
    /// and error when it doesn't recognize it. The second [bool] argument forces it to be
    /// interpreted literally when it is `true`.
    Unreact(Option<String>, bool),
}

/// An action taken in the currently selected space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpaceAction {
    /// Add a room or update metadata.
    ///
    /// The [`Option<String>`] argument is the order parameter.
    /// The [`bool`] argument indicates whether the room is suggested.
    SetChild(OwnedRoomId, Option<String>, bool),

    /// Remove the selected room.
    RemoveChild,
}

/// The type of room being created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateRoomType {
    /// A standard chat room.
    Room,

    /// A Matrix space.
    Space,
}

bitflags::bitflags! {
    /// Available options for newly created rooms.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct CreateRoomFlags: u32 {
        /// No flags specified.
        const NONE = 0b00000000;

        /// Make the room public.
        const PUBLIC = 0b00000001;

        /// Encrypt this room.
        const ENCRYPTED = 0b00000010;
    }
}

bitflags::bitflags! {
    /// Available options when downloading files.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct DownloadFlags: u32 {
        /// No flags specified.
        const NONE = 0b00000000;

        /// Overwrite file if it already exists.
        const FORCE = 0b00000001;

        /// Open file after downloading.
        const OPEN = 0b00000010;
    }
}

/// Fields that rooms and spaces can be sorted by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SortFieldRoom {
    /// Sort rooms by whether they have the Favorite tag.
    Favorite,

    /// Sort rooms by whether they have the Low Priority tag.
    LowPriority,

    /// Sort rooms by their room name.
    Name,

    /// Sort rooms by their canonical room alias.
    Alias,

    /// Sort rooms by their Matrix room identifier.
    RoomId,

    /// Sort rooms by whether they have unread messages.
    Unread,

    /// Sort rooms by the timestamps of their most recent messages.
    Recent,

    /// Sort rooms by whether they are invites.
    Invite,
}

/// Fields that users can be sorted by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SortFieldUser {
    PowerLevel,
    UserId,
    LocalPart,
    Server,
}

/// Whether to use the default sort direction for a field, or to reverse it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

/// One of the columns to sort on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortColumn<T>(pub T, pub SortOrder);

impl<'de> Deserialize<'de> for SortColumn<SortFieldRoom> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(SortRoomVisitor)
    }
}

/// [serde] visitor for deserializing [SortColumn] for rooms and spaces.
struct SortRoomVisitor;

impl Visitor<'_> for SortRoomVisitor {
    type Value = SortColumn<SortFieldRoom>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid field for sorting rooms")
    }

    fn visit_str<E>(self, mut value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        if value.is_empty() {
            return Err(E::custom("Invalid sort field"));
        }

        let order = if value.starts_with('~') {
            value = &value[1..];
            SortOrder::Descending
        } else {
            SortOrder::Ascending
        };

        let field = match value {
            "favorite" => SortFieldRoom::Favorite,
            "lowpriority" => SortFieldRoom::LowPriority,
            "recent" => SortFieldRoom::Recent,
            "unread" => SortFieldRoom::Unread,
            "name" => SortFieldRoom::Name,
            "alias" => SortFieldRoom::Alias,
            "id" => SortFieldRoom::RoomId,
            "invite" => SortFieldRoom::Invite,
            _ => {
                let msg = format!("Unknown sort field: {value:?}");
                return Err(E::custom(msg));
            },
        };

        Ok(SortColumn(field, order))
    }
}

impl<'de> Deserialize<'de> for SortColumn<SortFieldUser> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(SortUserVisitor)
    }
}

/// [serde] visitor for deserializing [SortColumn] for users.
struct SortUserVisitor;

impl Visitor<'_> for SortUserVisitor {
    type Value = SortColumn<SortFieldUser>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid field for sorting rooms")
    }

    fn visit_str<E>(self, mut value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        if value.is_empty() {
            return Err(E::custom("Invalid field for sorting users"));
        }

        let order = if value.starts_with('~') {
            value = &value[1..];
            SortOrder::Descending
        } else {
            SortOrder::Ascending
        };

        let field = match value {
            "id" => SortFieldUser::UserId,
            "localpart" => SortFieldUser::LocalPart,
            "server" => SortFieldUser::Server,
            "power" => SortFieldUser::PowerLevel,
            _ => {
                let msg = format!("Unknown sort field: {value:?}");
                return Err(E::custom(msg));
            },
        };

        Ok(SortColumn(field, order))
    }
}

/// A room property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoomField {
    /// The room's history visibility.
    History,

    /// The room name.
    Name,

    /// The room id.
    Id,

    /// A room tag.
    Tag(TagName),

    /// The room topic.
    Topic,

    /// Notification level.
    NotificationMode,

    /// The room's entire list of alternative aliases.
    Aliases,

    /// A specific alternative alias to the room.
    Alias(String),

    /// The room's canonical alias.
    CanonicalAlias,
}

/// An action that operates on a room member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemberUpdateAction {
    Ban,
    Kick,
    Unban,
}

impl Display for MemberUpdateAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberUpdateAction::Ban => write!(f, "ban"),
            MemberUpdateAction::Kick => write!(f, "kick"),
            MemberUpdateAction::Unban => write!(f, "unban"),
        }
    }
}

/// An action that operates on a focused room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoomAction {
    /// Accept an invitation to join this room.
    InviteAccept,

    /// Reject an invitation to join this room.
    InviteReject,

    /// Invite a user to this room.
    InviteSend(OwnedUserId),

    /// Leave this room.
    Leave(bool),

    /// Mark this room, or the thread being viewed, as read.
    MarkRead,

    /// Defer this room, or the thread being viewed, out of the inbox until a time.
    ///
    /// The [String] is the duration as the user wrote it. Parsing happens where the action runs,
    /// so that the error can name what was typed.
    Snooze(String),

    /// Cancel a snooze on this room, or on the thread being viewed.
    Unsnooze,

    /// Move the scrollback cursor onto a message, loading it first if necessary.
    SelectMessage(OwnedEventId),

    /// Update a user's membership in this room.
    MemberUpdate(MemberUpdateAction, String, Option<String>, bool),

    /// Open the members window.
    Members(Box<CommandContext>),

    /// Set whether a room is a direct message.
    SetDirect(bool),

    /// Set a room property.
    Set(RoomField, String),

    /// Unset a room property.
    Unset(RoomField),

    /// List the values in a list room property.
    Show(RoomField),
}

/// An action that sends a message to a room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SendAction {
    /// Send the text in the message bar.
    Submit,

    /// Send text provided from an external editor.
    SubmitFromEditor,

    /// Upload a file.
    Upload(String),

    /// Upload the image data.
    UploadImage(usize, usize, Cow<'static, [u8]>),

    /// Upload the image currently held in the system clipboard.
    UploadClipboard,
}

/// An action performed against the user's homeserver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HomeserverAction {
    /// Create a new room with an optional localpart.
    CreateRoom(Option<String>, CreateRoomType, CreateRoomFlags),
    Logout(String, bool),
    /// Forget all left rooms
    Forget,
}

/// An action performed against the user's room keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeysAction {
    /// Export room keys to a file, encrypted with a passphrase.
    Export(String, String),
    /// Import room keys from a file, encrypted with a passphrase.
    Import(String, String),
}

/// An action that the main program loop should.
///
/// See [the commands module][super::commands] for where these are usually created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IambAction {
    /// Perform an action against the homeserver.
    Homeserver(HomeserverAction),

    /// Perform an action over room keys.
    Keys(KeysAction),

    /// Perform an action on the currently selected message.
    Message(MessageAction),

    /// Perform an action on the current space.
    Space(SpaceAction),

    /// Open a URL.
    OpenLink(String),

    /// Perform an action on the currently focused room.
    Room(RoomAction),

    /// Send a message to the currently focused room.
    Send(SendAction),

    /// Perform an action for an in-progress verification.
    Verify(VerifyAction, String),

    /// Request a new verification with the specified user.
    VerifyRequest(String),

    /// Toggle the focus within the focused room.
    ToggleScrollbackFocus,

    /// Take the highlighted entry if a completion popup is open, and otherwise type a tab.
    ///
    /// This is what `<Tab>` does while composing. modalkit binds `<Tab>` in insert mode to typing
    /// a literal tab, which is still what happens whenever there is no popup to take from.
    AcceptCompletion,

    /// Clear all unread messages.
    ClearUnreads,

    /// Put the receipts moved by the most recent read operation back where they were.
    UndoRead,
}

impl IambAction {
    /// Indicates whether this action will draw over the screen.
    pub fn scribbles(&self) -> bool {
        matches!(self, IambAction::Send(SendAction::SubmitFromEditor))
    }
}

impl From<HomeserverAction> for IambAction {
    fn from(act: HomeserverAction) -> Self {
        IambAction::Homeserver(act)
    }
}

impl From<MessageAction> for IambAction {
    fn from(act: MessageAction) -> Self {
        IambAction::Message(act)
    }
}

impl From<SpaceAction> for IambAction {
    fn from(act: SpaceAction) -> Self {
        IambAction::Space(act)
    }
}

impl From<RoomAction> for IambAction {
    fn from(act: RoomAction) -> Self {
        IambAction::Room(act)
    }
}

impl From<SendAction> for IambAction {
    fn from(act: SendAction) -> Self {
        IambAction::Send(act)
    }
}

impl ApplicationAction for IambAction {
    fn is_edit_sequence(&self, _: &EditContext) -> SequenceStatus {
        match self {
            IambAction::AcceptCompletion => SequenceStatus::Track,
            IambAction::ClearUnreads => SequenceStatus::Break,
            IambAction::UndoRead => SequenceStatus::Break,
            IambAction::Homeserver(..) => SequenceStatus::Break,
            IambAction::Keys(..) => SequenceStatus::Break,
            IambAction::Message(..) => SequenceStatus::Break,
            IambAction::Space(..) => SequenceStatus::Break,
            IambAction::Room(..) => SequenceStatus::Break,
            IambAction::OpenLink(..) => SequenceStatus::Break,
            IambAction::Send(..) => SequenceStatus::Break,
            IambAction::ToggleScrollbackFocus => SequenceStatus::Break,
            IambAction::Verify(..) => SequenceStatus::Break,
            IambAction::VerifyRequest(..) => SequenceStatus::Break,
        }
    }

    fn is_last_action(&self, _: &EditContext) -> SequenceStatus {
        match self {
            IambAction::AcceptCompletion => SequenceStatus::Atom,
            IambAction::ClearUnreads => SequenceStatus::Atom,
            IambAction::UndoRead => SequenceStatus::Atom,
            IambAction::Homeserver(..) => SequenceStatus::Atom,
            IambAction::Keys(..) => SequenceStatus::Atom,
            IambAction::Message(..) => SequenceStatus::Atom,
            IambAction::Space(..) => SequenceStatus::Atom,
            IambAction::OpenLink(..) => SequenceStatus::Atom,
            IambAction::Room(..) => SequenceStatus::Atom,
            IambAction::Send(..) => SequenceStatus::Atom,
            IambAction::ToggleScrollbackFocus => SequenceStatus::Atom,
            IambAction::Verify(..) => SequenceStatus::Atom,
            IambAction::VerifyRequest(..) => SequenceStatus::Atom,
        }
    }

    fn is_last_selection(&self, _: &EditContext) -> SequenceStatus {
        match self {
            IambAction::AcceptCompletion => SequenceStatus::Ignore,
            IambAction::ClearUnreads => SequenceStatus::Ignore,
            IambAction::UndoRead => SequenceStatus::Ignore,
            IambAction::Homeserver(..) => SequenceStatus::Ignore,
            IambAction::Keys(..) => SequenceStatus::Ignore,
            IambAction::Message(..) => SequenceStatus::Ignore,
            IambAction::Space(..) => SequenceStatus::Ignore,
            IambAction::Room(..) => SequenceStatus::Ignore,
            IambAction::OpenLink(..) => SequenceStatus::Ignore,
            IambAction::Send(..) => SequenceStatus::Ignore,
            IambAction::ToggleScrollbackFocus => SequenceStatus::Ignore,
            IambAction::Verify(..) => SequenceStatus::Ignore,
            IambAction::VerifyRequest(..) => SequenceStatus::Ignore,
        }
    }

    fn is_switchable(&self, _: &EditContext) -> bool {
        match self {
            IambAction::AcceptCompletion => false,
            IambAction::ClearUnreads => false,
            IambAction::UndoRead => false,
            IambAction::Homeserver(..) => false,
            IambAction::Message(..) => false,
            IambAction::Space(..) => false,
            IambAction::Room(..) => false,
            IambAction::Keys(..) => false,
            IambAction::Send(..) => false,
            IambAction::OpenLink(..) => false,
            IambAction::ToggleScrollbackFocus => false,
            IambAction::Verify(..) => false,
            IambAction::VerifyRequest(..) => false,
        }
    }
}

impl From<RoomAction> for ProgramAction {
    fn from(act: RoomAction) -> Self {
        IambAction::from(act).into()
    }
}

impl From<SpaceAction> for ProgramAction {
    fn from(act: SpaceAction) -> Self {
        IambAction::from(act).into()
    }
}

impl From<IambAction> for ProgramAction {
    fn from(act: IambAction) -> Self {
        Action::Application(act)
    }
}

/// Alias for program actions.
pub type ProgramAction = Action<IambInfo>;
/// Alias for program context.
pub type ProgramContext = EditContext;
/// Alias for program keybindings.
pub type Keybindings = VimMachine<TerminalKey, IambInfo>;
/// Alias for a program command.
pub type ProgramCommand = VimCommand<IambInfo>;
/// Alias for mapped program commands.
pub type ProgramCommands = VimCommandMachine<IambInfo>;
/// Alias for program store.
pub type ProgramStore = Store<IambInfo>;
/// Alias for shared program store.
pub type AsyncProgramStore = Arc<AsyncMutex<ProgramStore>>;
/// Alias for an action result.
pub type IambResult<T> = UIResult<T, IambInfo>;

/// Reaction events for some message.
///
/// The event identifier used as a key here is the ID for the reaction, and not for the message
/// it's reacting to.
pub type MessageReactions = HashMap<OwnedEventId, (String, OwnedUserId)>;

/// Errors encountered during application use.
#[derive(thiserror::Error, Debug)]
pub enum IambError {
    /// An invalid history visibility was specified.
    #[error("Invalid history visibility setting: {0}")]
    InvalidHistoryVisibility(String),

    /// An invalid notification level was specified.
    #[error("Invalid notification level: {0}")]
    InvalidNotificationLevel(String),

    /// An invalid user identifier was specified.
    #[error("Invalid user identifier: {0}")]
    InvalidUserId(String),

    /// An invalid user identifier was specified.
    #[error("Invalid room alias: {0}")]
    InvalidRoomAlias(String),

    /// An invalid verification identifier was specified.
    #[error("Invalid verification user/device pair: {0}")]
    InvalidVerificationId(String),

    /// A failure related to the cryptographic store.
    #[error("Cryptographic storage error: {0}")]
    CryptoStore(#[from] matrix_sdk::encryption::CryptoStoreError),

    #[error("Failed to import room keys: {0}")]
    FailedKeyImport(#[from] matrix_sdk::encryption::RoomKeyImportError),

    /// A failure related to the cryptographic store.
    #[error("Cannot export keys from sled: {0}")]
    UpgradeSled(#[from] crate::sled_export::SledMigrationError),

    /// An HTTP error.
    #[error("HTTP client error: {0}")]
    Http(#[from] matrix_sdk::HttpError),

    /// A failure from the Matrix client.
    #[error("Matrix client error: {0}")]
    Matrix(#[from] matrix_sdk::Error),

    /// A failure in the sled storage.
    #[error("Matrix client storage error: {0}")]
    Store(#[from] matrix_sdk::StoreError),

    /// A failure during serialization or deserialization.
    #[error("Serialization/deserialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A failure due to not having a configured download directory.
    #[error("No download directory configured")]
    NoDownloadDir,

    /// A failure due to not having a message with an attachment selected.
    #[error("Selected message does not have any attachments")]
    NoAttachment,

    /// A failure due to not having a message selected.
    #[error("No message currently selected")]
    NoSelectedMessage,

    /// A failure due to not having a room or space selected.
    #[error("Current window is not a room or space")]
    NoSelectedRoomOrSpace,

    /// A failure due to not having a room or space item selected in a list.
    #[error("No room or space currently selected in list")]
    NoSelectedRoomOrSpaceItem,

    /// A failure due to not having a room selected.
    #[error("Current window is not a room")]
    NoSelectedRoom,

    /// A failure due to there being no recorded read operation left to undo.
    #[error("No read left to undo")]
    NothingToUndoRead,

    #[error("This room or thread is not snoozed")]
    NotSnoozed,

    #[error("{0}")]
    BadSnoozeDuration(String),

    /// A failure due to not having a space selected.
    #[error("Current window is not a space")]
    NoSelectedSpace,

    /// A failure due to not having sufficient permission to perform an action in a room.
    #[error("You do not have the permission to do that")]
    InsufficientPermission,

    /// A failure due to not having an outstanding room invitation.
    #[error("You do not have a current invitation to this room")]
    NotInvited,

    /// A failure due to not being a joined room member.
    #[error("You need to join the room before you can do that")]
    NotJoined,

    /// An unknown room was specified.
    #[error("Unknown room identifier: {0}")]
    UnknownRoom(OwnedRoomId),

    /// An invalid room alias id was specified.
    #[error("Invalid room alias id: {0}")]
    InvalidRoomAliasId(#[from] matrix_sdk::ruma::IdParseError),

    /// An invalid space child order was specified.
    #[error("Invalid space child order: {0}")]
    InvalidSpaceChildOrder(matrix_sdk::ruma::IdParseError),

    /// A failure occurred during verification.
    #[error("Verification request error: {0}")]
    VerificationRequestError(#[from] matrix_sdk::encryption::identities::RequestVerificationError),

    #[error("Notification setting error: {0}")]
    NotificationSettingError(#[from] matrix_sdk::NotificationSettingsError),

    /// A failure related to images.
    #[error("Image error: {0}")]
    Image(#[from] image::ImageError),

    /// A failure to access the system's clipboard.
    #[error("Could not use system clipboard data")]
    Clipboard,

    /// The system clipboard holds something other than an image.
    #[error("No image in the system clipboard")]
    ClipboardHasNoImage,

    /// An failure during disk/network/ipc/etc. I/O.
    #[error("Input/Output error: {0}")]
    IOError(#[from] std::io::Error),

    /// A failure while trying to show an image preview.
    #[error("Preview error: {0}")]
    Preview(String),
}

impl From<IambError> for UIError<IambInfo> {
    fn from(err: IambError) -> Self {
        UIError::Application(err)
    }
}

impl ApplicationError for IambError {}

/// Status for tracking how much room scrollback we've fetched.
#[derive(Default)]
pub enum RoomFetchStatus {
    /// Room history has been completely fetched.
    Done,

    /// More room history can be fetched.
    HaveMore(String),

    /// We have not yet started fetching history for this room.
    #[default]
    NotStarted,
}

/// Indicates where an [EventId] lives in the [ChatStore].
pub enum EventLocation {
    /// The [EventId] belongs to a message.
    ///
    /// If the first argument is [None], then it's part of the main scrollback. When [Some],
    /// it specifies which thread it's in reply to.
    Message(Option<OwnedEventId>, MessageKey),

    /// The [EventId] belongs to a reaction to the given event.
    Reaction(OwnedEventId),

    /// The [EventId] belongs to a state event in the main timeline of the room.
    State(MessageKey),

    /// The [EventId] belongs to a sticker event in the main scrollback
    Sticker(MessageKey),
}

impl EventLocation {
    fn to_message_key(&self) -> Option<&MessageKey> {
        match self {
            EventLocation::Message(_, key) => Some(key),
            EventLocation::Sticker(key) => Some(key),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnreadInfo {
    pub(crate) unread: bool,
    pub(crate) latest: Option<MessageTimeStamp>,
}

impl UnreadInfo {
    pub fn is_unread(&self) -> bool {
        self.unread
    }

    pub fn latest(&self) -> Option<&MessageTimeStamp> {
        self.latest.as_ref()
    }

    /// Give this entry a wake time in place of its newest message time.
    ///
    /// This is what makes a woken entry rise to the top of the inbox without any wake code. The
    /// sort reads `latest`, so an entry whose wake time has just passed carries a timestamp of
    /// roughly now and outranks older unread traffic.
    ///
    /// The later of the two is taken, so that a message which arrived after the wake time still
    /// decides the order.
    pub fn with_wake_time(mut self, wake_at: Option<WakeTime>) -> Self {
        let Some(wake_at) = wake_at else {
            return self;
        };

        let Ok(wake_at) = UInt::try_from(wake_at) else {
            return self;
        };

        let wake = MessageTimeStamp::OriginServer(wake_at);

        self.latest = match self.latest {
            Some(latest) if latest > wake => Some(latest),
            _ => Some(wake),
        };

        self
    }
}

/// How much of a thread's root message to show in a thread list.
const THREAD_PREVIEW_CHARS: usize = 60;

/// Appended to a thread preview that was cut short at [THREAD_PREVIEW_CHARS].
const THREAD_PREVIEW_ELLIPSIS: &str = "…";

/// Shown for a thread whose root message hasn't been loaded into scrollback.
const THREAD_PREVIEW_UNAVAILABLE: &str = "<unloaded thread root>";

/// A thread the user follows, as shown in the `:threads` and `:unreads-and-threads` windows.
#[derive(Clone)]
pub struct ThreadSummary {
    /// The event that started the thread.
    pub root: OwnedEventId,

    /// A short preview of the thread's root message.
    pub preview: String,

    /// Whether the thread has activity the user hasn't read yet.
    pub unread: UnreadInfo,
}

/// Track the display names for users and render any needed disambiguation for
/// those with overlapping names.
#[derive(Default)]
pub struct DisplayNameStore {
    by_ids: HashMap<OwnedUserId, String>,
    by_names: HashMap<String, HashSet<OwnedUserId>>,
}

impl DisplayNameStore {
    /// Update the `HashSet` associated with a given displayname.
    ///
    /// Note that this *could* be done more elegantly using the Entry API, but
    /// is intentionally written in a way to avoid cloning conflicting display
    /// names.
    fn set_by_name(&mut self, user_id: OwnedUserId, name: &str) {
        if let Some(existing) = self.by_names.get_mut(name) {
            existing.insert(user_id);
        } else {
            self.by_names.insert(name.to_owned(), HashSet::from([user_id]));
        }
    }

    /// Track a new user ID to displayname mapping, or unset any existing ones.
    pub fn set(&mut self, user_id: OwnedUserId, name: Option<String>) {
        if let Some(name) = name.as_deref() {
            self.set_by_name(user_id.clone(), name);
        }

        let previous = match (self.by_ids.entry(user_id), name) {
            // Nothing to do!
            (Entry::Vacant(_), None) => None,

            // Setting initial display name for user:
            (Entry::Vacant(v), Some(name)) => {
                v.insert(name);
                None
            },

            // Unsetting display name:
            (Entry::Occupied(o), None) => Some(o.remove_entry()),

            // Replacing existing name:
            (Entry::Occupied(mut o), Some(name)) => {
                if o.get() == &name {
                    None
                } else {
                    Some((o.key().clone(), o.insert(name)))
                }
            },
        };

        let Some((user_id, previous)) = previous else {
            return;
        };

        let Some(users) = self.by_names.get_mut(&previous) else {
            return;
        };

        users.remove(&user_id);

        if users.is_empty() {
            self.by_names.remove(&previous);
        }
    }

    pub fn get<'a>(&'a self, user_id: &UserId) -> Option<Cow<'a, str>> {
        let displayname = self.by_ids.get(user_id)?;
        let users = self.by_names.get(displayname)?;

        if !users.contains(user_id) {
            // Internal consistency error? Assume no display name:
            return None;
        }

        if users.len() == 1 {
            // Unambiguous!
            return Some(Cow::Borrowed(displayname.as_str()));
        }

        // Ambiguous username, so include unique user ID:
        Some(Cow::Owned(format!("{displayname} ({user_id})")))
    }
}

/// Information about room's the user's joined.
pub struct RoomInfo {
    /// The display name for this room.
    pub name: Option<String>,

    /// The tags placed on this room.
    pub tags: Option<Tags>,

    /// A map of event IDs to where they are stored in this struct.
    pub keys: HashMap<OwnedEventId, EventLocation>,

    /// The messages loaded for this room.
    messages: Messages,

    /// A map of read markers to display on different events.
    pub event_receipts: HashMap<ReceiptThread, HashMap<OwnedEventId, HashSet<OwnedUserId>>>,
    /// A map of the most recent read marker for each user.
    ///
    /// Every receipt in this map should also have an entry in [`event_receipts`](`Self::event_receipts`),
    /// however not every user has an entry. If a user's most recent receipt is
    /// older than the oldest loaded event, that user will not be included.
    pub user_receipts: HashMap<ReceiptThread, HashMap<OwnedUserId, OwnedEventId>>,
    /// A map of message identifiers to a map of reaction events.
    pub reactions: HashMap<OwnedEventId, MessageReactions>,

    /// A map of message identifiers to thread replies.
    threads: HashMap<OwnedEventId, Messages>,

    /// Whether the scrollback for this room is currently being fetched.
    pub fetching: bool,

    /// Where to continue fetching from when we continue loading scrollback history.
    pub fetch_id: RoomFetchStatus,

    /// The time that we last fetched scrollback for this room.
    pub fetch_last: Option<Instant>,

    /// Users currently typing in this room, and when we received notification of them doing so.
    pub users_typing: Option<(Instant, Vec<OwnedUserId>)>,

    /// The display names for users in this room.
    pub display_names: DisplayNameStore,

    /// The last time the room was rendered, used to detect if it is currently open.
    pub draw_last: Option<Instant>,
}

impl Default for RoomInfo {
    fn default() -> Self {
        Self {
            messages: Messages::new(ReceiptThread::Main),

            name: Default::default(),
            tags: Default::default(),
            keys: Default::default(),
            event_receipts: Default::default(),
            user_receipts: Default::default(),
            reactions: Default::default(),
            threads: Default::default(),
            fetching: Default::default(),
            fetch_id: Default::default(),
            fetch_last: Default::default(),
            users_typing: Default::default(),
            display_names: Default::default(),
            draw_last: Default::default(),
        }
    }
}

impl RoomInfo {
    pub fn get_thread(&self, root: Option<&EventId>) -> Option<&Messages> {
        if let Some(thread_root) = root {
            self.threads.get(thread_root)
        } else {
            Some(&self.messages)
        }
    }

    /// The roots of every thread we've loaded messages for in this room.
    pub fn thread_roots(&self) -> impl Iterator<Item = &OwnedEventId> + '_ {
        self.threads.keys()
    }

    pub fn get_thread_mut(&mut self, root: Option<OwnedEventId>) -> &mut Messages {
        if let Some(thread_root) = root {
            self.threads
                .entry(thread_root.clone())
                .or_insert_with(|| Messages::thread(thread_root))
        } else {
            &mut self.messages
        }
    }

    /// The threads in this room that the user follows, with their unread state.
    ///
    /// See [ThreadSummary] for what "follows" means here.
    pub fn followed_threads(&self, settings: &ApplicationSettings) -> Vec<ThreadSummary> {
        let user_id = &settings.profile.user_id;

        self.threads
            .keys()
            .filter(|root| self.follows_thread(root, user_id))
            .map(|root| {
                ThreadSummary {
                    root: root.clone(),
                    preview: self.thread_preview(root),
                    unread: self.thread_unreads(root, settings),
                }
            })
            .collect()
    }

    /// Whether the user follows the thread rooted at `root`.
    ///
    /// Matrix thread subscriptions (MSC4308) are not plumbed through iamb's sync worker, so
    /// following is derived from evidence we already track:
    ///
    /// - the user started the thread, or replied in it (participation), or
    /// - the user has a thread-scoped read receipt for it, which is how another client records
    ///   that it was tracking the thread on their behalf (subscription proxy).
    fn follows_thread(&self, root: &EventId, user_id: &UserId) -> bool {
        let sent_root = self.get_event(root).is_some_and(|msg| msg.sender == user_id);

        let sent_reply = self
            .threads
            .get(root)
            .is_some_and(|replies| replies.values().any(|msg| msg.sender == user_id));

        let has_thread_receipt = self
            .user_receipts
            .get(&ReceiptThread::Thread(root.to_owned()))
            .is_some_and(|receipts| receipts.contains_key(user_id));

        sent_root || sent_reply || has_thread_receipt
    }

    /// A short, single-line preview of the message that started a thread.
    fn thread_preview(&self, root: &EventId) -> String {
        let Some(msg) = self.get_event(root) else {
            return String::from(THREAD_PREVIEW_UNAVAILABLE);
        };

        let body = msg.event.body();
        let mut preview = String::new();

        for c in body.chars().take(THREAD_PREVIEW_CHARS) {
            preview.push(if c.is_control() { ' ' } else { c });
        }

        if body.chars().nth(THREAD_PREVIEW_CHARS).is_some() {
            preview.push_str(THREAD_PREVIEW_ELLIPSIS);
        }

        preview
    }

    /// Indicates whether the thread rooted at `root` has unread messages.
    ///
    /// The user's thread-scoped receipt is the authority, but clients that don't send threaded
    /// receipts advance only the main or unthreaded receipt, so those count as reading the thread
    /// too: we compare against whichever of the three is newest.
    pub fn thread_unreads(&self, root: &EventId, settings: &ApplicationSettings) -> UnreadInfo {
        let user_id = &settings.profile.user_id;
        let last_message = self
            .threads
            .get(root)
            .and_then(|replies| replies.last_key_value())
            .map(|(key, _)| key)
            .or_else(|| self.get_message_key(root));

        let last_receipt = [
            ReceiptThread::Thread(root.to_owned()),
            ReceiptThread::Main,
            ReceiptThread::Unthreaded,
        ]
        .iter()
        .filter_map(|thread| self.receipt_key(thread, user_id))
        .max();

        match (last_message, last_receipt) {
            (Some((ts, _)), Some((read_ts, _))) => {
                UnreadInfo { unread: ts > read_ts, latest: Some(*ts) }
            },
            (Some((ts, _)), None) => UnreadInfo { unread: true, latest: Some(*ts) },
            (None, _) => UnreadInfo::default(),
        }
    }

    /// Where a user's most recent read receipt for a receipt thread sits in the scrollback.
    ///
    /// This is `None` when the user has no such receipt, or when the event it points at is older
    /// than the oldest event we've loaded.
    fn receipt_key(&self, thread: &ReceiptThread, user_id: &UserId) -> Option<&MessageKey> {
        let event_id = self.user_receipts.get(thread)?.get(user_id)?;

        match self.keys.get(event_id)? {
            EventLocation::Message(_, key) |
            EventLocation::State(key) |
            EventLocation::Sticker(key) => Some(key),
            EventLocation::Reaction(_) => None,
        }
    }

    /// Get the event for the last message in a thread (or the thread root if there are no
    /// in-thread replies yet).
    ///
    /// This returns `None` if the event identifier isn't in the room.
    pub fn get_thread_last<'a>(
        &'a self,
        thread_root: &OwnedEventId,
    ) -> Option<&'a OriginalRoomMessageEvent> {
        let last = self.threads.get(thread_root).and_then(|t| Some(t.last_key_value()?.1));

        let msg = if let Some(last) = last {
            &last.event
        } else if let EventLocation::Message(_, key) = self.keys.get(thread_root)? {
            let msg = self.messages.get(key)?;
            &msg.event
        } else {
            return None;
        };

        if let MessageEvent::Original(ev) = &msg {
            Some(ev)
        } else {
            None
        }
    }

    /// Get the reactions and their counts for a message.
    pub fn get_reactions(&self, event_id: &EventId) -> Vec<(&str, usize)> {
        if let Some(reacts) = self.reactions.get(event_id) {
            let mut counts = HashMap::new();

            let mut seen_user_reactions = BTreeSet::new();

            for (key, user) in reacts.values() {
                if !seen_user_reactions.contains(&(key, user)) {
                    seen_user_reactions.insert((key, user));
                    let count = counts.entry(key.as_str()).or_default();
                    *count += 1;
                }
            }

            let mut reactions = counts.into_iter().collect::<Vec<_>>();
            reactions.sort();

            reactions
        } else {
            vec![]
        }
    }

    /// Map an event identifier to its [MessageKey].
    pub fn get_message_key(&self, event_id: &EventId) -> Option<&MessageKey> {
        self.keys.get(event_id)?.to_message_key()
    }

    /// Get an event for an identifier.
    pub fn get_event(&self, event_id: &EventId) -> Option<&Message> {
        self.messages.get(self.get_message_key(event_id)?)
    }

    /// Get an event for an identifier as mutable.
    pub fn get_event_mut(&mut self, event_id: &EventId) -> Option<&mut Message> {
        self.messages.get_mut(self.keys.get(event_id)?.to_message_key()?)
    }

    pub fn redact(&mut self, ev: OriginalSyncRoomRedactionEvent) {
        let Some(redacts) = &ev.redacts else {
            return;
        };

        match self.keys.get(redacts) {
            None => return,
            Some(EventLocation::State(key)) => {
                if let Some(msg) = self.messages.get_mut(key) {
                    let ev = SyncRoomRedactionEvent::Original(ev);
                    msg.redact(ev);
                }
            },
            Some(EventLocation::Message(None, key)) => {
                if let Some(msg) = self.messages.get_mut(key) {
                    let ev = SyncRoomRedactionEvent::Original(ev);
                    msg.redact(ev);
                }
            },
            Some(EventLocation::Message(Some(root), key)) => {
                if let Some(thread) = self.threads.get_mut(root) {
                    if let Some(msg) = thread.get_mut(key) {
                        let ev = SyncRoomRedactionEvent::Original(ev);
                        msg.redact(ev);
                    }
                }
            },
            Some(EventLocation::Reaction(event_id)) => {
                if let Some(reactions) = self.reactions.get_mut(event_id) {
                    reactions.remove(redacts);
                }

                self.keys.remove(redacts);
            },
            Some(EventLocation::Sticker(key)) => {
                if let Some(msg) = self.messages.get_mut(key) {
                    let ev = SyncRoomRedactionEvent::Original(ev);
                    msg.redact(ev);
                }
            },
        }
    }

    /// Insert a reaction to a message.
    pub fn insert_reaction(&mut self, react: ReactionEvent) {
        match react {
            MessageLikeEvent::Original(react) => {
                let rel_id = react.content.relates_to.event_id;
                let key = react.content.relates_to.key;

                let message = self.reactions.entry(rel_id.clone()).or_default();
                let event_id = react.event_id;
                let user_id = react.sender;

                message.insert(event_id.clone(), (key, user_id));

                let loc = EventLocation::Reaction(rel_id);
                self.keys.insert(event_id, loc);
            },
            MessageLikeEvent::Redacted(_) => {
                return;
            },
        }
    }

    /// Insert a sticker
    pub fn insert_sticker(
        &mut self,
        room_id: OwnedRoomId,
        store: AsyncProgramStore,
        picker: Option<Picker>,
        sticker: StickerEvent,
        settings: &ApplicationSettings,
        media: matrix_sdk::Media,
    ) {
        match sticker {
            MessageLikeEvent::Original(ref sticker_content) => {
                let key =
                    (sticker_content.origin_server_ts.into(), sticker_content.event_id.clone());

                let loc = EventLocation::Sticker(key.clone());

                self.keys.insert(sticker_content.event_id.clone(), loc);
                self.messages.insert_message(key.clone(), sticker.clone());

                if picker.is_some() {
                    if let (Some(msg), Some(image_preview)) = (
                        self.get_event_mut(&sticker_content.event_id),
                        &settings.tunables.image_preview,
                    ) {
                        msg.image_preview = ImageStatus::Downloading(image_preview.size.clone());
                        spawn_insert_preview(
                            store,
                            room_id,
                            sticker_content.event_id.clone(),
                            sticker_content.content.source.clone().into(),
                            media,
                            settings.dirs.image_previews.clone(),
                        )
                    }
                }
            },
            MessageLikeEvent::Redacted(ref redaction) => {
                let key = (redaction.origin_server_ts.into(), redaction.event_id.clone());
                self.messages.insert_message(key.clone(), sticker.clone());
            },
        }
    }

    /// Insert an edit.
    pub fn insert_edit(&mut self, msg: Replacement<RoomMessageEventContentWithoutRelation>) {
        let event_id = msg.event_id;
        let new_msgtype = msg.new_content;

        let Some(EventLocation::Message(thread, key)) = self.keys.get(&event_id) else {
            return;
        };

        let source = if let Some(thread) = thread {
            self.threads
                .entry(thread.clone())
                .or_insert_with(|| Messages::thread(thread.clone()))
        } else {
            &mut self.messages
        };

        let Some(msg) = source.get_mut(key) else {
            return;
        };

        match &mut msg.event {
            MessageEvent::Original(orig) => {
                orig.content.apply_replacement(new_msgtype);
            },
            MessageEvent::Local(_, content) => {
                content.apply_replacement(new_msgtype);
            },
            MessageEvent::Redacted(_, _) |
            MessageEvent::State(_) |
            MessageEvent::Sticker(_) |
            MessageEvent::EncryptedOriginal(_) |
            MessageEvent::EncryptedRedacted(_) => {
                return;
            },
        }

        msg.html = msg.event.html();
        msg.event.strip_reply_fallback();
    }

    pub fn insert_any_state(&mut self, msg: AnySyncStateEvent) {
        let event_id = msg.event_id().to_owned();
        let key = (msg.origin_server_ts().into(), event_id.clone());

        let loc = EventLocation::State(key.clone());
        self.keys.insert(event_id, loc);
        self.messages.insert_message(key, msg);
    }

    /// Indicates whether this room has unread messages.
    pub fn unreads(&self, settings: &ApplicationSettings) -> UnreadInfo {
        let last_message = self.messages.last_key_value();

        let user_id = &settings.profile.user_id;
        let last_receipt = self.receipt_key(&ReceiptThread::Main, user_id);
        let last_unthreaded = self.receipt_key(&ReceiptThread::Unthreaded, user_id);

        let last_receipt = std::cmp::max(last_receipt, last_unthreaded);

        match (last_message, last_receipt) {
            (Some(((ts, _), _)), Some((read_ts, _))) => {
                UnreadInfo { unread: ts > read_ts, latest: Some(*ts) }
            },
            (Some(((ts, _), _)), None) => {
                // If we've never loaded/generated a room's receipt (example,
                // a newly joined but never viewed room), show it as unread.
                UnreadInfo { unread: true, latest: Some(*ts) }
            },
            (None, _) => UnreadInfo::default(),
        }
    }

    /// Inserts events that couldn't be decrypted into the scrollback.
    pub fn insert_encrypted(&mut self, msg: RoomEncryptedEvent) {
        let event_id = msg.event_id().to_owned();
        let key = (msg.origin_server_ts().into(), event_id.clone());

        self.keys.insert(event_id, EventLocation::Message(None, key.clone()));
        self.messages.insert(key, msg.into());
    }

    /// Insert a new message.
    pub fn insert_message(&mut self, msg: RoomMessageEvent) {
        let event_id = msg.event_id().to_owned();
        let key = (msg.origin_server_ts().into(), event_id.clone());

        let loc = EventLocation::Message(None, key.clone());
        self.keys.insert(event_id, loc);
        self.messages.insert_message(key, msg);
    }

    fn insert_thread(&mut self, msg: RoomMessageEvent, thread_root: OwnedEventId) {
        let event_id = msg.event_id().to_owned();
        let key = (msg.origin_server_ts().into(), event_id.clone());

        let replies = self
            .threads
            .entry(thread_root.clone())
            .or_insert_with(|| Messages::thread(thread_root.clone()));
        let loc = EventLocation::Message(Some(thread_root), key.clone());
        self.keys.insert(event_id, loc);
        replies.insert_message(key, msg);
    }

    /// Insert a new message event.
    pub fn insert(&mut self, msg: RoomMessageEvent) {
        match msg {
            RoomMessageEvent::Original(OriginalRoomMessageEvent {
                content: RoomMessageEventContent { relates_to: Some(ref relates_to), .. },
                ..
            }) => {
                match relates_to {
                    Relation::Replacement(repl) => self.insert_edit(repl.clone()),
                    Relation::Thread(Thread { event_id, .. }) => {
                        let event_id = event_id.clone();
                        self.insert_thread(msg, event_id);
                    },
                    Relation::Reply { .. } => self.insert_message(msg),
                    _ => self.insert_message(msg),
                }
            },
            _ => self.insert_message(msg),
        }
    }

    /// Insert a new message event, and spawn a task for image-preview if it has an image
    /// attachment.
    pub fn insert_with_preview(
        &mut self,
        room_id: OwnedRoomId,
        store: AsyncProgramStore,
        picker: Option<Picker>,
        ev: RoomMessageEvent,
        settings: &mut ApplicationSettings,
        media: matrix_sdk::Media,
    ) {
        let source = picker.and_then(|_| source_from_event(&ev));
        self.insert(ev);

        if let Some((event_id, source)) = source {
            if let (Some(msg), Some(image_preview)) =
                (self.get_event_mut(&event_id), &settings.tunables.image_preview)
            {
                msg.image_preview = ImageStatus::Downloading(image_preview.size.clone());
                spawn_insert_preview(
                    store,
                    room_id,
                    event_id,
                    source,
                    media,
                    settings.dirs.image_previews.clone(),
                )
            }
        }
    }

    /// Indicates whether we've recently fetched scrollback for this room.
    pub fn recently_fetched(&self) -> bool {
        self.fetch_last.is_some_and(|i| i.elapsed() < ROOM_FETCH_DEBOUNCE)
    }

    fn clear_receipt(&mut self, thread: &ReceiptThread, user_id: &OwnedUserId) -> Option<()> {
        let old_event_id =
            self.user_receipts.get(thread).and_then(|receipts| receipts.get(user_id))?;
        let old_thread = self.event_receipts.get_mut(thread)?;
        let old_receipts = old_thread.get_mut(old_event_id)?;
        old_receipts.remove(user_id);

        if old_receipts.is_empty() {
            old_thread.remove(old_event_id);
        }
        if old_thread.is_empty() {
            self.event_receipts.remove(thread);
        }

        None
    }

    /// Whether `event_id` sits at or before the user's current receipt for `thread`.
    ///
    /// Read receipts only ever move forward. The server hands us stale ones all the time: a
    /// scrollback fetch reports who had read each older event, and an ephemeral receipt event can
    /// race a `:read` we just performed locally. Applying those would drag the marker backwards
    /// and make an already-read room look unread again.
    ///
    /// Events we haven't loaded have no position to compare against, so those are not stale.
    fn receipt_is_stale(
        &self,
        thread: &ReceiptThread,
        user_id: &UserId,
        event_id: &EventId,
    ) -> bool {
        let Some(old_key) = self.receipt_key(thread, user_id) else {
            return false;
        };
        let Some(new_key) = self.get_message_key(event_id) else {
            return false;
        };

        new_key <= old_key
    }

    /// Where the user's receipt currently sits in each thread of this room.
    pub fn receipt_snapshot(&self, user_id: &UserId) -> HashMap<ReceiptThread, OwnedEventId> {
        self.receipts(user_id).map(|(t, e)| (t.clone(), e.clone())).collect()
    }

    /// Move the user's receipt for `thread` back to `event_id`, or drop it entirely when `None`.
    ///
    /// This deliberately skips the [RoomInfo::receipt_is_stale] check that [RoomInfo::set_receipt]
    /// applies. That check discards receipts the *server* hands back out of order, which is a
    /// different situation from this one: the only caller here is `:undoread` restoring a position
    /// the user explicitly asked to go back to. Code handling a receipt that came from the server
    /// must go through [RoomInfo::set_receipt] so that it stays guarded.
    pub fn rewind_receipt(
        &mut self,
        thread: ReceiptThread,
        user_id: OwnedUserId,
        event_id: Option<OwnedEventId>,
    ) {
        self.clear_receipt(&thread, &user_id);

        let Some(event_id) = event_id else {
            // `clear_receipt` only prunes `event_receipts`; `set_receipt` always overwrites the
            // `user_receipts` entry afterwards, but there is nothing to put back here.
            if let Some(users) = self.user_receipts.get_mut(&thread) {
                users.remove(&user_id);

                if users.is_empty() {
                    self.user_receipts.remove(&thread);
                }
            }

            return;
        };

        self.event_receipts
            .entry(thread.clone())
            .or_default()
            .entry(event_id.clone())
            .or_default()
            .insert(user_id.clone());
        self.user_receipts.entry(thread).or_default().insert(user_id, event_id);
    }

    pub fn set_receipt(
        &mut self,
        thread: ReceiptThread,
        user_id: OwnedUserId,
        event_id: OwnedEventId,
    ) {
        if self.receipt_is_stale(&thread, &user_id, &event_id) {
            return;
        }

        self.clear_receipt(&thread, &user_id);
        self.event_receipts
            .entry(thread.clone())
            .or_default()
            .entry(event_id.clone())
            .or_default()
            .insert(user_id.clone());
        self.user_receipts.entry(thread).or_default().insert(user_id, event_id);
    }

    /// Mark a thread as read, or the whole room when `thread` is `None`.
    ///
    /// This is what `:read` drives, and it is the only thing that advances the read marker when
    /// `read_receipt_manual` is set.
    pub fn mark_read(&mut self, user_id: &UserId, thread: Option<OwnedEventId>) {
        let Some(root) = thread else {
            self.fully_read(user_id);
            return;
        };

        // Fall back to the root itself for a thread whose replies aren't loaded.
        let last = self
            .threads
            .get(&root)
            .and_then(|replies| replies.last_key_value())
            .map(|((_, event_id), _)| event_id.clone())
            .unwrap_or_else(|| root.clone());

        self.set_receipt(ReceiptThread::Thread(root), user_id.to_owned(), last);
    }

    pub fn fully_read(&mut self, user_id: &UserId) {
        let Some(((_, event_id), _)) = self.messages.last_key_value() else {
            return;
        };

        self.set_receipt(ReceiptThread::Main, user_id.to_owned(), event_id.clone());

        let newest = self
            .threads
            .iter()
            .filter_map(|(thread_id, messages)| {
                let thread = ReceiptThread::Thread(thread_id.to_owned());

                messages
                    .last_key_value()
                    .map(|((_, event_id), _)| (thread, event_id.to_owned()))
            })
            .collect::<Vec<_>>();

        for (thread, event_id) in newest.into_iter() {
            self.set_receipt(thread, user_id.to_owned(), event_id.clone());
        }
    }

    pub fn receipts<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> impl Iterator<Item = (&'a ReceiptThread, &'a OwnedEventId)> + 'a {
        self.user_receipts
            .iter()
            .filter_map(move |(t, rs)| rs.get(user_id).map(|r| (t, r)))
    }

    fn get_typers(&self) -> &[OwnedUserId] {
        if let Some((t, users)) = &self.users_typing {
            if t.elapsed() < Duration::from_secs(4) {
                return users.as_ref();
            } else {
                return &[];
            }
        } else {
            return &[];
        }
    }

    fn get_typing_spans<'a>(&'a self, settings: &'a ApplicationSettings) -> Line<'a> {
        let typers = self.get_typers();
        let n = typers.len();

        match n {
            0 => Line::from(vec![]),
            1 => {
                let user = settings.get_user_span(typers[0].as_ref(), self);

                Line::from(vec![user, Span::from(" is typing...")])
            },
            2 => {
                let user1 = settings.get_user_span(typers[0].as_ref(), self);
                let user2 = settings.get_user_span(typers[1].as_ref(), self);

                Line::from(vec![
                    user1,
                    Span::raw(" and "),
                    user2,
                    Span::from(" are typing..."),
                ])
            },
            n if n < 5 => Line::from("Several people are typing..."),
            _ => Line::from("Many people are typing..."),
        }
    }

    /// Update typing information for this room.
    pub fn set_typing(&mut self, user_ids: Vec<OwnedUserId>) {
        self.users_typing = (Instant::now(), user_ids).into();
    }

    /// Create a [Rect] that displays what users are typing.
    pub fn render_typing(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        settings: &ApplicationSettings,
    ) -> Rect {
        if area.height <= 2 || area.width <= 20 {
            return area;
        }

        if !settings.tunables.typing_notice_display {
            // still keep one line blank, so `render_jump_to_recent` doesn't immediately hide the
            // last line in scrollback
            return Rect::new(area.x, area.y, area.width, area.height - 1);
        }

        let top = Rect::new(area.x, area.y, area.width, area.height - 1);
        let bar = Rect::new(area.x, area.y + top.height, area.width, 1);

        Paragraph::new(self.get_typing_spans(settings))
            .alignment(Alignment::Center)
            .render(bar, buf);

        return top;
    }

    /// Checks if a given user has reacted with the given emoji on the given event
    pub fn user_reactions_contains(
        &mut self,
        user_id: &UserId,
        event_id: &EventId,
        emoji: &str,
    ) -> bool {
        if let Some(reactions) = self.reactions.get(event_id) {
            reactions
                .values()
                .any(|(annotation, user)| annotation == emoji && user == user_id)
        } else {
            false
        }
    }
}

#[cfg(unix)]
fn picker_from_termios(protocol_type: Option<ProtocolType>) -> Option<Picker> {
    let mut picker = match Picker::from_query_stdio() {
        Ok(picker) => picker,
        Err(e) => {
            tracing::error!("Failed to setup image previews: {e}");
            return None;
        },
    };

    if let Some(protocol_type) = protocol_type {
        picker.set_protocol_type(protocol_type);
    }

    Some(picker)
}

/// Windows cannot guess the right protocol, and always needs type and font_size.
#[cfg(windows)]
fn picker_from_termios(_: Option<ProtocolType>) -> Option<Picker> {
    tracing::error!("\"image_preview\" requires \"protocol\" with \"type\" and \"font_size\" options on Windows.");
    None
}

fn picker_from_settings(settings: &ApplicationSettings) -> Option<Picker> {
    let image_preview = settings.tunables.image_preview.as_ref()?;
    let image_preview_protocol = image_preview.protocol.as_ref();

    if let Some(&ImagePreviewProtocolValues {
        r#type: Some(protocol_type),
        font_size: Some(font_size),
    }) = image_preview_protocol
    {
        // User forced type and font_size: use that.
        let mut picker = Picker::from_fontsize(font_size);
        picker.set_protocol_type(protocol_type);
        Some(picker)
    } else {
        // Guess, but use type if forced.
        picker_from_termios(image_preview_protocol.and_then(|p| p.r#type))
    }
}

/// Information gathered during server syncs about joined rooms.
#[derive(Default)]
pub struct SyncInfo {
    /// Spaces that the user is a member of.
    pub spaces: Vec<Arc<(MatrixRoom, Option<Tags>)>>,

    /// Rooms that the user is a member of.
    pub rooms: Vec<Arc<(MatrixRoom, Option<Tags>)>>,

    /// DMs that the user is a member of.
    pub dms: Vec<Arc<(MatrixRoom, Option<Tags>)>>,
}

impl SyncInfo {
    pub fn rooms(&self) -> impl Iterator<Item = &RoomId> {
        self.rooms.iter().map(|r| r.0.room_id())
    }

    pub fn dms(&self) -> impl Iterator<Item = &RoomId> {
        self.dms.iter().map(|r| r.0.room_id())
    }

    pub fn chats(&self) -> impl Iterator<Item = &RoomId> {
        self.rooms().chain(self.dms())
    }
}

static MESSAGE_NEED_TTL: u8 = 30;

#[derive(Debug, PartialEq)]
/// Load messages until the event is loaded or `ttl` loads are exceeded
pub struct MessageNeed {
    pub event_id: OwnedEventId,
    pub ttl: u8,
}

#[derive(Default, Debug, PartialEq)]
pub struct Need {
    pub members: bool,
    pub messages: Option<Vec<MessageNeed>>,
}

/// Things that need loading for different rooms.
#[derive(Default)]
pub struct RoomNeeds {
    needs: HashMap<OwnedRoomId, Need>,
}

impl RoomNeeds {
    /// Mark a room for needing to load members.
    pub fn need_members(&mut self, room_id: OwnedRoomId) {
        self.needs.entry(room_id).or_default().members = true;
    }

    /// Mark a room for needing to load messages.
    pub fn need_messages(&mut self, room_id: OwnedRoomId) {
        self.needs.entry(room_id).or_default().messages.get_or_insert_default();
    }

    /// Mark a room for needing to load messages until the given message is loaded or a retry limit
    /// is exceeded.
    pub fn need_message(&mut self, room_id: OwnedRoomId, event_id: OwnedEventId) {
        let messages = &mut self.needs.entry(room_id).or_default().messages.get_or_insert_default();

        messages.push(MessageNeed { event_id, ttl: MESSAGE_NEED_TTL });
    }

    pub fn need_messages_all(&mut self, room_id: OwnedRoomId, message_needs: Vec<MessageNeed>) {
        self.needs
            .entry(room_id)
            .or_default()
            .messages
            .get_or_insert_default()
            .extend(message_needs);
    }

    pub fn rooms(&self) -> usize {
        self.needs.len()
    }
}

impl IntoIterator for RoomNeeds {
    type Item = (OwnedRoomId, Need);
    type IntoIter = IntoIter<OwnedRoomId, Need>;

    fn into_iter(self) -> Self::IntoIter {
        self.needs.into_iter()
    }
}

/// The main application state.
pub struct ChatStore {
    /// `:`-commands
    pub cmds: ProgramCommands,

    /// Handle for communicating w/ the worker thread.
    pub worker: Requester,

    /// Map of joined rooms.
    pub rooms: CompletionMap<OwnedRoomId, RoomInfo>,

    /// Map of room names.
    pub names: CompletionMap<String, OwnedRoomId>,

    /// Presence information for other users.
    pub presences: CompletionMap<OwnedUserId, PresenceState>,

    /// In-progress and completed verifications.
    pub verifications: HashMap<String, SasVerification>,

    /// Settings for the current profile loaded from config file.
    pub settings: ApplicationSettings,

    /// When each deferred room or thread comes back to the inbox.
    ///
    /// Held here rather than on [RoomInfo] because a thread's wake time must be reachable while
    /// building the inbox list, where the rooms map is already borrowed.
    pub snooze: SnoozeStore,

    /// Set of rooms that need more messages loaded in their scrollback.
    pub need_load: RoomNeeds,

    /// Information gathered by the background thread.
    pub sync_info: SyncInfo,

    /// Image preview "protocol" picker.
    pub picker: Option<Picker>,

    /// Last draw time, used to match with RoomInfo's draw_last.
    pub draw_curr: Option<Instant>,

    /// Whether to ring the terminal bell on the next redraw.
    pub ring_bell: bool,

    /// Whether the application is currently focused
    pub focused: bool,

    /// Collator for locale-aware text sorting.
    pub collator: feruca::Collator,

    /// Notifications that should be dismissed when the user opens the room.
    pub open_notifications: HashMap<OwnedRoomId, Vec<NotificationHandle>>,

    /// Read operations that `:undoread` can walk back through, oldest first.
    pub read_undos: Vec<ReadUndoEntry>,

    /// Where a clicked desktop notification wants the main loop to take the user.
    ///
    /// Notifications are handled off the main loop, which is blocked reading terminal input, so
    /// the click leaves the target here and the loop picks it up on its next pass. Only the most
    /// recent click is kept: when several notifications are clicked in quick succession, the user
    /// only ends up looking at one of them anyway.
    pub notification_jump: Option<NotificationJump>,
}

/// Where a clicked desktop notification takes the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationJump {
    /// The window to switch to: the room, or the thread the notified message lives in.
    pub window: IambId,

    /// The notified message, to be selected once the window is showing it.
    pub event_id: OwnedEventId,
}

/// How many read operations `:undoread` can walk back through.
///
/// The stack lives in memory only, so this exists to bound it over a long-running session rather
/// than to enforce any policy.
pub const READ_UNDO_STACK_LIMIT: usize = 64;

/// One receipt that a read operation moved, and where it sat beforehand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadUndoTarget {
    pub room_id: OwnedRoomId,
    pub thread: ReceiptThread,

    /// Where the receipt was before the read, or `None` when there was no receipt at all.
    pub previous: Option<OwnedEventId>,
}

/// One undoable read operation.
///
/// A bulk read (`:read all`, `:unreads clear`) touches many receipts across many rooms, but it is
/// a single thing the user did, so it becomes a single entry that one `:undoread` reverses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadUndoEntry {
    pub targets: Vec<ReadUndoTarget>,
}

impl ChatStore {
    /// Now, as the wake times are measured.
    pub fn now_ms(&self) -> WakeTime {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as WakeTime)
            .unwrap_or(0)
    }

    /// Read a snooze duration against the current time and the configured hour for "tomorrow".
    ///
    /// The error names what the user typed, so that a mistyped duration is obvious.
    pub fn parse_snooze(&self, when: &str) -> Result<WakeTime, IambError> {
        let when = if when.trim().is_empty() {
            self.settings.tunables.snooze_default.as_str()
        } else {
            when
        };

        parse_when(when, self.now_ms(), self.settings.tunables.snooze_tomorrow_hour)
            .map_err(|e| IambError::BadSnoozeDuration(e.to_string()))
    }

    /// Whether an inbox entry is deferred right now.
    pub fn is_deferred(&self, room_id: &OwnedRoomId, thread: Option<&OwnedEventId>) -> bool {
        self.snooze.is_deferred(room_id, thread, self.now_ms())
    }

    /// Create a new [ChatStore].
    pub fn new(worker: Requester, settings: ApplicationSettings) -> Self {
        let picker = picker_from_settings(&settings);

        ChatStore {
            worker,
            settings,
            snooze: SnoozeStore::default(),
            picker,
            cmds: crate::commands::setup_commands(),

            collator: Default::default(),
            names: Default::default(),
            rooms: Default::default(),
            presences: Default::default(),
            verifications: Default::default(),
            need_load: Default::default(),
            sync_info: Default::default(),
            draw_curr: None,
            ring_bell: false,
            focused: true,
            open_notifications: Default::default(),
            read_undos: Default::default(),
            notification_jump: None,
        }
    }

    /// Snapshot the receipts in `room_ids`, run `apply`, and record what moved for `:undoread`.
    ///
    /// Only receipts that actually changed are recorded, so a bulk read over mostly-read rooms
    /// still produces one small entry, and a read that changed nothing produces no entry at all.
    ///
    /// This wraps the user-initiated read paths only. Receipts that advance because the user
    /// merely looked at a room are not recorded: with `read_receipt_manual` off that happens
    /// constantly, and burying the deliberate reads under it would make the stack useless.
    pub fn record_read<F>(&mut self, room_ids: Vec<OwnedRoomId>, apply: F)
    where
        F: FnOnce(&mut Self),
    {
        let user_id = self.settings.profile.user_id.clone();
        let before = room_ids
            .into_iter()
            .map(|room_id| {
                let snapshot = self
                    .rooms
                    .get(&room_id)
                    .map(|i| i.receipt_snapshot(&user_id))
                    .unwrap_or_default();

                (room_id, snapshot)
            })
            .collect::<Vec<_>>();

        apply(self);

        let mut targets = Vec::new();

        for (room_id, before) in before {
            let Some(info) = self.rooms.get(&room_id) else {
                continue;
            };

            for (thread, current) in info.receipts(&user_id) {
                let previous = before.get(thread);

                if previous == Some(current) {
                    continue;
                }

                targets.push(ReadUndoTarget {
                    room_id: room_id.clone(),
                    thread: thread.clone(),
                    previous: previous.cloned(),
                });
            }
        }

        if targets.is_empty() {
            return;
        }

        if self.read_undos.len() >= READ_UNDO_STACK_LIMIT {
            self.read_undos.remove(0);
        }

        self.read_undos.push(ReadUndoEntry { targets });
    }

    /// Put the receipts moved by the most recent recorded read operation back where they were.
    pub fn undo_read(&mut self) -> Option<ReadUndoEntry> {
        let entry = self.read_undos.pop()?;
        let user_id = self.settings.profile.user_id.clone();

        for target in entry.targets.iter() {
            let info = self.rooms.get_or_default(target.room_id.clone());
            info.rewind_receipt(target.thread.clone(), user_id.clone(), target.previous.clone());
        }

        Some(entry)
    }

    /// Get a joined room.
    pub fn get_joined_room(&self, room_id: &RoomId) -> Option<MatrixRoom> {
        let room = self.worker.client.get_room(room_id)?;

        if room.state() == MatrixRoomState::Joined {
            Some(room)
        } else {
            None
        }
    }

    /// Get the title for a room.
    pub fn get_room_title(&self, room_id: &RoomId) -> String {
        self.rooms
            .get(room_id)
            .and_then(|i| i.name.as_ref())
            .map(String::from)
            .unwrap_or_else(|| "Untitled Matrix Room".to_string())
    }

    /// Get the [RoomInfo] for a given room identifier.
    pub fn get_room_info(&mut self, room_id: OwnedRoomId) -> &mut RoomInfo {
        self.rooms.get_or_default(room_id)
    }

    /// Set the name for a room.
    pub fn set_room_name(&mut self, room_id: &RoomId, name: &str) {
        self.rooms.get_or_default(room_id.to_owned()).name = name.to_string().into();
    }

    /// Insert a new E2EE verification.
    pub fn insert_sas(&mut self, sas: SasVerification) {
        let key = format!("{}/{}", sas.other_user_id(), sas.other_device().device_id());

        self.verifications.insert(key, sas);
    }
}

impl ApplicationStore for ChatStore {}

/// Identified used to track window content.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IambId {
    /// A Matrix room, with an optional thread to show.
    Room(OwnedRoomId, Option<OwnedEventId>),

    /// The `:dms` window.
    DirectList,

    /// The `:members` window for a given Matrix room.
    MemberList(OwnedRoomId),

    /// The `:rooms` window.
    RoomList,

    /// The `:spaces` window.
    SpaceList,

    /// The `:verify` window.
    VerifyList,

    /// The `:welcome` window.
    Welcome,

    /// The `:chats` window.
    ChatList,

    /// The `:unreads` window.
    UnreadList,

    /// The `:threads` window.
    ThreadList,

    /// The `:unreads-and-threads` window.
    UnreadThreadList,

    /// The `:commands` window.
    CommandPalette,

    /// The `:switch` window.
    QuickSwitcher,
}

impl Display for IambId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IambId::Room(room_id, None) => {
                write!(f, "iamb://room/{room_id}")
            },
            IambId::Room(room_id, Some(thread)) => {
                write!(f, "iamb://room/{room_id}/threads/{thread}")
            },
            IambId::MemberList(room_id) => {
                write!(f, "iamb://members/{room_id}")
            },
            IambId::DirectList => f.write_str("iamb://dms"),
            IambId::RoomList => f.write_str("iamb://rooms"),
            IambId::SpaceList => f.write_str("iamb://spaces"),
            IambId::VerifyList => f.write_str("iamb://verify"),
            IambId::Welcome => f.write_str("iamb://welcome"),
            IambId::ChatList => f.write_str("iamb://chats"),
            IambId::UnreadList => f.write_str("iamb://unreads"),
            IambId::ThreadList => f.write_str("iamb://threads"),
            IambId::UnreadThreadList => f.write_str("iamb://unreads-and-threads"),
            IambId::CommandPalette => f.write_str("iamb://commands"),
            IambId::QuickSwitcher => f.write_str("iamb://switch"),
        }
    }
}

impl ApplicationWindowId for IambId {}

impl Serialize for IambId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for IambId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(IambIdVisitor)
    }
}

/// [serde] visitor for deserializing [IambId].
struct IambIdVisitor;

impl Visitor<'_> for IambIdVisitor {
    type Value = IambId;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid window URL")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        let Ok(url) = Url::parse(value) else {
            return Err(E::custom("Invalid iamb window URL"));
        };

        if url.scheme() != "iamb" {
            return Err(E::custom("Invalid iamb window URL"));
        }

        match url.domain() {
            Some("room") => {
                let Some(path) = url.path_segments() else {
                    return Err(E::custom("Invalid members window URL"));
                };

                match *path.collect::<Vec<_>>().as_slice() {
                    [room_id] => {
                        let Ok(room_id) = OwnedRoomId::try_from(room_id) else {
                            return Err(E::custom("Invalid room identifier"));
                        };

                        Ok(IambId::Room(room_id, None))
                    },
                    [room_id, "threads", thread_root] => {
                        let Ok(room_id) = OwnedRoomId::try_from(room_id) else {
                            return Err(E::custom("Invalid room identifier"));
                        };

                        let Ok(thread_root) = OwnedEventId::try_from(thread_root) else {
                            return Err(E::custom("Invalid thread root identifier"));
                        };

                        Ok(IambId::Room(room_id, Some(thread_root)))
                    },
                    _ => return Err(E::custom("Invalid members window URL")),
                }
            },
            Some("members") => {
                let Some(path) = url.path_segments() else {
                    return Err(E::custom("Invalid members window URL"));
                };

                let &[room_id] = path.collect::<Vec<_>>().as_slice() else {
                    return Err(E::custom("Invalid members window URL"));
                };

                let Ok(room_id) = OwnedRoomId::try_from(room_id) else {
                    return Err(E::custom("Invalid room identifier"));
                };

                Ok(IambId::MemberList(room_id))
            },
            Some("dms") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://dms takes no path"));
                }

                Ok(IambId::DirectList)
            },
            Some("rooms") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://rooms takes no path"));
                }

                Ok(IambId::RoomList)
            },
            Some("spaces") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://spaces takes no path"));
                }

                Ok(IambId::SpaceList)
            },
            Some("verify") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://verify takes no path"));
                }

                Ok(IambId::VerifyList)
            },
            Some("welcome") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://welcome takes no path"));
                }

                Ok(IambId::Welcome)
            },
            Some("chats") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://chats takes no path"));
                }

                Ok(IambId::ChatList)
            },
            Some("unreads") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://unreads takes no path"));
                }

                Ok(IambId::UnreadList)
            },
            Some("threads") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://threads takes no path"));
                }

                Ok(IambId::ThreadList)
            },
            Some("unreads-and-threads") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://unreads-and-threads takes no path"));
                }

                Ok(IambId::UnreadThreadList)
            },
            Some("commands") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://commands takes no path"));
                }

                Ok(IambId::CommandPalette)
            },
            Some("switch") => {
                if url.path() != "" {
                    return Err(E::custom("iamb://switch takes no path"));
                }

                Ok(IambId::QuickSwitcher)
            },
            Some(s) => Err(E::custom(format!("{s:?} is not a valid window"))),
            None => Err(E::custom("Invalid iamb window URL")),
        }
    }
}

/// Which part of the room window's UI is focused.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RoomFocus {
    /// The scrollback for a room window is focused.
    Scrollback,

    /// The message bar for a room window is focused.
    MessageBar,
}

impl RoomFocus {
    /// Whether this is [RoomFocus::Scrollback].
    pub fn is_scrollback(&self) -> bool {
        matches!(self, RoomFocus::Scrollback)
    }

    /// Whether this is [RoomFocus::MessageBar].
    pub fn is_msgbar(&self) -> bool {
        matches!(self, RoomFocus::MessageBar)
    }

    pub fn toggle(&mut self) {
        *self = match self {
            RoomFocus::MessageBar => RoomFocus::Scrollback,
            RoomFocus::Scrollback => RoomFocus::MessageBar,
        };
    }
}

/// Identifiers used to track where a mark was placed.
///
/// While this is the "buffer identifier" for the mark,
/// not all of these are necessarily actual buffers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IambBufferId {
    /// The command bar buffer.
    Command(CommandType),

    /// The message buffer or a specific message in a room.
    Room(OwnedRoomId, Option<OwnedEventId>, RoomFocus),

    /// The `:dms` window.
    DirectList,

    /// The `:members` window for a room.
    MemberList(OwnedRoomId),

    /// The `:rooms` window.
    RoomList,

    /// The `:spaces` window.
    SpaceList,

    /// The `:verify` window.
    VerifyList,

    /// The buffer for the `:rooms` window.
    Welcome,

    /// The `:chats` window.
    ChatList,

    /// The `:unreads` window.
    UnreadList,

    /// The `:threads` window.
    ThreadList,

    /// The `:unreads-and-threads` window.
    UnreadThreadList,

    /// The command palette's entry list.
    CommandPaletteList,

    /// The command palette's filter bar.
    CommandPaletteFilter,

    /// The quick switcher's entry list.
    QuickSwitcherList,

    /// The quick switcher's filter bar.
    QuickSwitcherFilter,
}

impl IambBufferId {
    /// Get the identifier for the window that contains this buffer.
    pub fn to_window(&self) -> Option<IambId> {
        let id = match self {
            IambBufferId::Command(_) => return None,
            IambBufferId::Room(room, thread, _) => IambId::Room(room.clone(), thread.clone()),
            IambBufferId::DirectList => IambId::DirectList,
            IambBufferId::MemberList(room) => IambId::MemberList(room.clone()),
            IambBufferId::RoomList => IambId::RoomList,
            IambBufferId::SpaceList => IambId::SpaceList,
            IambBufferId::VerifyList => IambId::VerifyList,
            IambBufferId::Welcome => IambId::Welcome,
            IambBufferId::ChatList => IambId::ChatList,
            IambBufferId::UnreadList => IambId::UnreadList,
            IambBufferId::ThreadList => IambId::ThreadList,
            IambBufferId::UnreadThreadList => IambId::UnreadThreadList,
            IambBufferId::CommandPaletteList => IambId::CommandPalette,
            IambBufferId::CommandPaletteFilter => IambId::CommandPalette,
            IambBufferId::QuickSwitcherList => IambId::QuickSwitcher,
            IambBufferId::QuickSwitcherFilter => IambId::QuickSwitcher,
        };

        Some(id)
    }
}

impl ApplicationContentId for IambBufferId {}

impl ApplicationInfo for IambInfo {
    type Error = IambError;
    type Store = ChatStore;
    type Action = IambAction;
    type WindowId = IambId;
    type ContentId = IambBufferId;

    fn content_of_command(ct: CommandType) -> IambBufferId {
        IambBufferId::Command(ct)
    }
}

pub struct IambCompleter;

impl Completer<IambInfo> for IambCompleter {
    fn complete(
        &mut self,
        text: &EditRope,
        cursor: &mut Cursor,
        content: &IambBufferId,
        store: &mut ChatStore,
    ) -> Vec<String> {
        match content {
            IambBufferId::Command(CommandType::Command) => complete_cmdbar(text, cursor, store),
            IambBufferId::Command(CommandType::Search) => vec![],
            IambBufferId::Room(room_id, _, RoomFocus::MessageBar) => {
                complete_msgbar(text, cursor, room_id, store)
            },
            IambBufferId::Room(_, _, RoomFocus::Scrollback) => vec![],

            IambBufferId::DirectList => vec![],
            IambBufferId::MemberList(_) => vec![],
            IambBufferId::RoomList => vec![],
            IambBufferId::SpaceList => vec![],
            IambBufferId::VerifyList => vec![],
            IambBufferId::Welcome => vec![],
            IambBufferId::ChatList => vec![],
            IambBufferId::UnreadList => vec![],
            IambBufferId::ThreadList => vec![],
            IambBufferId::UnreadThreadList => vec![],
            IambBufferId::CommandPaletteList => vec![],
            IambBufferId::CommandPaletteFilter => vec![],
            IambBufferId::QuickSwitcherList => vec![],
            IambBufferId::QuickSwitcherFilter => vec![],
        }
    }
}

/// Tab completion for user IDs.
fn complete_users(text: &EditRope, cursor: &mut Cursor, store: &ChatStore) -> Vec<String> {
    let id = text
        .get_prefix_word_mut(cursor, &MATRIX_ID_WORD)
        .unwrap_or_else(EditRope::empty);
    let id = Cow::from(&id);

    store
        .presences
        .complete(id.as_ref())
        .into_iter()
        .map(|i| i.to_string())
        .collect()
}

/// Whether somebody with this membership is still around for a mention to reach.
fn is_mentionable(membership: &MembershipState) -> bool {
    matches!(membership, MembershipState::Join | MembershipState::Invite)
}

/// Completion for mentioning somebody in the room being composed in.
///
/// The candidates come from the same place the `:members` window gets its list, so anybody who can
/// be seen in that window can be mentioned, not just the people who happen to have spoken in the
/// loaded scrollback. Unlike that window, though, people who have left or been banned are left out:
/// there is no point offering to notify somebody who is no longer here.
fn complete_mention(needle: &str, room_id: &RoomId, store: &ChatStore) -> Vec<String> {
    let Ok(members) = store.worker.members(room_id.to_owned()) else {
        return vec![];
    };

    let candidates = members
        .into_iter()
        .filter(|member| is_mentionable(member.membership()))
        .map(|member| {
            let display_name = member.display_name().map(ToString::to_string);

            MentionCandidate::new(
                member.user_id().to_owned(),
                display_name,
                member.name_ambiguous(),
            )
        })
        .collect();

    complete_mentions(needle, candidates)
}

/// Tab completion within the message bar.
fn complete_msgbar(
    text: &EditRope,
    cursor: &mut Cursor,
    room_id: &RoomId,
    store: &ChatStore,
) -> Vec<String> {
    let id = text
        .get_prefix_word_mut(cursor, &MATRIX_ID_WORD)
        .unwrap_or_else(EditRope::empty);
    let id = Cow::from(&id);

    match id.chars().next() {
        // Complete room aliases.
        Some('#') => {
            return store.names.complete(id.as_ref());
        },

        // Complete room identifiers.
        Some('!') => {
            return store
                .rooms
                .complete(id.as_ref())
                .into_iter()
                .map(|i| i.to_string())
                .collect();
        },

        // Complete Emoji shortcodes.
        Some(EMOJI_SIGIL) => {
            return complete_emojis(id.as_ref());
        },

        // Complete a mention of somebody in this room.
        Some(MENTION_SIGIL) => {
            return complete_mention(id.as_ref(), room_id, store);
        },

        // Complete usernames when there's nothing to go on but the cursor position.
        None => {
            return store
                .presences
                .complete(id.as_ref())
                .into_iter()
                .map(|i| i.to_string())
                .collect();
        },

        // Unknown sigil.
        Some(_) => return vec![],
    }
}

/// Tab completion for Matrix identifiers (usernames, room aliases, etc.)
fn complete_matrix_names(text: &EditRope, cursor: &mut Cursor, store: &ChatStore) -> Vec<String> {
    let id = text
        .get_prefix_word_mut(cursor, &MATRIX_ID_WORD)
        .unwrap_or_else(EditRope::empty);
    let id = Cow::from(&id);

    let list = store.names.complete(id.as_ref());
    if !list.is_empty() {
        return list;
    }

    let list = store.presences.complete(id.as_ref());
    if !list.is_empty() {
        return list.into_iter().map(|i| i.to_string()).collect();
    }

    store
        .rooms
        .complete(id.as_ref())
        .into_iter()
        .map(|i| i.to_string())
        .collect()
}

/// Tab completion for Emoji shortcode names.
///
/// The word style is the one used for Matrix identifiers because it treats `:` as part of a word,
/// which lets the argument be written either as `smile` or as `:smile`. The sigil is optional here
/// since the command already says an Emoji is what is wanted.
fn complete_emoji(text: &EditRope, cursor: &mut Cursor) -> Vec<String> {
    let sc = text.get_prefix_word_mut(cursor, &MATRIX_ID_WORD);
    let sc = sc.unwrap_or_else(EditRope::empty);
    let sc = Cow::from(&sc);

    complete_emoji_names(sc.as_ref())
}

/// Tab completion for command names.
fn complete_cmdname(
    desc: CommandDescription,
    text: &EditRope,
    cursor: &mut Cursor,
    store: &ChatStore,
) -> Vec<String> {
    // Complete command name and set cursor position.
    let _ = text.get_prefix_word_mut(cursor, &WordStyle::Little);
    store.cmds.complete_name(desc.command.as_str())
}

/// Tab completion for command arguments.
fn complete_cmdarg(
    desc: CommandDescription,
    text: &EditRope,
    cursor: &mut Cursor,
    store: &ChatStore,
) -> Vec<String> {
    let cmd = match store.cmds.get(desc.command.as_str()) {
        Ok(cmd) => cmd,
        Err(_) => return vec![],
    };

    match cmd.name.as_str() {
        "cancel" | "dms" | "edit" | "redact" | "reply" => vec![],
        "members" | "rooms" | "spaces" | "welcome" => vec![],
        "download" | "keys" | "open" | "upload" => complete_path(text, cursor),
        "react" | "unreact" => complete_emoji(text, cursor),

        "invite" => complete_users(text, cursor, store),
        "join" | "split" | "vsplit" | "tabedit" => complete_matrix_names(text, cursor, store),
        "room" => vec![],
        "verify" => vec![],
        "vertical" | "horizontal" | "aboveleft" | "belowright" | "tab" => {
            complete_cmd(desc.arg.text.as_str(), text, cursor, store)
        },
        _ => vec![],
    }
}

/// Tab completion for commands.
fn complete_cmd(cmd: &str, text: &EditRope, cursor: &mut Cursor, store: &ChatStore) -> Vec<String> {
    match CommandDescription::from_str(cmd) {
        Ok(desc) => {
            if desc.arg.untrimmed.is_empty() {
                complete_cmdname(desc, text, cursor, store)
            } else {
                // Complete command argument.
                complete_cmdarg(desc, text, cursor, store)
            }
        },

        // Can't parse command text, so return zero completions.
        Err(_) => vec![],
    }
}

/// Tab completion for the command bar.
fn complete_cmdbar(text: &EditRope, cursor: &mut Cursor, store: &ChatStore) -> Vec<String> {
    let eo = text.cursor_to_offset(cursor);
    let slice = text.slice(..eo);
    let cow = Cow::from(&slice);

    complete_cmd(cow.as_ref(), text, cursor, store)
}

#[cfg(test)]
pub mod tests {
    use std::iter::FromIterator as _;

    use super::*;
    use crate::config::user_style_from_color;
    use crate::tests::*;
    use matrix_sdk::ruma::{
        events::{reaction::ReactionEventContent, relation::Annotation},
        owned_event_id,
        server_name,
        MilliSecondsSinceUnixEpoch,
        UInt,
    };
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use serde_json::{Map, Value};

    /// A room with two threads: one the test user replied in, and one they never touched.
    fn mock_room_with_threads() -> (RoomInfo, ApplicationSettings, OwnedEventId, OwnedEventId) {
        let settings = mock_settings();
        let user_id = settings.profile.user_id.clone();
        let stranger = TEST_USER2.clone();

        let followed_root = EventId::new_v1(server_name!("example.com"));
        let ignored_root = EventId::new_v1(server_name!("example.com"));

        let mut room = RoomInfo::default();

        let insert = |info: &mut RoomInfo, key: MessageKey, sender: OwnedUserId, body: &str| {
            let content = MessageEvent::Local(
                key.1.clone(),
                RoomMessageEventContent::text_plain(body).into(),
            );
            let root = match info.keys.get(&key.1) {
                Some(EventLocation::Message(root, _)) => root.clone(),
                _ => None,
            };
            info.get_thread_mut(root)
                .insert(key.clone(), Message::new(content, sender, key.0));
        };

        // Both thread roots live in the main scrollback.
        let roots: Vec<OwnedEventId> = vec![followed_root.clone(), ignored_root.clone()];

        for (i, root) in roots.iter().enumerate() {
            let ts = MessageTimeStamp::OriginServer(UInt::new(i as u64 + 1).unwrap());
            let key: MessageKey = (ts, root.clone());
            room.keys.insert(root.clone(), EventLocation::Message(None, key.clone()));
            insert(&mut room, key, stranger.clone(), "thread root");
        }

        // A reply from the test user in the first thread, and from someone else in the second.
        let repliers: Vec<OwnedUserId> = vec![user_id, stranger];

        for (i, (root, sender)) in roots.iter().zip(repliers).enumerate() {
            let reply_id = EventId::new_v1(server_name!("example.com"));
            let ts = MessageTimeStamp::OriginServer(UInt::new(i as u64 + 10).unwrap());
            let key: MessageKey = (ts, reply_id.clone());
            room.keys
                .insert(reply_id, EventLocation::Message(Some(root.clone()), key.clone()));
            insert(&mut room, key, sender, "reply");
        }

        (room, settings, followed_root, ignored_root)
    }

    #[test]
    fn test_followed_threads_requires_participation() {
        let (room, settings, followed_root, ignored_root) = mock_room_with_threads();

        let followed = room.followed_threads(&settings);
        let roots = followed.iter().map(|t| t.root.clone()).collect::<Vec<_>>();

        assert_eq!(roots, vec![followed_root]);
        assert!(!roots.contains(&ignored_root));

        // With no receipts at all, a followed thread reads as unread.
        assert!(followed[0].unread.is_unread());
        assert_eq!(followed[0].preview, "thread root");
    }

    #[test]
    fn test_followed_threads_includes_subscribed_thread() {
        let (mut room, settings, _, ignored_root) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();

        // A thread-scoped receipt stands in for a thread subscription, even though the user
        // never posted in the thread.
        let last_reply = room
            .get_thread(Some(&ignored_root))
            .and_then(|t| t.last_key_value())
            .map(|((_, event_id), _)| event_id.clone())
            .unwrap();
        room.set_receipt(ReceiptThread::Thread(ignored_root.clone()), user_id, last_reply);

        let followed = room.followed_threads(&settings);
        let roots = followed.iter().map(|t| t.root.clone()).collect::<Vec<_>>();

        assert!(roots.contains(&ignored_root));

        let subscribed = followed.iter().find(|t| t.root == ignored_root).unwrap();
        assert!(!subscribed.unread.is_unread());
    }

    #[test]
    fn test_thread_unreads_honors_main_receipt() {
        let (mut room, settings, followed_root, _) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();

        assert!(room.thread_unreads(&followed_root, &settings).is_unread());

        // A client that doesn't send threaded receipts advances only the main receipt, which
        // still counts as having read the thread.
        let last_reply = room
            .get_thread(Some(&followed_root))
            .and_then(|t| t.last_key_value())
            .map(|((_, event_id), _)| event_id.clone())
            .unwrap();
        room.set_receipt(ReceiptThread::Main, user_id, last_reply);

        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());
    }

    #[test]
    fn test_mark_read_scopes_to_thread() {
        let (mut room, settings, followed_root, ignored_root) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();

        // Subscribe to the second thread so that both show up as followed and unread.
        room.set_receipt(
            ReceiptThread::Thread(ignored_root.clone()),
            user_id.clone(),
            ignored_root.clone(),
        );

        assert!(room.thread_unreads(&followed_root, &settings).is_unread());
        assert!(room.thread_unreads(&ignored_root, &settings).is_unread());

        // Marking one thread read leaves the other alone.
        room.mark_read(&user_id, Some(followed_root.clone()));
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());
        assert!(room.thread_unreads(&ignored_root, &settings).is_unread());

        // Marking the room read with no thread covers every thread in it.
        room.mark_read(&user_id, None);
        assert!(!room.thread_unreads(&ignored_root, &settings).is_unread());
        assert!(!room.unreads(&settings).is_unread());
    }

    #[test]
    fn test_stale_receipts_do_not_unmark_read() {
        let (mut room, settings, followed_root, _) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();

        room.mark_read(&user_id, None);
        assert!(!room.unreads(&settings).is_unread());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // Scrollback fetches and ephemeral receipt events replay whatever the server knows,
        // which can be a receipt we have already advanced past locally.
        room.set_receipt(ReceiptThread::Main, user_id.clone(), followed_root.clone());
        assert!(!room.unreads(&settings).is_unread());

        room.set_receipt(
            ReceiptThread::Thread(followed_root.clone()),
            user_id.clone(),
            followed_root.clone(),
        );
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // Receipts that genuinely move forward still apply.
        let (mut room, settings, followed_root, ignored_root) = mock_room_with_threads();

        room.set_receipt(ReceiptThread::Main, user_id.clone(), followed_root);
        assert!(room.unreads(&settings).is_unread());

        room.set_receipt(ReceiptThread::Main, user_id, ignored_root);
        assert!(!room.unreads(&settings).is_unread());
    }

    #[test]
    fn test_thread_receipt_survives_reload() {
        let (mut room, settings, followed_root, _) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();

        room.mark_read(&user_id, Some(followed_root.clone()));
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // What the worker pushes to the server, and what it later reads back out of the client's
        // local store, is keyed by thread.
        let sent = room.receipts(&user_id).collect::<HashMap<_, _>>();
        let thread = ReceiptThread::Thread(followed_root.clone());
        let stored = sent.get(&thread).cloned().cloned().unwrap();

        // Restarting rebuilds the scrollback from scratch and keeps no receipts: an incremental
        // sync won't repeat them, so the thread is unread again until we load them back.
        room.user_receipts.clear();
        room.event_receipts.clear();
        assert!(room.thread_unreads(&followed_root, &settings).is_unread());

        // The worker only asks the store about threads it knows the roots of.
        let roots = room.thread_roots().cloned().collect::<Vec<_>>();
        assert!(roots.contains(&followed_root));

        room.set_receipt(thread.clone(), user_id.clone(), stored.clone());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // A restore that races a newer local `:read` must not drag the marker backwards.
        room.set_receipt(thread, user_id, followed_root.clone());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());
    }

    /// A [ChatStore] holding a single room with two followed threads.
    async fn mock_store_with_threads() -> (ChatStore, OwnedRoomId, OwnedEventId, OwnedEventId) {
        let (room, _, followed_root, ignored_root) = mock_room_with_threads();
        let mut store = mock_store().await.application;
        let room_id = TEST_ROOM1_ID.clone();

        store.rooms.insert(room_id.clone(), room);

        (store, room_id, followed_root, ignored_root)
    }

    #[test]
    fn test_rewind_receipt_bypasses_stale_guard() {
        let (mut room, settings, followed_root, _) = mock_room_with_threads();
        let user_id = settings.profile.user_id.clone();
        let thread = ReceiptThread::Thread(followed_root.clone());

        room.mark_read(&user_id, Some(followed_root.clone()));
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // `set_receipt` still refuses to walk the marker back, because that is what protects us
        // from receipts the server replays out of order.
        room.set_receipt(thread.clone(), user_id.clone(), followed_root.clone());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        // The explicit rewind path does move it back.
        room.rewind_receipt(thread.clone(), user_id.clone(), Some(followed_root.clone()));
        assert!(room.thread_unreads(&followed_root, &settings).is_unread());

        // Rewinding to `None` drops the receipt entirely.
        room.rewind_receipt(thread.clone(), user_id.clone(), None);
        assert_eq!(room.receipt_snapshot(&user_id).get(&thread), None);
    }

    #[tokio::test]
    async fn test_undo_read_is_scoped_to_the_thread_that_was_read() {
        let (mut store, room_id, followed_root, ignored_root) = mock_store_with_threads().await;
        let user_id = store.settings.profile.user_id.clone();
        let settings = store.settings.clone();

        // Subscribe to the second thread so both are followed and unread.
        store.rooms.get_or_default(room_id.clone()).set_receipt(
            ReceiptThread::Thread(ignored_root.clone()),
            user_id.clone(),
            ignored_root.clone(),
        );

        store.record_read(vec![room_id.clone()], |app| {
            app.rooms
                .get_or_default(room_id.clone())
                .mark_read(&user_id, Some(followed_root.clone()));
        });

        let room = store.rooms.get(&room_id).unwrap();
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        let entry = store.undo_read().unwrap();
        assert_eq!(entry.targets.len(), 1);
        assert_eq!(entry.targets[0].thread, ReceiptThread::Thread(followed_root.clone()));
        assert_eq!(entry.targets[0].previous, None);

        // The read is undone, and the room's other threads and its main timeline are untouched.
        let room = store.rooms.get(&room_id).unwrap();
        assert!(room.thread_unreads(&followed_root, &settings).is_unread());

        let snapshot = room.receipt_snapshot(&user_id);
        assert_eq!(snapshot.get(&ReceiptThread::Thread(ignored_root.clone())), Some(&ignored_root));
        assert_eq!(snapshot.get(&ReceiptThread::Main), None);

        // The stack is now empty.
        assert_eq!(store.undo_read(), None);
    }

    #[tokio::test]
    async fn test_undo_read_reverses_a_bulk_read_in_one_step() {
        let (mut store, room_id, followed_root, ignored_root) = mock_store_with_threads().await;
        let user_id = store.settings.profile.user_id.clone();
        let settings = store.settings.clone();

        store.record_read(vec![room_id.clone()], |app| {
            app.rooms.get_or_default(room_id.clone()).fully_read(&user_id);
        });

        let room = store.rooms.get(&room_id).unwrap();
        assert!(!room.unreads(&settings).is_unread());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());
        assert!(!room.thread_unreads(&ignored_root, &settings).is_unread());

        // One bulk read is one entry, however many receipts it moved.
        assert_eq!(store.read_undos.len(), 1);
        assert!(store.read_undos[0].targets.len() > 1);

        let entry = store.undo_read().unwrap();
        assert!(entry.targets.iter().all(|t| t.previous.is_none()));

        let room = store.rooms.get(&room_id).unwrap();
        assert!(room.unreads(&settings).is_unread());
        assert!(room.thread_unreads(&followed_root, &settings).is_unread());
        assert!(room.thread_unreads(&ignored_root, &settings).is_unread());
    }

    #[tokio::test]
    async fn test_repeated_reads_undo_one_at_a_time() {
        let (mut store, room_id, followed_root, ignored_root) = mock_store_with_threads().await;
        let user_id = store.settings.profile.user_id.clone();
        let settings = store.settings.clone();

        for root in [&followed_root, &ignored_root] {
            store.record_read(vec![room_id.clone()], |app| {
                app.rooms
                    .get_or_default(room_id.clone())
                    .mark_read(&user_id, Some(root.clone()));
            });
        }

        assert_eq!(store.read_undos.len(), 2);

        // Reading a thread that is already read moves nothing, so it records nothing.
        store.record_read(vec![room_id.clone()], |app| {
            app.rooms
                .get_or_default(room_id.clone())
                .mark_read(&user_id, Some(ignored_root.clone()));
        });
        assert_eq!(store.read_undos.len(), 2);

        // Undoing walks back through the reads in reverse order.
        store.undo_read().unwrap();
        let room = store.rooms.get(&room_id).unwrap();
        assert!(room.thread_unreads(&ignored_root, &settings).is_unread());
        assert!(!room.thread_unreads(&followed_root, &settings).is_unread());

        store.undo_read().unwrap();
        let room = store.rooms.get(&room_id).unwrap();
        assert!(room.thread_unreads(&followed_root, &settings).is_unread());
    }

    #[test]
    fn test_thread_window_urls() {
        for (id, url) in [
            (IambId::ThreadList, "iamb://threads"),
            (IambId::UnreadThreadList, "iamb://unreads-and-threads"),
        ] {
            assert_eq!(id.to_string(), url);

            let json = serde_json::to_string(&id).unwrap();
            let parsed: IambId = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, id);
        }

        assert!(serde_json::from_str::<IambId>("\"iamb://threads/extra\"").is_err());
    }

    fn create_reaction_event(
        content: &ReactionEventContent,
        event_id: &str,
        sender: &str,
    ) -> ReactionEvent {
        serde_json::from_value(Value::Object(Map::from_iter([
            ("type".to_owned(), Value::String("m.reaction".into())),
            ("content".to_owned(), serde_json::to_value(content).unwrap()),
            ("event_id".to_owned(), serde_json::to_value(event_id).unwrap()),
            ("sender".to_owned(), Value::String(sender.into())),
            (
                "origin_server_ts".to_owned(),
                serde_json::to_value(MilliSecondsSinceUnixEpoch::now()).unwrap(),
            ),
            ("room_id".to_owned(), Value::String("!foo:example.org".into())),
        ])))
        .unwrap()
    }

    #[test]
    fn multiple_identical_reactions() {
        let mut info = RoomInfo::default();

        let content = ReactionEventContent::new(Annotation::new(
            owned_event_id!("$my_reaction"),
            "🏠".to_owned(),
        ));

        for i in 0..3 {
            let event_id = format!("$house_{i}");
            let react = create_reaction_event(&content, &event_id, "@foo:example.com");
            info.insert_reaction(react);
        }

        let content = ReactionEventContent::new(Annotation::new(
            owned_event_id!("$my_reaction"),
            "🙂".to_owned(),
        ));

        for i in 0..2 {
            let event_id = format!("$smile_{i}");
            let react = create_reaction_event(&content, &event_id, "@foo:example.com");
            info.insert_reaction(react);
        }

        for i in 2..4 {
            let event_id = format!("$smile2_{i}");
            let react = create_reaction_event(&content, &event_id, "@bar:example.com");
            info.insert_reaction(react);
        }

        assert_eq!(info.get_reactions(&owned_event_id!("$my_reaction")), vec![
            ("🏠", 1),
            ("🙂", 2)
        ]);
    }

    #[test]
    fn test_typing_spans() {
        let mut info = RoomInfo::default();
        let settings = mock_settings();

        let users0 = vec![];
        let users1 = vec![TEST_USER1.clone()];
        let users2 = vec![TEST_USER1.clone(), TEST_USER2.clone()];
        let users4 = vec![
            TEST_USER1.clone(),
            TEST_USER2.clone(),
            TEST_USER3.clone(),
            TEST_USER4.clone(),
        ];
        let users5 = vec![
            TEST_USER1.clone(),
            TEST_USER2.clone(),
            TEST_USER3.clone(),
            TEST_USER4.clone(),
            TEST_USER5.clone(),
        ];

        // Nothing set.
        assert_eq!(info.users_typing, None);
        assert_eq!(info.get_typing_spans(&settings), Line::from(vec![]));

        // Empty typing list.
        info.set_typing(users0);
        assert!(info.users_typing.is_some());
        assert_eq!(info.get_typing_spans(&settings), Line::from(vec![]));

        // Single user typing.
        info.set_typing(users1);
        assert!(info.users_typing.is_some());
        assert_eq!(
            info.get_typing_spans(&settings),
            Line::from(vec![
                Span::styled("@user1:example.com", user_style("@user1:example.com")),
                Span::from(" is typing...")
            ])
        );

        // Two users typing.
        info.set_typing(users2);
        assert!(info.users_typing.is_some());
        assert_eq!(
            info.get_typing_spans(&settings),
            Line::from(vec![
                Span::styled("@user1:example.com", user_style("@user1:example.com")),
                Span::raw(" and "),
                Span::styled("@user2:example.com", user_style("@user2:example.com")),
                Span::raw(" are typing...")
            ])
        );

        // Four users typing.
        info.set_typing(users4);
        assert!(info.users_typing.is_some());
        assert_eq!(info.get_typing_spans(&settings), Line::from("Several people are typing..."));

        // Five users typing.
        info.set_typing(users5);
        assert!(info.users_typing.is_some());
        assert_eq!(info.get_typing_spans(&settings), Line::from("Many people are typing..."));

        // Test that USER5 gets rendered using the configured color and name.
        info.set_typing(vec![TEST_USER5.clone()]);
        assert!(info.users_typing.is_some());
        assert_eq!(
            info.get_typing_spans(&settings),
            Line::from(vec![
                Span::styled("USER 5", user_style_from_color(Color::Black)),
                Span::from(" is typing...")
            ])
        );
    }

    #[test]
    fn test_need_load() {
        let room_id = TEST_ROOM1_ID.clone();

        let mut need_load = RoomNeeds::default();

        need_load.need_messages(room_id.clone());
        need_load.need_members(room_id.clone());

        assert_eq!(need_load.into_iter().collect::<Vec<(OwnedRoomId, Need)>>(), vec![(
            room_id,
            Need { members: true, messages: Some(Vec::new()) }
        )],);
    }

    #[tokio::test]
    async fn test_complete_msgbar() {
        let store = mock_store().await;
        let store = store.application;

        let room_id = TEST_ROOM1_ID.as_ref();

        let text = EditRope::from("going for a walk :walk ");
        let mut cursor = Cursor::new(0, 22);
        let res = complete_msgbar(&text, &mut cursor, room_id, &store);
        // Fuzzy matching finds looser matches too -- "walk" is a subsequence of "woman health
        // worker" -- but they rank below the shortcodes that actually start with what was typed.
        assert_eq!(res[..3], [":walking:", ":walking_man:", ":walking_woman:"]);
        assert_eq!(cursor, Cursor::new(0, 17));

        // A ":" in the middle of a word belongs to that word, so a time is not a shortcode.
        let text = EditRope::from("see you at 10:30 ");
        let mut cursor = Cursor::new(0, 16);
        let res = complete_msgbar(&text, &mut cursor, room_id, &store);
        assert_eq!(res, Vec::<String>::new());

        // Nor is the ":" in a URL.
        let text = EditRope::from("read https://iamb.chat ");
        let mut cursor = Cursor::new(0, 22);
        let res = complete_msgbar(&text, &mut cursor, room_id, &store);
        assert_eq!(res, Vec::<String>::new());

        // An "@" in the middle of a word belongs to that word, so an email address is not a
        // mention and does not go looking for room members.
        let text = EditRope::from("mail me at daniel@lyte.dev ");
        let mut cursor = Cursor::new(0, 25);
        let res = complete_msgbar(&text, &mut cursor, room_id, &store);
        assert_eq!(res, Vec::<String>::new());

        // Completing a real "@" is mention completion, which needs the worker thread to hand over
        // the room's members. See [crate::message::mention] for its tests.

        let text = EditRope::from("see #room ");
        let mut cursor = Cursor::new(0, 9);
        let res = complete_msgbar(&text, &mut cursor, room_id, &store);
        assert_eq!(res, vec!["#room1:example.com"]);
        assert_eq!(cursor, Cursor::new(0, 4));
    }

    #[tokio::test]
    async fn test_complete_cmdbar() {
        let store = mock_store().await;
        let store = store.application;
        let users = vec![
            "@user1:example.com",
            "@user2:example.com",
            "@user3:example.com",
            "@user4:example.com",
            "@user5:example.com",
        ];

        let text = EditRope::from("invite    ");
        let mut cursor = Cursor::new(0, 7);
        let id = text
            .get_prefix_word_mut(&mut cursor, &MATRIX_ID_WORD)
            .unwrap_or_else(EditRope::empty);
        assert_eq!(id.to_string(), "");
        assert_eq!(cursor, Cursor::new(0, 7));

        let text = EditRope::from("invite    ");
        let mut cursor = Cursor::new(0, 7);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, users);

        let text = EditRope::from("invite ignored");
        let mut cursor = Cursor::new(0, 7);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, users);

        let text = EditRope::from("invite @user1ignored");
        let mut cursor = Cursor::new(0, 13);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, vec!["@user1:example.com"]);

        let text = EditRope::from("abo hor");
        let mut cursor = Cursor::new(0, 7);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, vec!["horizontal"]);

        let text = EditRope::from("abo hor inv");
        let mut cursor = Cursor::new(0, 11);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, vec!["invite"]);

        let text = EditRope::from("abo hor invite \n");
        let mut cursor = Cursor::new(0, 15);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res, users);

        // A reaction can be named with or without the shortcode sigil, and either way it is the
        // bare name that gets inserted, since that is what the command wants.
        let text = EditRope::from("react polrbear");
        let mut cursor = Cursor::new(0, 14);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res.first().map(String::as_str), Some("polar_bear"));
        assert_eq!(cursor, Cursor::new(0, 6));

        let text = EditRope::from("react :polrbear");
        let mut cursor = Cursor::new(0, 15);
        let res = complete_cmdbar(&text, &mut cursor, &store);
        assert_eq!(res.first().map(String::as_str), Some("polar_bear"));
        assert_eq!(cursor, Cursor::new(0, 6));
    }

    #[test]
    fn test_ambiguous_displaynames() {
        let mut store = DisplayNameStore::default();

        store.set(TEST_USER1.clone(), Some("John".into()));
        store.set(TEST_USER2.clone(), Some("John".into()));
        store.set(TEST_USER3.clone(), Some("Jane".into()));
        store.set(TEST_USER4.clone(), Some("Alice".into()));
        store.set(TEST_USER5.clone(), Some("Bob".into()));

        // TEST_USER1 and TEST_USER2 are both ambiguous, while the other are unambiguous:
        assert_eq!(store.get(&TEST_USER1).unwrap().as_ref(), "John (@user1:example.com)");
        assert_eq!(store.get(&TEST_USER2).unwrap().as_ref(), "John (@user2:example.com)");
        assert_eq!(store.get(&TEST_USER3).unwrap().as_ref(), "Jane");
        assert_eq!(store.get(&TEST_USER4).unwrap().as_ref(), "Alice");
        assert_eq!(store.get(&TEST_USER5).unwrap().as_ref(), "Bob");

        // TEST_USER1 becomes unambiguous when TEST_USER2 changes:
        store.set(TEST_USER2.clone(), Some("Eve".into()));
        assert_eq!(store.get(&TEST_USER1).unwrap().as_ref(), "John");
        assert_eq!(store.get(&TEST_USER2).unwrap().as_ref(), "Eve");
        assert_eq!(store.get(&TEST_USER3).unwrap().as_ref(), "Jane");
        assert_eq!(store.get(&TEST_USER4).unwrap().as_ref(), "Alice");
        assert_eq!(store.get(&TEST_USER5).unwrap().as_ref(), "Bob");

        // TEST_USER5 becomes ambiguous when TEST_USER2 once again changes their name to match:
        store.set(TEST_USER2.clone(), Some("Bob".into()));
        assert_eq!(store.get(&TEST_USER1).unwrap().as_ref(), "John");
        assert_eq!(store.get(&TEST_USER2).unwrap().as_ref(), "Bob (@user2:example.com)");
        assert_eq!(store.get(&TEST_USER3).unwrap().as_ref(), "Jane");
        assert_eq!(store.get(&TEST_USER4).unwrap().as_ref(), "Alice");
        assert_eq!(store.get(&TEST_USER5).unwrap().as_ref(), "Bob (@user5:example.com)");

        // Now "Everyone is John":
        store.set(TEST_USER2.clone(), Some("John".into()));
        store.set(TEST_USER3.clone(), Some("John".into()));
        store.set(TEST_USER4.clone(), Some("John".into()));
        store.set(TEST_USER5.clone(), Some("John".into()));
        assert_eq!(store.get(&TEST_USER1).unwrap().as_ref(), "John (@user1:example.com)");
        assert_eq!(store.get(&TEST_USER2).unwrap().as_ref(), "John (@user2:example.com)");
        assert_eq!(store.get(&TEST_USER3).unwrap().as_ref(), "John (@user3:example.com)");
        assert_eq!(store.get(&TEST_USER4).unwrap().as_ref(), "John (@user4:example.com)");
        assert_eq!(store.get(&TEST_USER5).unwrap().as_ref(), "John (@user5:example.com)");

        // 2-5 unset their displayname:
        store.set(TEST_USER2.clone(), None);
        store.set(TEST_USER3.clone(), None);
        store.set(TEST_USER4.clone(), None);
        store.set(TEST_USER5.clone(), None);
        assert_eq!(store.get(&TEST_USER1).unwrap().as_ref(), "John");
        assert_eq!(store.get(&TEST_USER2), None);
        assert_eq!(store.get(&TEST_USER3), None);
        assert_eq!(store.get(&TEST_USER4), None);
        assert_eq!(store.get(&TEST_USER5), None);
    }
}
