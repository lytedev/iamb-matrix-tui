//! # Logic for loading and validating application configuration
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fmt;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, ExitStatus, Stdio};

use clap::Parser;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::{OwnedDeviceId, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, UserId};
use matrix_sdk::EncryptionState;
use ratatui::style::{Color, Modifier as StyleModifier, Style};
use ratatui::text::Span;
use ratatui_image::picker::ProtocolType;
use serde::{de::Error as SerdeError, de::Visitor, Deserialize, Deserializer, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use url::Url;

use modalkit::{env::vim::VimMode, key::TerminalKey, keybindings::InputKey};

use super::base::{
    IambError,
    IambId,
    RoomInfo,
    SortColumn,
    SortFieldRoom,
    SortFieldUser,
    SortOrder,
};

type Macros = HashMap<VimModes, HashMap<Keys, Keys>>;

macro_rules! usage {
    ( $($args: tt)* ) => {
        println!($($args)*);
        process::exit(2);
    }
}

const DEFAULT_MEMBERS_SORT: [SortColumn<SortFieldUser>; 2] = [
    SortColumn(SortFieldUser::PowerLevel, SortOrder::Ascending),
    SortColumn(SortFieldUser::UserId, SortOrder::Ascending),
];

const DEFAULT_ROOM_SORT: [SortColumn<SortFieldRoom>; 5] = [
    SortColumn(SortFieldRoom::Favorite, SortOrder::Ascending),
    SortColumn(SortFieldRoom::Invite, SortOrder::Ascending),
    SortColumn(SortFieldRoom::LowPriority, SortOrder::Ascending),
    SortColumn(SortFieldRoom::Unread, SortOrder::Ascending),
    SortColumn(SortFieldRoom::Name, SortOrder::Ascending),
];

const DEFAULT_ENABLE_TITLE: bool = true;
const DEFAULT_ENC_INDICATOR_LOC: EncryptionIndicatorLocation = EncryptionIndicatorLocation::PROMPT;
const DEFAULT_REQ_TIMEOUT: u64 = 120;

/// Rendered in the read receipt gutter when a user has no usable name.
const EMPTY_USER_CHAR: &str = " ";

const COLORS: [Color; 13] = [
    Color::Blue,
    Color::Cyan,
    Color::Green,
    Color::LightBlue,
    Color::LightGreen,
    Color::LightCyan,
    Color::LightMagenta,
    Color::LightRed,
    Color::LightYellow,
    Color::Magenta,
    Color::Red,
    Color::Reset,
    Color::Yellow,
];

pub fn user_color(user: &str) -> Color {
    let mut hasher = DefaultHasher::new();
    user.hash(&mut hasher);
    let color = hasher.finish() as usize % COLORS.len();

    COLORS[color]
}

pub fn user_style_from_color(color: Color) -> Style {
    Style::default().fg(color).add_modifier(StyleModifier::BOLD)
}

fn is_profile_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-'
}

fn default_true() -> bool {
    true
}

fn validate_profile_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut chars = name.chars();

    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return false;
    }

    name.chars().all(is_profile_char)
}

fn validate_profile_names(names: &BTreeMap<String, ProfileConfig>) {
    for name in names.keys() {
        if validate_profile_name(name.as_str()) {
            continue;
        }

        usage!(
            "{:?} is not a valid profile name.\n\n\
            Profile names can only contain the characters \
            a-z, A-Z, and 0-9. Period (.) and hyphen (-) are allowed after the first character.",
            name
        );
    }
}

const VERSION: &str = match option_env!("VERGEN_GIT_SHA") {
    None => env!("CARGO_PKG_VERSION"),
    Some(_) => concat!(env!("CARGO_PKG_VERSION"), " (", env!("VERGEN_GIT_SHA"), ")"),
};

#[derive(Parser)]
#[clap(version = VERSION, about, long_about = None)]
#[clap(propagate_version = true)]
pub struct Iamb {
    #[clap(long, value_parser)]
    pub completions: Option<clap_complete::Shell>,

    #[clap(short = 'P', long, value_parser)]
    pub profile: Option<String>,

    #[clap(short = 'C', long, value_parser)]
    pub config_directory: Option<PathBuf>,
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("Error reading configuration file: {0}")]
    IO(#[from] std::io::Error),

    #[error("Error loading configuration file: {0}")]
    Invalid(#[from] toml::de::Error),

    #[error("Error loading JSON configuration file: {0}")]
    InvalidJSON(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Keys(pub Vec<TerminalKey>, pub String);
pub struct KeysVisitor;

impl Visitor<'_> for KeysVisitor {
    type Value = Keys;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid Vim mode (e.g. \"normal\" or \"insert\")")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        match TerminalKey::from_macro_str(value) {
            Ok(keys) => Ok(Keys(keys, value.to_string())),
            Err(e) => Err(E::custom(format!("Could not parse key sequence: {e}"))),
        }
    }
}

impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(KeysVisitor)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VimModes(pub Vec<VimMode>);
pub struct VimModesVisitor;

impl Visitor<'_> for VimModesVisitor {
    type Value = VimModes;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid Vim mode (e.g. \"normal\" or \"insert\")")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        let mut modes = vec![];

        for mode in value.split('|') {
            let mode = match mode.to_ascii_lowercase().as_str() {
                "insert" | "i" => VimMode::Insert,
                "normal" | "n" => VimMode::Normal,
                "visual" | "v" => VimMode::Visual,
                "command" | "c" => VimMode::Command,
                "select" => VimMode::Select,
                "operator-pending" => VimMode::OperationPending,
                _ => return Err(E::custom("Could not parse into a Vim mode")),
            };

            modes.push(mode);
        }

        Ok(VimModes(modes))
    }
}

impl<'de> Deserialize<'de> for VimModes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(VimModesVisitor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserColor(pub Color);
pub struct UserColorVisitor;

impl Visitor<'_> for UserColorVisitor {
    type Value = UserColor;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid color")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        match value {
            "none" => Ok(UserColor(Color::Reset)),
            "red" => Ok(UserColor(Color::Red)),
            "black" => Ok(UserColor(Color::Black)),
            "green" => Ok(UserColor(Color::Green)),
            "yellow" => Ok(UserColor(Color::Yellow)),
            "blue" => Ok(UserColor(Color::Blue)),
            "magenta" => Ok(UserColor(Color::Magenta)),
            "cyan" => Ok(UserColor(Color::Cyan)),
            "gray" => Ok(UserColor(Color::Gray)),
            "dark-gray" => Ok(UserColor(Color::DarkGray)),
            "light-red" => Ok(UserColor(Color::LightRed)),
            "light-green" => Ok(UserColor(Color::LightGreen)),
            "light-yellow" => Ok(UserColor(Color::LightYellow)),
            "light-blue" => Ok(UserColor(Color::LightBlue)),
            "light-magenta" => Ok(UserColor(Color::LightMagenta)),
            "light-cyan" => Ok(UserColor(Color::LightCyan)),
            "white" => Ok(UserColor(Color::White)),
            _ => Err(E::custom("Could not parse color")),
        }
    }
}

impl<'de> Deserialize<'de> for UserColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(UserColorVisitor)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Session {
    access_token: String,
    refresh_token: Option<String>,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
}

impl From<Session> for MatrixSession {
    fn from(session: Session) -> Self {
        MatrixSession {
            tokens: matrix_sdk::authentication::SessionTokens {
                access_token: session.access_token,
                refresh_token: session.refresh_token,
            },
            meta: matrix_sdk::SessionMeta {
                user_id: session.user_id,
                device_id: session.device_id,
            },
        }
    }
}

impl From<MatrixSession> for Session {
    fn from(session: MatrixSession) -> Self {
        Session {
            access_token: session.tokens.access_token,
            refresh_token: session.tokens.refresh_token,
            user_id: session.meta.user_id,
            device_id: session.meta.device_id,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct UserDisplayTunables {
    pub color: Option<UserColor>,
    pub name: Option<String>,
}

pub type UserOverrides = HashMap<OwnedUserId, UserDisplayTunables>;

fn merge_maps<K, V>(
    profile: Option<HashMap<K, V>>,
    global: Option<HashMap<K, V>>,
) -> Option<HashMap<K, V>>
where
    K: Eq + Hash,
{
    match (global, profile) {
        (Some(m), None) | (None, Some(m)) => Some(m),
        (Some(mut global), Some(profile)) => {
            for (k, v) in profile {
                global.insert(k, v);
            }

            Some(global)
        },
        (None, None) => None,
    }
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum EncryptionIndicator {
    /// Always indicate the room's encryption status.
    #[default]
    Enabled,
    /// Never indicate the room's encryption status.
    Disabled,
    /// Only indicate the room's encryption status when it is encrypted.
    OnlyEncrypted,
    /// Only indicate the room's encryption status when it is unencrypted.
    OnlyUnencrypted,
}

bitflags::bitflags! {
    /// Available options for where to show the encryption status indicator.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct EncryptionIndicatorLocation: u8 {
        const NONE   = 0b00000000;
        const TITLE  = 0b00000001;
        const PROMPT = 0b00000010;
    }
}

pub struct EncryptionIndicatorLocationVisitor;

impl Visitor<'_> for EncryptionIndicatorLocationVisitor {
    type Value = EncryptionIndicatorLocation;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid encryption indicator location (e.g. \"title\" or \"prompt\")")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        let mut location = EncryptionIndicatorLocation::NONE;

        for value in value.split('|') {
            match value.to_ascii_lowercase().as_str() {
                "title" => location |= EncryptionIndicatorLocation::TITLE,
                "prompt" => location |= EncryptionIndicatorLocation::PROMPT,
                _ => {
                    return Err(E::custom("could not parse into an encryption indicator location"))
                },
            };
        }

        Ok(location)
    }
}

impl<'de> Deserialize<'de> for EncryptionIndicatorLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(EncryptionIndicatorLocationVisitor)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UserDisplayStyle {
    // The Matrix username for the sender (e.g., "@user:example.com").
    #[default]
    Username,

    // The localpart of the Matrix username (e.g., "@user").
    LocalPart,

    // The display name for the Matrix user, calculated according to the rules from the spec.
    //
    // This is usually something like "Ada Lovelace" if the user has configured a display name, but
    // it can wind up being the Matrix username if there are display name collisions in the room,
    // in order to avoid any confusion.
    DisplayName,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotifyVia {
    /// Deliver notifications via terminal bell.
    pub bell: bool,
    /// Deliver notifications via desktop mechanism.
    #[cfg(feature = "desktop")]
    pub desktop: bool,
}
pub struct NotifyViaVisitor;

impl Default for NotifyVia {
    fn default() -> Self {
        Self {
            bell: cfg!(not(feature = "desktop")),
            #[cfg(feature = "desktop")]
            desktop: true,
        }
    }
}

impl Visitor<'_> for NotifyViaVisitor {
    type Value = NotifyVia;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a valid notify destination (e.g. \"bell\" or \"desktop\")")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: SerdeError,
    {
        let mut via = NotifyVia {
            bell: false,
            #[cfg(feature = "desktop")]
            desktop: false,
        };

        for value in value.split('|') {
            match value.to_ascii_lowercase().as_str() {
                "bell" => {
                    via.bell = true;
                },
                #[cfg(feature = "desktop")]
                "desktop" => {
                    via.desktop = true;
                },
                #[cfg(not(feature = "desktop"))]
                "desktop" => {
                    return Err(E::custom("desktop notification support was compiled out"))
                },
                _ => return Err(E::custom("could not parse into a notify destination")),
            };
        }

        Ok(via)
    }
}

impl<'de> Deserialize<'de> for NotifyVia {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(NotifyViaVisitor)
    }
}

/// How the local message index is stored, and whether it exists at all.
///
/// A homeserver cannot index a room it cannot read, so `:search` finds nothing in an encrypted
/// room. An index that runs on this machine, over the text after it is decrypted, is the only
/// answer to that. It is also a decision the user must make, because it gives up a property that
/// encryption otherwise provides: a client that shows a message and forgets it leaves nothing
/// behind, and a client that indexes it leaves the words on disk for as long as the index lives.
///
/// For that reason this is off unless the user turns it on, and turning it on turns on the
/// matrix-sdk event cache, which is the thing that feeds the index. The event cache writes the
/// full decrypted JSON of an event to the sqlite store, for every room, not only the rooms that
/// are searched. That is a larger disclosure than the index itself.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LocalIndex {
    enabled: Option<bool>,
    passphrase: Option<String>,
    passphrase_command: Option<String>,
}

impl LocalIndex {
    fn merge(profile: Self, global: Self) -> Self {
        LocalIndex {
            enabled: profile.enabled.or(global.enabled),
            passphrase: profile.passphrase.or(global.passphrase),
            passphrase_command: profile.passphrase_command.or(global.passphrase_command),
        }
    }

    pub fn values(self) -> Result<LocalIndexValues, PassphraseError> {
        Ok(LocalIndexValues {
            enabled: self.enabled.unwrap_or(DEFAULT_LOCAL_INDEX_ENABLED),
            passphrase: passphrase(self.passphrase, self.passphrase_command)?,
        })
    }
}

/// The local message index is off until the user asks for it.
///
/// See [LocalIndex] for what turning it on writes to disk.
const DEFAULT_LOCAL_INDEX_ENABLED: bool = false;

/// Why the index passphrase could not be established.
///
/// Every one of these stops iamb at startup. An index that quietly fell back to no passphrase, or
/// to an empty one, would be weaker than the plain `passphrase` option it was meant to improve on,
/// and the user would have no way to notice.
#[derive(Debug, thiserror::Error)]
pub enum PassphraseError {
    #[error(
        "settings.local_index: set passphrase or passphrase_command, not both. \
         Remove whichever one you do not want."
    )]
    BothSources,

    #[error("settings.local_index.passphrase_command failed to start: {command:?}: {source}")]
    CommandFailed { command: String, source: std::io::Error },

    #[error("settings.local_index.passphrase_command exited with {status}: {command:?}{stderr}")]
    CommandStatus { command: String, status: String, stderr: String },

    #[error("settings.local_index.passphrase_command printed nothing: {command:?}")]
    CommandEmpty { command: String },
}

/// The passphrase the index is encrypted with, from whichever option supplied it.
///
/// The two options do the same job by different means, so setting both says nothing about which
/// one is meant. Refuse rather than choose: a silent preference would encrypt the index with the
/// weaker of the two and look like it had obeyed.
///
/// The command's first line of standard output is the passphrase. Only the line ending is
/// removed, because a passphrase can legitimately hold spaces at either end and a tool that
/// trimmed them would produce a passphrase that opens nothing.
fn passphrase(
    literal: Option<String>,
    command: Option<String>,
) -> Result<Option<String>, PassphraseError> {
    match (literal, command) {
        (Some(_), Some(_)) => Err(PassphraseError::BothSources),
        (Some(literal), None) => Ok(Some(literal)),
        (None, None) => Ok(None),
        (None, Some(command)) => run_passphrase_command(command).map(Some),
    }
}

/// Run `command` through the shell and take the first line it prints.
///
/// The shell runs it so that the option holds a command line rather than a list of words, which is
/// what the user writes in every other tool that has this option.
fn run_passphrase_command(command: String) -> Result<String, PassphraseError> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| PassphraseError::CommandFailed { command: command.clone(), source })?;

    if !output.status.success() {
        return Err(PassphraseError::CommandStatus {
            command,
            status: describe_status(output.status),
            stderr: first_stderr_line(&output.stderr),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    match stdout
        .split('\n')
        .next()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
    {
        Some(line) if !line.is_empty() => Ok(line.to_string()),
        _ => Err(PassphraseError::CommandEmpty { command }),
    }
}

/// How the command ended, in words a user can act on.
///
/// The Display of an [ExitStatus] reads "exit status: 3", which does not fit in a sentence that
/// already says the command exited.
fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => "a signal".to_string(),
    }
}

/// The command's own first line of error output, ready to append to a message.
///
/// This is what tells a locked vault apart from a misspelled entry name, and the user cannot see
/// it otherwise: iamb captures the command's output rather than letting it reach the terminal.
fn first_stderr_line(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let line = stderr.lines().find(|line| !line.trim().is_empty());

    match line {
        Some(line) => format!(": {}", line.trim()),
        None => String::new(),
    }
}

#[derive(Clone, Debug)]
pub struct LocalIndexValues {
    pub enabled: bool,

    /// The passphrase the index directory is encrypted with.
    ///
    /// Without one the index is written as plain tantivy files, and the words of every indexed
    /// message can be read out of them with no key at all.
    ///
    /// Two configuration options supply this, and they are mutually exclusive.
    ///
    /// `passphrase` holds the passphrase itself. It is the simplest thing that works, and it
    /// protects a copy of the index that leaves this machine: a backup, a misconfigured sync, or
    /// a disk read later. It protects against nothing that can read the configuration file, which
    /// is anything running as this user.
    ///
    /// `passphrase_command` runs a command and takes its first line of output. This keeps the
    /// passphrase out of the configuration file, and one option covers every source: a file
    /// through `cat`, a password manager through `rbw get`, a keychain through `secret-tool
    /// lookup`. A separate file option would add nothing that `cat` does not already do, and one
    /// option is easier to explain than three. It is also why iamb links no keychain library:
    /// `secret-tool` reaches the keychain without a new dependency, and without iamb having to
    /// know which keychain this machine runs.
    ///
    /// The command runs once, at startup, and iamb waits for it. A command that needs an unlocked
    /// vault, such as `rbw`, therefore fails whenever the vault is locked, which on a machine that
    /// starts iamb unattended is most of the time. Reading a decrypted secrets file does not have
    /// that problem, and is the better choice here.
    ///
    /// Neither option protects against anything that can read the crypto store, which sits beside
    /// the index, is not encrypted either, and holds the room keys that decrypt the whole history.
    pub passphrase: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Encryption {
    indicator: Option<EncryptionIndicator>,
    indicator_location: Option<EncryptionIndicatorLocation>,
}

impl Encryption {
    fn merge(profile: Self, global: Self) -> Self {
        Encryption {
            indicator: profile.indicator.or(global.indicator),
            indicator_location: profile.indicator_location.or(global.indicator_location),
        }
    }

    pub fn values(self) -> EncryptionValues {
        EncryptionValues {
            indicator: self.indicator.unwrap_or_default(),
            indicator_location: self.indicator_location.unwrap_or(DEFAULT_ENC_INDICATOR_LOC),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncryptionValues {
    pub indicator: EncryptionIndicator,
    pub indicator_location: EncryptionIndicatorLocation,
}

impl EncryptionValues {
    pub fn get_indicator(
        &self,
        location: EncryptionIndicatorLocation,
        state: EncryptionState,
    ) -> Option<Span<'static>> {
        if !self.indicator_location.contains(location) {
            return None;
        }

        let indicator = match (self.indicator, state) {
            (EncryptionIndicator::Disabled, _) |
            (EncryptionIndicator::OnlyUnencrypted, EncryptionState::Encrypted) |
            (EncryptionIndicator::OnlyEncrypted, EncryptionState::NotEncrypted) => {
                // User doesn't want to see anything:
                return None;
            },
            (
                EncryptionIndicator::Enabled | EncryptionIndicator::OnlyEncrypted,
                EncryptionState::Encrypted,
            ) => {
                // Green lock:
                Span::styled("\u{1F512}\u{FE0E} ", Style::new().fg(Color::LightGreen))
            },
            (
                EncryptionIndicator::Enabled | EncryptionIndicator::OnlyUnencrypted,
                EncryptionState::NotEncrypted,
            ) => {
                // Red unlocked lock:
                Span::styled("\u{1F513}\u{FE0E} ", Style::new().fg(Color::Red))
            },

            (_, EncryptionState::Unknown) => {
                // Yellow question mark:
                Span::styled("? ", Style::new().fg(Color::Yellow))
            },
        };

        Some(indicator)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct Mouse {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct Notifications {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub via: NotifyVia,
    #[serde(default = "default_true")]
    pub show_message: bool,
    #[serde(default)]
    pub sound_hint: Option<String>,

    /// Name to register with the external `focus-tui` helper, enabling clickable notifications.
    ///
    /// Opting in means iamb runs `focus-tui-register <name>` at startup, and runs
    /// `focus-tui <name>` when a notification is clicked. Leaving this unset means iamb never
    /// shells out, and clicking a notification only jumps within an already-visible iamb.
    #[serde(default)]
    pub focus_tui: Option<String>,
}

#[derive(Clone)]
pub struct ImagePreviewValues {
    pub lazy_load: bool,
    pub size: ImagePreviewSize,
    pub protocol: Option<ImagePreviewProtocolValues>,
}

#[derive(Clone, Default, Deserialize)]
pub struct ImagePreview {
    pub lazy_load: Option<bool>,
    pub size: Option<ImagePreviewSize>,
    pub protocol: Option<ImagePreviewProtocolValues>,
}

impl ImagePreview {
    fn values(self) -> ImagePreviewValues {
        ImagePreviewValues {
            lazy_load: self.lazy_load.unwrap_or(true),
            size: self.size.unwrap_or_default(),
            protocol: self.protocol,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Debug)]
pub struct ImagePreviewSize {
    pub width: usize,
    pub height: usize,
}

impl Default for ImagePreviewSize {
    fn default() -> Self {
        ImagePreviewSize { width: 66, height: 10 }
    }
}

#[derive(Clone, Deserialize)]
pub struct ImagePreviewProtocolValues {
    pub r#type: Option<ProtocolType>,
    pub font_size: Option<(u16, u16)>,
}

#[derive(Clone)]
pub struct SortValues {
    pub chats: Vec<SortColumn<SortFieldRoom>>,
    pub dms: Vec<SortColumn<SortFieldRoom>>,
    pub rooms: Vec<SortColumn<SortFieldRoom>>,
    pub spaces: Vec<SortColumn<SortFieldRoom>>,
    pub members: Vec<SortColumn<SortFieldUser>>,
}

#[derive(Clone, Default, Deserialize)]
pub struct SortOverrides {
    pub chats: Option<Vec<SortColumn<SortFieldRoom>>>,
    pub dms: Option<Vec<SortColumn<SortFieldRoom>>>,
    pub rooms: Option<Vec<SortColumn<SortFieldRoom>>>,
    pub spaces: Option<Vec<SortColumn<SortFieldRoom>>>,
    pub members: Option<Vec<SortColumn<SortFieldUser>>>,
}

impl SortOverrides {
    fn merge(profile: Self, global: Self) -> Self {
        Self {
            chats: profile.chats.or(global.chats),
            dms: profile.dms.or(global.dms),
            rooms: profile.rooms.or(global.rooms),
            spaces: profile.spaces.or(global.spaces),
            members: profile.members.or(global.members),
        }
    }

    pub fn values(self) -> SortValues {
        let rooms = self.rooms.unwrap_or_else(|| Vec::from(DEFAULT_ROOM_SORT));
        let chats = self.chats.unwrap_or_else(|| rooms.clone());
        let dms = self.dms.unwrap_or_else(|| rooms.clone());
        let spaces = self.spaces.unwrap_or_else(|| rooms.clone());
        let members = self.members.unwrap_or_else(|| Vec::from(DEFAULT_MEMBERS_SORT));

        SortValues { rooms, members, chats, dms, spaces }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Terminal {
    pub cursor_shape: Option<CursorShape>,
    pub enable_extended_keys: Option<bool>,
    pub enable_title: Option<bool>,
}

impl Terminal {
    fn merge(profile: Self, global: Self) -> Self {
        Self {
            cursor_shape: profile.cursor_shape.or(global.cursor_shape),
            enable_extended_keys: profile.enable_extended_keys.or(global.enable_extended_keys),
            enable_title: profile.enable_title.or(global.enable_title),
        }
    }

    pub fn values(self) -> TerminalValues {
        TerminalValues {
            cursor_shape: self.cursor_shape.unwrap_or_default(),
            enable_extended_keys: self.enable_extended_keys,
            enable_title: self.enable_title.unwrap_or(DEFAULT_ENABLE_TITLE),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TerminalValues {
    pub cursor_shape: CursorShape,
    pub enable_extended_keys: Option<bool>,
    pub enable_title: bool,
}

#[derive(Clone)]
pub struct TunableValues {
    pub encryption: EncryptionValues,
    pub local_index: LocalIndexValues,
    pub log_level: String,
    pub max_log_files: usize,
    pub message_shortcode_display: bool,
    pub normal_after_send: bool,
    pub reaction_display: bool,
    pub reaction_shortcode_display: bool,
    pub read_receipt_send: bool,
    /// The duration `:snooze` uses when given no argument.
    pub snooze_default: String,
    /// The local hour that `:snooze tomorrow` resolves to.
    pub snooze_tomorrow_hour: u32,
    pub read_receipt_display: bool,
    pub read_receipt_manual: bool,
    pub request_timeout: u64,
    pub sort: SortValues,
    pub state_event_display: bool,
    pub typing_notice_send: bool,
    pub typing_notice_display: bool,
    pub users: UserOverrides,
    pub username_display: UserDisplayStyle,
    pub message_user_color: bool,
    pub default_room: Option<String>,
    pub open_command: Option<Vec<String>>,
    pub mouse: Mouse,
    pub notifications: Notifications,
    pub terminal: TerminalValues,
    pub image_preview: Option<ImagePreviewValues>,
    pub user_gutter_width: usize,
    /// The largest share of the pane, in percent, that the sender column may take.
    pub user_gutter_max_percent: usize,
    pub external_edit_file_suffix: String,
    pub tabstop: usize,
    pub default_split: SplitDirection,
    pub ssl_verify: bool,
}

#[derive(Clone, Default, Deserialize)]
pub struct Tunables {
    /// Subsection for overriding encryption-related settings.
    #[serde(default)]
    pub encryption: Encryption,

    /// Subsection for the local message index.
    #[serde(default)]
    pub local_index: LocalIndex,

    /// Subsection for overriding sort orders in UI lists.
    #[serde(default)]
    pub sort: SortOverrides,

    /// Subsection for overriding terminal settings.
    #[serde(default)]
    pub terminal: Terminal,

    /// Subsection for overriding how specific Matrix users are rendered.
    pub users: Option<UserOverrides>,

    pub log_level: Option<String>,
    pub max_log_files: Option<usize>,
    pub message_shortcode_display: Option<bool>,
    pub normal_after_send: Option<bool>,
    pub reaction_display: Option<bool>,
    pub reaction_shortcode_display: Option<bool>,
    pub read_receipt_send: Option<bool>,
    pub snooze_default: Option<String>,
    pub snooze_tomorrow_hour: Option<u32>,
    pub read_receipt_display: Option<bool>,
    pub read_receipt_manual: Option<bool>,
    pub request_timeout: Option<u64>,
    pub state_event_display: Option<bool>,
    pub typing_notice_send: Option<bool>,
    pub typing_notice_display: Option<bool>,
    pub username_display: Option<UserDisplayStyle>,
    pub message_user_color: Option<bool>,
    pub default_room: Option<String>,
    pub open_command: Option<Vec<String>>,
    pub mouse: Option<Mouse>,
    pub notifications: Option<Notifications>,
    pub image_preview: Option<ImagePreview>,
    pub user_gutter_width: Option<usize>,
    pub user_gutter_max_percent: Option<usize>,
    pub external_edit_file_suffix: Option<String>,
    pub tabstop: Option<usize>,
    pub default_split: Option<SplitDirection>,
    pub ssl_verify: Option<bool>,
}

impl Tunables {
    fn merge(self, other: Self) -> Self {
        Tunables {
            encryption: Encryption::merge(self.encryption, other.encryption),
            local_index: LocalIndex::merge(self.local_index, other.local_index),
            sort: SortOverrides::merge(self.sort, other.sort),
            terminal: Terminal::merge(self.terminal, other.terminal),
            users: merge_maps(self.users, other.users),

            log_level: self.log_level.or(other.log_level),
            max_log_files: self.max_log_files.or(other.max_log_files),
            message_shortcode_display: self
                .message_shortcode_display
                .or(other.message_shortcode_display),
            normal_after_send: self.normal_after_send.or(other.normal_after_send),
            reaction_display: self.reaction_display.or(other.reaction_display),
            reaction_shortcode_display: self
                .reaction_shortcode_display
                .or(other.reaction_shortcode_display),
            read_receipt_send: self.read_receipt_send.or(other.read_receipt_send),
            snooze_default: self.snooze_default.or(other.snooze_default),
            snooze_tomorrow_hour: self.snooze_tomorrow_hour.or(other.snooze_tomorrow_hour),
            read_receipt_display: self.read_receipt_display.or(other.read_receipt_display),
            read_receipt_manual: self.read_receipt_manual.or(other.read_receipt_manual),
            request_timeout: self.request_timeout.or(other.request_timeout),
            state_event_display: self.state_event_display.or(other.state_event_display),
            typing_notice_send: self.typing_notice_send.or(other.typing_notice_send),
            typing_notice_display: self.typing_notice_display.or(other.typing_notice_display),
            username_display: self.username_display.or(other.username_display),
            message_user_color: self.message_user_color.or(other.message_user_color),
            default_room: self.default_room.or(other.default_room),
            open_command: self.open_command.or(other.open_command),
            mouse: self.mouse.or(other.mouse),
            notifications: self.notifications.or(other.notifications),
            image_preview: self.image_preview.or(other.image_preview),
            user_gutter_width: self.user_gutter_width.or(other.user_gutter_width),
            user_gutter_max_percent: self
                .user_gutter_max_percent
                .or(other.user_gutter_max_percent),
            external_edit_file_suffix: self
                .external_edit_file_suffix
                .or(other.external_edit_file_suffix),
            tabstop: self.tabstop.or(other.tabstop),
            default_split: self.default_split.or(other.default_split),
            ssl_verify: self.ssl_verify.or(other.ssl_verify),
        }
    }

    fn values(self) -> Result<TunableValues, PassphraseError> {
        Ok(TunableValues {
            encryption: self.encryption.values(),
            local_index: self.local_index.values()?,
            sort: self.sort.values(),
            terminal: self.terminal.values(),

            log_level: self.log_level.unwrap_or_else(|| "warn".to_string()),
            max_log_files: self.max_log_files.unwrap_or(7),
            message_shortcode_display: self.message_shortcode_display.unwrap_or(false),
            normal_after_send: self.normal_after_send.unwrap_or(false),
            reaction_display: self.reaction_display.unwrap_or(true),
            reaction_shortcode_display: self.reaction_shortcode_display.unwrap_or(false),
            read_receipt_send: self.read_receipt_send.unwrap_or(true),
            // An hour is long enough to clear a distraction and short enough that a forgotten
            // snooze surfaces the same day.
            snooze_default: self.snooze_default.unwrap_or_else(|| "1h".into()),
            // The start of a working day, so that "tomorrow" means "when I next sit down".
            snooze_tomorrow_hour: self.snooze_tomorrow_hour.unwrap_or(9).min(23),
            read_receipt_display: self.read_receipt_display.unwrap_or(true),
            read_receipt_manual: self.read_receipt_manual.unwrap_or(false),
            request_timeout: self.request_timeout.unwrap_or(DEFAULT_REQ_TIMEOUT),
            state_event_display: self.state_event_display.unwrap_or(true),
            typing_notice_send: self.typing_notice_send.unwrap_or(true),
            typing_notice_display: self.typing_notice_display.unwrap_or(true),
            users: self.users.unwrap_or_default(),
            username_display: self.username_display.unwrap_or_default(),
            message_user_color: self.message_user_color.unwrap_or(false),
            default_room: self.default_room,
            open_command: self.open_command,
            mouse: self.mouse.unwrap_or_default(),
            notifications: self.notifications.unwrap_or_default(),
            image_preview: self.image_preview.map(ImagePreview::values),
            user_gutter_width: self.user_gutter_width.unwrap_or(30),
            // A quarter of the pane. A pane of 120 columns keeps the full 30 columns that the
            // sender column had before this cap existed, so a wide window looks the same as it
            // always did. A pane of 60 columns gives the sender 15 and the message the rest,
            // which is the case the cap exists for: a name is worth a glance, and a stack trace
            // squeezed into a few columns is worth nothing.
            user_gutter_max_percent: self.user_gutter_max_percent.unwrap_or(25).min(100),
            external_edit_file_suffix: self
                .external_edit_file_suffix
                .unwrap_or_else(|| ".md".to_string()),
            tabstop: self.tabstop.unwrap_or(4),
            default_split: self.default_split.unwrap_or_default(),
            ssl_verify: self.ssl_verify.unwrap_or(true),
        })
    }
}

#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum CursorShape {
    #[default]
    Default,
    Block,
    Line,
    Underline,
}

impl From<CursorShape> for modalkit::crossterm::cursor::SetCursorStyle {
    fn from(shape: CursorShape) -> Self {
        match shape {
            CursorShape::Default => Self::DefaultUserShape,
            CursorShape::Block => Self::SteadyBlock,
            CursorShape::Line => Self::SteadyBar,
            CursorShape::Underline => Self::SteadyUnderScore,
        }
    }
}

#[derive(Clone)]
pub struct DirectoryValues {
    pub cache: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
    pub downloads: Option<PathBuf>,
}

impl DirectoryValues {
    fn create_dir_all(&self) -> std::io::Result<()> {
        use std::fs::create_dir_all;

        let Self { cache, data, logs, downloads } = self;

        create_dir_all(cache)?;
        create_dir_all(data)?;
        create_dir_all(logs)?;

        if let Some(downloads) = downloads {
            create_dir_all(downloads)?;
        }

        Ok(())
    }
}

#[derive(Clone, Default, Deserialize)]
pub struct Directories {
    pub cache: Option<String>,
    pub data: Option<String>,
    pub logs: Option<String>,
    pub downloads: Option<String>,
}

impl Directories {
    fn merge(self, other: Self) -> Self {
        Directories {
            cache: self.cache.or(other.cache),
            data: self.data.or(other.data),
            logs: self.logs.or(other.logs),
            downloads: self.downloads.or(other.downloads),
        }
    }

    fn values(self) -> DirectoryValues {
        let cache = self
            .cache
            .map(|dir| {
                let dir = shellexpand::full(&dir)
                    .expect("unable to expand shell variables in dirs.cache");
                Path::new(dir.as_ref()).to_owned()
            })
            .or_else(|| {
                let mut dir = dirs::cache_dir()?;
                dir.push("iamb");
                dir.into()
            })
            .expect("no dirs.cache value configured!");

        let data = self
            .data
            .map(|dir| {
                let dir = shellexpand::full(&dir)
                    .expect("unable to expand shell variables in dirs.cache");
                Path::new(dir.as_ref()).to_owned()
            })
            .or_else(|| {
                let mut dir = dirs::data_dir()?;
                dir.push("iamb");
                dir.into()
            })
            .expect("no dirs.data value configured!");

        let logs = self
            .logs
            .map(|dir| {
                let dir = shellexpand::full(&dir)
                    .expect("unable to expand shell variables in dirs.cache");
                Path::new(dir.as_ref()).to_owned()
            })
            .unwrap_or_else(|| {
                let mut dir = cache.clone();
                dir.push("logs");
                dir
            });

        let downloads = self
            .downloads
            .map(|dir| {
                let dir = shellexpand::full(&dir)
                    .expect("unable to expand shell variables in dirs.cache");
                Path::new(dir.as_ref()).to_owned()
            })
            .or_else(dirs::download_dir);

        DirectoryValues { cache, data, logs, downloads }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum WindowPath {
    AliasId(OwnedRoomAliasId),
    RoomId(OwnedRoomId),
    UserId(OwnedUserId),
    Window(IambId),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum WindowLayout {
    Window { window: WindowPath },
    Split { split: Vec<WindowLayout> },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase", tag = "style")]
pub enum Layout {
    /// Restore the layout from the previous session.
    #[default]
    Restore,

    /// Open a single window using the `default_room` value.
    New,

    /// Open the window layouts described under `tabs`.
    Config { tabs: Vec<WindowLayout> },
}

#[derive(Clone, Deserialize)]
pub struct ProfileConfig {
    pub user_id: OwnedUserId,
    pub password_file: Option<PathBuf>,
    pub url: Option<Url>,
    pub settings: Option<Tunables>,
    pub dirs: Option<Directories>,
    pub layout: Option<Layout>,
    pub macros: Option<Macros>,
}

#[derive(Clone, Deserialize)]
pub struct IambConfig {
    pub profiles: BTreeMap<String, ProfileConfig>,
    pub default_profile: Option<String>,
    pub settings: Option<Tunables>,
    pub dirs: Option<Directories>,
    pub layout: Option<Layout>,
    pub macros: Option<Macros>,
}

impl IambConfig {
    pub fn load_toml(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        let config = toml::from_str(&s)?;

        Ok(config)
    }

    pub fn load_json(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        let config = serde_json::from_str(&s)?;

        Ok(config)
    }
}

#[derive(Clone)]
pub struct ApplicationSettings {
    pub layout_json: PathBuf,
    pub session_json: PathBuf,
    pub session_json_old: PathBuf,
    pub sled_dir: PathBuf,
    pub sqlite_dir: PathBuf,

    /// Where each room's local message index directory is written.
    pub search_index_dir: PathBuf,

    pub profile_name: String,
    pub profile: ProfileConfig,
    pub tunables: TunableValues,
    pub dirs: DirectoryValues,
    pub layout: Layout,
    pub macros: Macros,
}

impl ApplicationSettings {
    fn get_xdg_config_home() -> Option<PathBuf> {
        env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from)
    }

    pub fn load(cli: Iamb) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config_dir = cli
            .config_directory
            .or_else(Self::get_xdg_config_home)
            .or_else(dirs::config_dir)
            .unwrap_or_else(|| {
                usage!(
                    "No user configuration directory found;\
                    please specify one via -C.\n\n
                    For more information try '--help'"
                );
            });

        config_dir.push("iamb");
        let config_json = config_dir.join("config.json");
        let config_toml = config_dir.join("config.toml");

        let config = if config_toml.is_file() {
            IambConfig::load_toml(config_toml.as_path())?
        } else if config_json.is_file() {
            IambConfig::load_json(config_json.as_path())?
        } else {
            usage!(
                "Please create a configuration file at {}\n\n\
                For more information try '--help'",
                config_toml.display(),
            );
        };

        let IambConfig {
            mut profiles,
            default_profile,
            dirs,
            settings: global,
            layout,
            macros,
        } = config;

        validate_profile_names(&profiles);

        let (profile_name, mut profile) = if let Some(profile) = cli.profile.or(default_profile) {
            profiles.remove_entry(&profile).unwrap_or_else(|| {
                usage!(
                    "No configured profile with the name {:?} in {}",
                    profile,
                    config_json.display()
                );
            })
        } else if profiles.len() == 1 {
            profiles.into_iter().next().unwrap()
        } else {
            loop {
                println!("\nNo profile specified. Available profiles:");
                profiles.keys().enumerate().for_each(|(i, name)| println!("{i}: {name}"));

                print!("Select a number or 'q' to quit: ");
                let _ = std::io::stdout().flush();

                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);

                if input.trim() == "q" {
                    usage!(
                        "No profile specified. \
                        Please use -P or add \"default_profile\" to your configuration.\n\n\
                        For more information try '--help'",
                    );
                }
                if let Ok(i) = input.trim().parse::<usize>() {
                    if i < profiles.len() {
                        break profiles.into_iter().nth(i).unwrap();
                    }
                }
                println!("\nInvalid index.");
            }
        };

        let macros = merge_maps(profile.macros.take(), macros).unwrap_or_default();
        let layout = profile.layout.take().or(layout).unwrap_or_default();

        let tunables = global.unwrap_or_default();
        let tunables = profile.settings.take().unwrap_or_default().merge(tunables);
        let tunables = tunables.values()?;

        let dirs = dirs.unwrap_or_default();
        let dirs = profile.dirs.take().unwrap_or_default().merge(dirs);
        let dirs = dirs.values();

        // Create directories
        dirs.create_dir_all()?;

        // Set up paths that live inside the profile's data directory.
        let mut profile_dir = config_dir.clone();
        profile_dir.push("profiles");
        profile_dir.push(profile_name.as_str());

        let mut profile_data_dir = dirs.data.clone();
        profile_data_dir.push("profiles");
        profile_data_dir.push(profile_name.as_str());

        let mut sled_dir = profile_dir.clone();
        sled_dir.push("matrix");

        let mut sqlite_dir = profile_data_dir.clone();
        sqlite_dir.push("sqlite");

        // The index sits beside the sqlite stores, under the data directory rather than the cache
        // directory, so that clearing caches does not silently empty it. It is still disposable:
        // every event in it can be fetched again from the homeserver.
        let mut search_index_dir = profile_data_dir.clone();
        search_index_dir.push("search-index");

        let mut session_json = profile_data_dir.clone();
        session_json.push("session.json");

        let mut session_json_old = profile_dir;
        session_json_old.push("session.json");

        // Set up paths that live inside the profile's cache directory.
        let mut cache_dir = dirs.cache.clone();
        cache_dir.push("profiles");
        cache_dir.push(profile_name.as_str());

        let mut layout_json = cache_dir.clone();
        layout_json.push("layout.json");

        let settings = ApplicationSettings {
            sled_dir,
            layout_json,
            session_json,
            session_json_old,
            sqlite_dir,
            search_index_dir,
            profile_name,
            profile,
            tunables,
            dirs,
            layout,
            macros,
        };

        Ok(settings)
    }

    pub fn read_session(&self, path: impl AsRef<Path>) -> Result<Session, IambError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let session = serde_json::from_reader(reader).map_err(IambError::from)?;
        Ok(session)
    }

    pub fn write_session(&self, session: MatrixSession) -> Result<(), IambError> {
        let file = File::create(self.session_json.as_path())?;
        let writer = BufWriter::new(file);
        let session = Session::from(session);
        serde_json::to_writer(writer, &session).map_err(IambError::from)?;
        Ok(())
    }

    /// The single-grapheme label used for a user in the read receipt gutter.
    ///
    /// This prefers the same name that [ApplicationSettings::get_user_span] shows in the sender
    /// column: a configured override first, then the room's display name, then the localpart.
    pub fn get_user_char_span(&self, user_id: &UserId, info: &RoomInfo) -> Span<'static> {
        let (color, name) = self.get_user_overrides(user_id);

        let color = color.unwrap_or_else(|| user_color(user_id.as_str()));
        let style = user_style_from_color(color);

        let name = match name {
            Some(name) => name,
            None => info
                .display_names
                .get(user_id)
                .unwrap_or_else(|| Cow::Borrowed(user_id.localpart())),
        };

        let c = name.graphemes(true).next().unwrap_or(EMPTY_USER_CHAR);

        Span::styled(String::from(c), style)
    }

    pub fn get_user_overrides(
        &self,
        user_id: &UserId,
    ) -> (Option<Color>, Option<Cow<'static, str>>) {
        self.tunables
            .users
            .get(user_id)
            .map(|user| (user.color.as_ref().map(|c| c.0), user.name.clone().map(Cow::Owned)))
            .unwrap_or_default()
    }

    pub fn get_user_color(&self, user_id: &UserId) -> Color {
        self.tunables
            .users
            .get(user_id)
            .and_then(|user| user.color.as_ref().map(|c| c.0))
            .unwrap_or_else(|| user_color(user_id.as_str()))
    }

    pub fn get_user_style(&self, user_id: &UserId) -> Style {
        user_style_from_color(self.get_user_color(user_id))
    }

    /// The name to show for a user, without any style.
    pub fn get_user_name<'a>(&self, user_id: &'a UserId, info: &'a RoomInfo) -> Cow<'a, str> {
        let (_, name) = self.get_user_overrides(user_id);

        match (name, &self.tunables.username_display) {
            (Some(name), _) => name,
            (None, UserDisplayStyle::Username) => Cow::Borrowed(user_id.as_str()),
            (None, UserDisplayStyle::LocalPart) => Cow::Borrowed(user_id.localpart()),
            (None, UserDisplayStyle::DisplayName) => {
                if let Some(name) = info.display_names.get(user_id) {
                    name
                } else {
                    Cow::Borrowed(user_id.as_str())
                }
            },
        }
    }

    pub fn get_user_span<'a>(&self, user_id: &'a UserId, info: &'a RoomInfo) -> Span<'a> {
        let (color, _) = self.get_user_overrides(user_id);

        let color = color.unwrap_or_else(|| user_color(user_id.as_str()));
        let style = user_style_from_color(color);
        let name = self.get_user_name(user_id, info);

        Span::styled(name, style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;
    use std::convert::TryFrom;

    /// The local index settings out of a TOML fragment, as [ApplicationSettings::load] gets them.
    fn local_index(toml: &str) -> Result<LocalIndexValues, PassphraseError> {
        toml::from_str::<LocalIndex>(toml).unwrap().values()
    }

    #[test]
    fn test_a_passphrase_command_supplies_the_passphrase() {
        let values = local_index(r#"passphrase_command = "printf 'from the vault'""#).unwrap();

        assert_eq!(values.passphrase.as_deref(), Some("from the vault"));
    }

    #[test]
    fn test_a_passphrase_command_keeps_the_spaces_and_drops_the_line_ending() {
        // A passphrase can hold a space at either end, so only the line ending goes.
        let values =
            local_index(r#"passphrase_command = "printf ' padded \n second line'""#).unwrap();

        assert_eq!(values.passphrase.as_deref(), Some(" padded "));
    }

    #[test]
    fn test_two_passphrase_sources_are_refused() {
        // Preferring one silently would encrypt the index with a passphrase the user did not
        // choose, and look like it had obeyed.
        let both = local_index(
            r#"
            passphrase = "in the file"
            passphrase_command = "printf 'from the vault'"
            "#,
        );

        assert!(matches!(both, Err(PassphraseError::BothSources)));
    }

    #[test]
    fn test_a_failing_passphrase_command_is_refused() {
        let failed = local_index(r#"passphrase_command = "exit 3""#);

        assert!(matches!(failed, Err(PassphraseError::CommandStatus { .. })));
    }

    #[test]
    fn test_a_failing_passphrase_command_repeats_what_it_said() {
        // iamb captures the command's output, so this is the user's only sight of the reason.
        let locked = local_index(r#"passphrase_command = "echo 'vault is locked' >&2; exit 1""#);

        assert_eq!(
            locked.unwrap_err().to_string(),
            "settings.local_index.passphrase_command exited with status 1: \
             \"echo 'vault is locked' >&2; exit 1\": vault is locked"
        );
    }

    #[test]
    fn test_a_passphrase_command_that_prints_nothing_is_refused() {
        // An empty passphrase is a passphrase, and it protects nothing.
        let empty = local_index(r#"passphrase_command = "true""#);

        assert!(matches!(empty, Err(PassphraseError::CommandEmpty { .. })));
    }

    #[test]
    fn test_user_char_span_prefers_display_name() {
        use crate::tests::{mock_settings, TEST_USER1};

        let mut settings = mock_settings();
        let mut info = RoomInfo::default();

        // With no display name, we fall back to the localpart.
        assert_eq!(settings.get_user_char_span(&TEST_USER1, &info).content, "u");

        // The room display name wins over the localpart.
        info.display_names.set(TEST_USER1.clone(), Some("Ada Lovelace".into()));
        assert_eq!(settings.get_user_char_span(&TEST_USER1, &info).content, "A");

        // A multi-codepoint grapheme is not split apart.
        info.display_names.set(TEST_USER1.clone(), Some("\u{1f469}\u{200d}\u{1f4bb} Ada".into()));
        assert_eq!(
            settings.get_user_char_span(&TEST_USER1, &info).content,
            "\u{1f469}\u{200d}\u{1f4bb}"
        );

        // A configured name override still wins over the display name.
        settings.tunables.users.insert(TEST_USER1.clone(), UserDisplayTunables {
            name: Some("Zed".into()),
            color: None,
        });
        assert_eq!(settings.get_user_char_span(&TEST_USER1, &info).content, "Z");
    }

    #[test]
    fn test_user_char_span_matches_the_sender_column_color() {
        use crate::tests::{mock_settings, TEST_USER1};

        let mut settings = mock_settings();
        let mut info = RoomInfo::default();

        info.display_names.set(TEST_USER1.clone(), Some("Ada Lovelace".into()));

        // The receipt letter is styled exactly like the sender column, both when the color is
        // derived from the user ID and when it is overridden in the configuration.
        let derived = settings.get_user_char_span(&TEST_USER1, &info).style;
        assert_eq!(derived, settings.get_user_span(&TEST_USER1, &info).style);

        settings.tunables.users.insert(TEST_USER1.clone(), UserDisplayTunables {
            name: None,
            color: Some(UserColor(Color::LightRed)),
        });

        let overridden = settings.get_user_char_span(&TEST_USER1, &info).style;
        assert_eq!(overridden, settings.get_user_span(&TEST_USER1, &info).style);
        assert_ne!(overridden, derived);
    }

    #[test]
    fn test_profile_name_invalid() {
        assert_eq!(validate_profile_name(""), false);
        assert_eq!(validate_profile_name(" "), false);
        assert_eq!(validate_profile_name("a b"), false);
        assert_eq!(validate_profile_name("foo^bar"), false);
        assert_eq!(validate_profile_name("FOO/BAR"), false);
        assert_eq!(validate_profile_name("-b-c"), false);
        assert_eq!(validate_profile_name("-B-c"), false);
        assert_eq!(validate_profile_name(".b-c"), false);
        assert_eq!(validate_profile_name(".B-c"), false);
    }

    #[test]
    fn test_profile_name_valid() {
        assert_eq!(validate_profile_name("foo"), true);
        assert_eq!(validate_profile_name("FOO"), true);
        assert_eq!(validate_profile_name("a-b-c"), true);
        assert_eq!(validate_profile_name("a-B-c"), true);
        assert_eq!(validate_profile_name("a.b-c"), true);
        assert_eq!(validate_profile_name("a.B-c"), true);
    }

    #[test]
    fn test_merge_users() {
        let a = None;
        let b = vec![(user_id!("@a:b.c").to_owned(), UserDisplayTunables {
            color: Some(UserColor(Color::Red)),
            name: Some("Hello".into()),
        })]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let c = vec![(user_id!("@a:b.c").to_owned(), UserDisplayTunables {
            color: Some(UserColor(Color::Green)),
            name: Some("World".into()),
        })]
        .into_iter()
        .collect::<HashMap<_, _>>();

        let res = merge_maps(a.clone(), a.clone());
        assert_eq!(res, None);

        let res = merge_maps(a.clone(), Some(b.clone()));
        assert_eq!(res, Some(b.clone()));

        let res = merge_maps(Some(b.clone()), a.clone());
        assert_eq!(res, Some(b.clone()));

        let res = merge_maps(Some(b.clone()), Some(b.clone()));
        assert_eq!(res, Some(b.clone()));

        let res = merge_maps(Some(b.clone()), Some(c.clone()));
        assert_eq!(res, Some(b.clone()));

        let res = merge_maps(Some(c.clone()), Some(b.clone()));
        assert_eq!(res, Some(c.clone()));
    }

    #[test]
    fn test_parse_tunables() {
        let res: Tunables = serde_json::from_str("{}").unwrap();
        assert_eq!(res.typing_notice_send, None);
        assert_eq!(res.typing_notice_display, None);
        assert_eq!(res.users, None);

        let res: Tunables = serde_json::from_str("{\"typing_notice_send\": true}").unwrap();
        assert_eq!(res.typing_notice_send, Some(true));
        assert_eq!(res.typing_notice_display, None);
        assert_eq!(res.users, None);

        let res: Tunables = serde_json::from_str("{\"typing_notice_send\": false}").unwrap();
        assert_eq!(res.typing_notice_send, Some(false));
        assert_eq!(res.typing_notice_display, None);
        assert_eq!(res.users, None);

        let res: Tunables = serde_json::from_str("{\"users\": {}}").unwrap();
        assert_eq!(res.typing_notice_send, None);
        assert_eq!(res.typing_notice_display, None);
        assert_eq!(res.users, Some(HashMap::new()));

        let res: Tunables = serde_json::from_str(
            "{\"users\": {\"@a:b.c\": {\"color\": \"black\", \"name\": \"Tim\"}}}",
        )
        .unwrap();
        assert_eq!(res.typing_notice_send, None);
        assert_eq!(res.typing_notice_display, None);
        let users = vec![(user_id!("@a:b.c").to_owned(), UserDisplayTunables {
            color: Some(UserColor(Color::Black)),
            name: Some("Tim".into()),
        })];
        assert_eq!(res.users, Some(users.into_iter().collect()));
    }

    #[test]
    fn test_parse_tunables_username_display() {
        let res: Tunables = serde_json::from_str("{\"username_display\": \"username\"}").unwrap();
        assert_eq!(res.username_display, Some(UserDisplayStyle::Username));

        let res: Tunables = serde_json::from_str("{\"username_display\": \"localpart\"}").unwrap();
        assert_eq!(res.username_display, Some(UserDisplayStyle::LocalPart));

        let res: Tunables =
            serde_json::from_str("{\"username_display\": \"displayname\"}").unwrap();
        assert_eq!(res.username_display, Some(UserDisplayStyle::DisplayName));
    }

    #[test]
    fn test_parse_tunables_sort() {
        let res: Tunables = serde_json::from_str(
            r#"{"sort": {"members": ["server","~localpart"],"spaces":["~favorite", "alias"]}}"#,
        )
        .unwrap();
        assert_eq!(
            res.sort.members,
            Some(vec![
                SortColumn(SortFieldUser::Server, SortOrder::Ascending),
                SortColumn(SortFieldUser::LocalPart, SortOrder::Descending),
            ])
        );
        assert_eq!(
            res.sort.spaces,
            Some(vec![
                SortColumn(SortFieldRoom::Favorite, SortOrder::Descending),
                SortColumn(SortFieldRoom::Alias, SortOrder::Ascending),
            ])
        );
        assert_eq!(res.sort.rooms, None);
        assert_eq!(res.sort.dms, None);

        // Check that we get the right default "rooms" and "dms" values.
        let res = res.values().unwrap();
        assert_eq!(res.sort.members, vec![
            SortColumn(SortFieldUser::Server, SortOrder::Ascending),
            SortColumn(SortFieldUser::LocalPart, SortOrder::Descending),
        ]);
        assert_eq!(res.sort.spaces, vec![
            SortColumn(SortFieldRoom::Favorite, SortOrder::Descending),
            SortColumn(SortFieldRoom::Alias, SortOrder::Ascending),
        ]);
        assert_eq!(res.sort.rooms, Vec::from(DEFAULT_ROOM_SORT));
        assert_eq!(res.sort.dms, Vec::from(DEFAULT_ROOM_SORT));
    }

    #[test]
    fn test_parse_layout() {
        let user = WindowPath::UserId(user_id!("@user:example.com").to_owned());
        let alias = WindowPath::AliasId(OwnedRoomAliasId::try_from("#room:example.com").unwrap());
        let room = WindowPath::RoomId(OwnedRoomId::try_from("!room:example.com").unwrap());
        let dms = WindowPath::Window(IambId::DirectList);
        let welcome = WindowPath::Window(IambId::Welcome);

        let res: Layout = serde_json::from_str("{\"style\": \"restore\"}").unwrap();
        assert_eq!(res, Layout::Restore);

        let res: Layout = serde_json::from_str("{\"style\": \"new\"}").unwrap();
        assert_eq!(res, Layout::New);

        let res: Layout = serde_json::from_str(
            "{\"style\": \"config\", \"tabs\": [{\"window\":\"@user:example.com\"}]}",
        )
        .unwrap();
        assert_eq!(res, Layout::Config {
            tabs: vec![WindowLayout::Window { window: user.clone() }]
        });

        let res: Layout = serde_json::from_str(
            "{\
            \"style\": \"config\",\
            \"tabs\": [\
                {\"split\":[\
                    {\"window\":\"@user:example.com\"},\
                    {\"window\":\"#room:example.com\"}\
                ]},\
                {\"split\":[\
                    {\"window\":\"!room:example.com\"},\
                    {\"split\":[\
                        {\"window\":\"iamb://dms\"},\
                        {\"window\":\"iamb://welcome\"}\
                    ]}\
                ]}\
            ]}",
        )
        .unwrap();
        let split1 = WindowLayout::Split {
            split: vec![
                WindowLayout::Window { window: user.clone() },
                WindowLayout::Window { window: alias },
            ],
        };
        let split2 = WindowLayout::Split {
            split: vec![WindowLayout::Window { window: dms }, WindowLayout::Window {
                window: welcome,
            }],
        };
        let split3 = WindowLayout::Split {
            split: vec![WindowLayout::Window { window: room }, split2],
        };
        let tabs = vec![split1, split3];
        assert_eq!(res, Layout::Config { tabs });
    }

    #[test]
    fn test_parse_macros() {
        let res: Macros = serde_json::from_str("{\"i|c\":{\"jj\":\"<Esc>\"}}").unwrap();
        assert_eq!(res.len(), 1);

        let modes = VimModes(vec![VimMode::Insert, VimMode::Command]);
        let mapped = res.get(&modes).unwrap();
        assert_eq!(mapped.len(), 1);

        let j = "j".parse::<TerminalKey>().unwrap();
        let esc = "<Esc>".parse::<TerminalKey>().unwrap();

        let jj = Keys(vec![j, j], "jj".into());
        let run = mapped.get(&jj).unwrap();
        let exp = Keys(vec![esc], "<Esc>".into());
        assert_eq!(run, &exp);
    }

    #[test]
    fn test_parse_notify_via() {
        assert_eq!(NotifyVia { bell: false, desktop: true }, NotifyVia::default());
        assert_eq!(
            NotifyVia { bell: false, desktop: true },
            serde_json::from_str(r#""desktop""#).unwrap()
        );
        assert_eq!(
            NotifyVia { bell: true, desktop: false },
            serde_json::from_str(r#""bell""#).unwrap()
        );
        assert_eq!(
            NotifyVia { bell: true, desktop: true },
            serde_json::from_str(r#""bell|desktop""#).unwrap()
        );
        assert_eq!(
            NotifyVia { bell: true, desktop: true },
            serde_json::from_str(r#""desktop|bell""#).unwrap()
        );
        assert!(serde_json::from_str::<NotifyVia>(r#""other""#).is_err());
        assert!(serde_json::from_str::<NotifyVia>(r#""""#).is_err());
    }

    #[test]
    fn test_parse_notifications_focus_tui() {
        let res: Notifications = serde_json::from_str("{\"enabled\": true}").unwrap();
        assert_eq!(res.focus_tui, None);

        let res: Notifications =
            serde_json::from_str("{\"enabled\": true, \"focus_tui\": \"iamb\"}").unwrap();
        assert_eq!(res.focus_tui, Some("iamb".into()));
    }

    #[test]
    fn test_parse_cursor_shape() {
        assert_eq!(CursorShape::Default, CursorShape::default());
        assert_eq!(CursorShape::Default, serde_json::from_str(r#""default""#).unwrap());
        assert_eq!(CursorShape::Block, serde_json::from_str(r#""block""#).unwrap());
        assert_eq!(CursorShape::Line, serde_json::from_str(r#""line""#).unwrap());
        assert_eq!(CursorShape::Underline, serde_json::from_str(r#""underline""#).unwrap());
        assert!(serde_json::from_str::<CursorShape>(r#""beam""#).is_err());
    }

    #[test]
    fn test_load_example_config_toml() {
        let path = PathBuf::from("config.example.toml");
        let config = IambConfig::load_toml(&path).expect("can load example_config.toml");

        let IambConfig {
            profiles,
            default_profile,
            settings,
            dirs,
            layout,
            macros,
        } = &config;

        // There should be an example object for each top-level field.
        assert!(!profiles.is_empty());
        assert!(default_profile.is_some());
        assert!(settings.is_some());
        assert!(dirs.is_some());
        assert!(layout.is_some());
        assert!(macros.is_some());
    }

    #[test]
    fn test_encryption_indicator_enabled() {
        use EncryptionState::*;

        let enc = EncryptionValues {
            indicator: EncryptionIndicator::Enabled,
            indicator_location: EncryptionIndicatorLocation::TITLE,
        };

        // Always shows in the title:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Encrypted).is_some());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::TITLE, NotEncrypted)
            .is_some());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Unknown).is_some());

        // Doesn't show in the prompt:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Encrypted).is_none());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::PROMPT, NotEncrypted)
            .is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Unknown).is_none());
    }

    #[test]
    fn test_encryption_indicator_disabled() {
        use EncryptionState::*;

        let enc = EncryptionValues {
            indicator: EncryptionIndicator::Disabled,
            indicator_location: EncryptionIndicatorLocation::TITLE,
        };

        // Never shows in the title or the prompt:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Encrypted).is_none());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::TITLE, NotEncrypted)
            .is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Unknown).is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Encrypted).is_none());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::PROMPT, NotEncrypted)
            .is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Unknown).is_none());
    }

    #[test]
    fn test_encryption_indicator_only_encrypted() {
        use EncryptionState::*;

        let enc = EncryptionValues {
            indicator: EncryptionIndicator::OnlyEncrypted,
            indicator_location: EncryptionIndicatorLocation::PROMPT,
        };

        // Shows in the prompt when encrypted or unknown:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Encrypted).is_some());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Unknown).is_some());

        // But is hidden when unencrypted:
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::PROMPT, NotEncrypted)
            .is_none());

        // Doesn't show in the title:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Encrypted).is_none());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::TITLE, NotEncrypted)
            .is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Unknown).is_none());
    }
    #[test]
    fn test_encryption_indicator_only_unencrypted() {
        use EncryptionState::*;

        let enc = EncryptionValues {
            indicator: EncryptionIndicator::OnlyUnencrypted,
            indicator_location: EncryptionIndicatorLocation::all(),
        };

        // Shows in both the prompt and title when unencrypted or unknown:
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::TITLE, NotEncrypted)
            .is_some());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Unknown).is_some());
        assert!(enc
            .get_indicator(EncryptionIndicatorLocation::PROMPT, NotEncrypted)
            .is_some());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Unknown).is_some());

        // But is hidden when encrypted:
        assert!(enc.get_indicator(EncryptionIndicatorLocation::TITLE, Encrypted).is_none());
        assert!(enc.get_indicator(EncryptionIndicatorLocation::PROMPT, Encrypted).is_none());
    }
}
