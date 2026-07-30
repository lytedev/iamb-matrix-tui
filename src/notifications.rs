use std::process::{Command, Stdio};
use std::time::SystemTime;

use matrix_sdk::{
    deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    notification_settings::{IsEncrypted, IsOneToOne, NotificationSettings, RoomNotificationMode},
    room::Room as MatrixRoom,
    ruma::{
        events::{
            room::message::{MessageType, Relation},
            AnyMessageLikeEventContent,
            AnySyncTimelineEvent,
        },
        serde::Raw,
        MilliSecondsSinceUnixEpoch,
        OwnedEventId,
        OwnedRoomId,
        RoomId,
    },
    Client,
    EncryptionState,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    base::{AsyncProgramStore, IambError, IambId, IambResult, NotificationJump, ProgramStore},
    config::{ApplicationSettings, NotifyVia},
};

const IAMB_XDG_NAME: &str = match option_env!("IAMB_XDG_NAME") {
    None => "iamb",
    Some(iamb) => iamb,
};

/// Notification action invoked by clicking the notification body rather than a button.
const DEFAULT_ACTION: &str = "default";

/// External helper that records which window and terminal tab iamb is running in.
const FOCUS_TUI_REGISTER_COMMAND: &str = "focus-tui-register";

/// External helper that raises the window and tab recorded by [FOCUS_TUI_REGISTER_COMMAND].
const FOCUS_TUI_COMMAND: &str = "focus-tui";

/// Where a notification should take the user when they click it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationTarget {
    pub room_id: OwnedRoomId,
    pub thread_root: Option<OwnedEventId>,
    pub event_id: OwnedEventId,
}

impl From<&NotificationTarget> for IambId {
    fn from(target: &NotificationTarget) -> Self {
        IambId::Room(target.room_id.clone(), target.thread_root.clone())
    }
}

impl From<&NotificationTarget> for NotificationJump {
    fn from(target: &NotificationTarget) -> Self {
        NotificationJump {
            window: IambId::from(target),
            event_id: target.event_id.clone(),
        }
    }
}

/// Handle for an open notification that should be closed when the user views it.
///
/// Dropping this cancels the task that is waiting for the user to click the notification, which
/// closes the notification itself.
pub struct NotificationHandle(
    #[cfg(all(feature = "desktop", unix, not(target_os = "macos")))]
    #[allow(dead_code)]
    tokio::sync::oneshot::Sender<()>,
);

/// Record where iamb is running, so that clicking a notification can bring this window back.
///
/// This has to happen at startup rather than when a notification is clicked: several terminal
/// windows typically share one process, so iamb's own identity does not name a window, and by the
/// time a notification arrives iamb is by definition not the focused window.
///
/// Entirely optional. Users opt in by setting `notifications.focus_tui`, and even then a missing
/// or failing helper is logged and ignored rather than being allowed to hold up startup.
pub fn register_focus_tui(settings: &ApplicationSettings) {
    let Some(name) = settings.tunables.notifications.focus_tui.as_deref() else {
        return;
    };

    match run_focus_tui_helper(FOCUS_TUI_REGISTER_COMMAND, name) {
        Ok(status) if status.success() => (),
        Ok(status) => tracing::warn!("{FOCUS_TUI_REGISTER_COMMAND} exited with {status}"),
        Err(err) => tracing::warn!("Failed to run {FOCUS_TUI_REGISTER_COMMAND}: {err}"),
    }
}

/// Run one of the `focus-tui` helpers, with its output detached from iamb's terminal.
fn run_focus_tui_helper(command: &str, name: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new(command)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

pub async fn register_notifications(
    client: &Client,
    settings: &ApplicationSettings,
    store: &AsyncProgramStore,
) {
    if !settings.tunables.notifications.enabled {
        return;
    }
    let notify_via = settings.tunables.notifications.via;
    let show_message = settings.tunables.notifications.show_message;
    let sound_hint = settings.tunables.notifications.sound_hint.clone();
    let focus_tui = settings.tunables.notifications.focus_tui.clone();
    let server_settings = client.notification_settings().await;
    let Some(startup_ts) = MilliSecondsSinceUnixEpoch::from_system_time(SystemTime::now()) else {
        return;
    };

    let store = store.clone();
    client
        .register_notification_handler(move |notification, room: MatrixRoom, client: Client| {
            let store = store.clone();
            let server_settings = server_settings.clone();
            let sound_hint = sound_hint.clone();
            let focus_tui = focus_tui.clone();
            async move {
                let mode = global_or_room_mode(&server_settings, &room).await;
                if mode == RoomNotificationMode::Mute {
                    return;
                }

                if is_visible_room(&store, room.room_id()).await {
                    return;
                }

                match notification.event {
                    RawAnySyncOrStrippedTimelineEvent::Sync(e) => {
                        match parse_full_notification(e, room, show_message).await {
                            Ok((summary, body, server_ts, target)) => {
                                if server_ts < startup_ts {
                                    return;
                                }

                                if is_missing_mention(&body, mode, &client) {
                                    return;
                                }

                                send_notification(
                                    &notify_via,
                                    &summary,
                                    body.as_deref(),
                                    target,
                                    &store,
                                    sound_hint.as_deref(),
                                    focus_tui.as_deref(),
                                )
                                .await;
                            },
                            Err(err) => {
                                tracing::error!("Failed to extract notification data: {err}")
                            },
                        }
                    },
                    // Stripped events may be dropped silently because they're
                    // only relevant if we're not in a room, and we presumably
                    // don't want notifications for rooms we're not in.
                    RawAnySyncOrStrippedTimelineEvent::Stripped(_) => (),
                }
            }
        })
        .await;
}

async fn send_notification(
    via: &NotifyVia,
    summary: &str,
    body: Option<&str>,
    target: NotificationTarget,
    store: &AsyncProgramStore,
    sound_hint: Option<&str>,
    focus_tui: Option<&str>,
) {
    #[cfg(feature = "desktop")]
    if via.desktop {
        send_notification_desktop(summary, body, target, store, sound_hint, focus_tui).await;
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = (summary, body, target, focus_tui, IAMB_XDG_NAME);
    }

    if via.bell {
        send_notification_bell(store).await;
    }
}

async fn send_notification_bell(store: &AsyncProgramStore) {
    let mut locked = store.lock().await;
    locked.application.ring_bell = true;
}

#[cfg(feature = "desktop")]
#[cfg_attr(target_os = "macos", allow(unused_variables))]
async fn send_notification_desktop(
    summary: &str,
    body: Option<&str>,
    target: NotificationTarget,
    _store: &AsyncProgramStore,
    sound_hint: Option<&str>,
    focus_tui: Option<&str>,
) {
    let mut desktop_notification = notify_rust::Notification::new();
    desktop_notification
        .summary(summary)
        .appname(IAMB_XDG_NAME)
        .icon(IAMB_XDG_NAME)
        .action(DEFAULT_ACTION, DEFAULT_ACTION);

    if let Some(sound_hint) = sound_hint {
        desktop_notification.sound_name(sound_hint);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    desktop_notification.urgency(notify_rust::Urgency::Normal);

    if let Some(body) = body {
        desktop_notification.body(body);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    let res = desktop_notification.show_async().await;
    #[cfg(any(not(unix), target_os = "macos"))]
    let res = desktop_notification.show();

    match res {
        Err(err) => tracing::error!("Failed to send notification: {err}"),
        Ok(handle) => {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                let (dismiss, dismissed) = tokio::sync::oneshot::channel();
                let focus_tui = focus_tui.map(ToOwned::to_owned);
                let jump = NotificationJump::from(&target);
                let store = _store.clone();

                tokio::spawn(async move {
                    if wait_for_click(&handle, dismissed).await {
                        jump_to_notification(jump, focus_tui, &store).await;
                    }

                    handle.close_async().await;
                });

                _store
                    .lock()
                    .await
                    .application
                    .open_notifications
                    .entry(target.room_id)
                    .or_default()
                    .push(NotificationHandle(dismiss));
            }
        },
    }
}

/// Wait until the user clicks `handle`, or until it is dismissed because they read the room.
///
/// Returns whether the notification was clicked.
#[cfg(all(feature = "desktop", unix, not(target_os = "macos")))]
async fn wait_for_click(
    handle: &notify_rust::NotificationHandle,
    dismissed: tokio::sync::oneshot::Receiver<()>,
) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};

    // `wait_for_action_async` reports the action through a synchronous callback, so the answer has
    // to come back out through a flag rather than the future's output.
    let clicked = AtomicBool::new(false);

    tokio::select! {
        _ = handle.wait_for_action_async(|response| {
            let is_default = matches!(
                response,
                notify_rust::ActionResponse::Custom(action) if *action == DEFAULT_ACTION
            );

            clicked.store(is_default, Ordering::Relaxed);
        }) => (),
        _ = dismissed => (),
    }

    clicked.load(Ordering::Relaxed)
}

/// Bring iamb back to the front and queue the jump to the message that was notified about.
#[cfg(all(feature = "desktop", unix, not(target_os = "macos")))]
async fn jump_to_notification(
    jump: NotificationJump,
    focus_tui: Option<String>,
    store: &AsyncProgramStore,
) {
    if let Some(name) = focus_tui {
        let raised = tokio::task::spawn_blocking(move || {
            run_focus_tui_helper(FOCUS_TUI_COMMAND, &name)
        })
        .await;

        match raised {
            Ok(Ok(status)) if status.success() => (),
            Ok(Ok(status)) => tracing::warn!("{FOCUS_TUI_COMMAND} exited with {status}"),
            Ok(Err(err)) => tracing::warn!("Failed to run {FOCUS_TUI_COMMAND}: {err}"),
            Err(err) => tracing::warn!("Failed to wait for {FOCUS_TUI_COMMAND}: {err}"),
        }
    }

    store.lock().await.application.notification_jump = Some(jump);
}

async fn global_or_room_mode(
    settings: &NotificationSettings,
    room: &MatrixRoom,
) -> RoomNotificationMode {
    let room_mode = settings.get_user_defined_room_notification_mode(room.room_id()).await;
    if let Some(mode) = room_mode {
        return mode;
    }
    let is_one_to_one = match room.is_direct().await {
        Ok(true) => IsOneToOne::Yes,
        _ => IsOneToOne::No,
    };
    let is_encrypted = match room.latest_encryption_state().await {
        Ok(EncryptionState::Encrypted) => IsEncrypted::Yes,
        _ => IsEncrypted::No,
    };
    settings
        .get_default_room_notification_mode(is_encrypted, is_one_to_one)
        .await
}

fn is_missing_mention(body: &Option<String>, mode: RoomNotificationMode, client: &Client) -> bool {
    if let Some(body) = body {
        if mode == RoomNotificationMode::MentionsAndKeywordsOnly {
            let mentioned = match client.user_id() {
                Some(user_id) => body.contains(user_id.localpart()),
                _ => false,
            };
            return !mentioned;
        }
    }
    false
}

fn is_open(locked: &mut ProgramStore, room_id: &RoomId) -> bool {
    if let Some(draw_curr) = locked.application.draw_curr {
        let info = locked.application.get_room_info(room_id.to_owned());
        if let Some(draw_last) = info.draw_last {
            return draw_last == draw_curr;
        }
    }
    false
}

fn is_focused(locked: &ProgramStore) -> bool {
    locked.application.focused
}

async fn is_visible_room(store: &AsyncProgramStore, room_id: &RoomId) -> bool {
    let mut locked = store.lock().await;

    is_focused(&locked) && is_open(&mut locked, room_id)
}

pub async fn parse_full_notification(
    event: Raw<AnySyncTimelineEvent>,
    room: MatrixRoom,
    show_body: bool,
) -> IambResult<(String, Option<String>, MilliSecondsSinceUnixEpoch, NotificationTarget)> {
    let event = event.deserialize().map_err(IambError::from)?;

    let server_ts = event.origin_server_ts();

    let sender_id = event.sender();
    let sender = room.get_member_no_sync(sender_id).await.map_err(IambError::from)?;

    let sender_name = sender
        .as_ref()
        .and_then(|m| m.display_name())
        .unwrap_or_else(|| sender_id.localpart());

    let summary = if let Some(room_name) = room.cached_display_name() {
        if room.is_direct().await.map_err(IambError::from)? && sender_name == room_name.to_string()
        {
            sender_name.to_string()
        } else {
            format!("{sender_name} in {room_name}")
        }
    } else {
        sender_name.to_string()
    };

    let body = if show_body {
        event_notification_body(&event, sender_name).map(truncate)
    } else {
        None
    };

    let target = NotificationTarget {
        room_id: room.room_id().to_owned(),
        thread_root: event_thread_root(&event),
        event_id: event.event_id().to_owned(),
    };

    return Ok((summary, body, server_ts, target));
}

/// The thread a notified-about event lives in, so that clicking it opens the thread and not just
/// the room's main timeline.
pub fn event_thread_root(event: &AnySyncTimelineEvent) -> Option<OwnedEventId> {
    let AnySyncTimelineEvent::MessageLike(event) = event else {
        return None;
    };

    let AnyMessageLikeEventContent::RoomMessage(message) = event.original_content()? else {
        return None;
    };

    match message.relates_to? {
        Relation::Thread(thread) => Some(thread.event_id),
        _ => None,
    }
}

pub fn event_notification_body(event: &AnySyncTimelineEvent, sender_name: &str) -> Option<String> {
    let AnySyncTimelineEvent::MessageLike(event) = event else {
        return None;
    };

    match event.original_content()? {
        AnyMessageLikeEventContent::RoomMessage(message) => {
            let body = match message.msgtype {
                MessageType::Audio(_) => {
                    format!("{sender_name} sent an audio file.")
                },
                MessageType::Emote(content) => content.body,
                MessageType::File(_) => {
                    format!("{sender_name} sent a file.")
                },
                MessageType::Image(_) => {
                    format!("{sender_name} sent an image.")
                },
                MessageType::Location(_) => {
                    format!("{sender_name} sent their location.")
                },
                MessageType::Notice(content) => content.body,
                MessageType::ServerNotice(content) => content.body,
                MessageType::Text(content) => content.body,
                MessageType::Video(_) => {
                    format!("{sender_name} sent a video.")
                },
                MessageType::VerificationRequest(_) => {
                    format!("{sender_name} sent a verification request.")
                },
                _ => {
                    format!("[Unknown message type: {:?}]", &message.msgtype)
                },
            };
            Some(body)
        },
        AnyMessageLikeEventContent::Sticker(_) => Some(format!("{sender_name} sent a sticker.")),
        _ => None,
    }
}

fn truncate(s: String) -> String {
    static MAX_LENGTH: usize = 5000;
    if s.graphemes(true).count() > MAX_LENGTH {
        let truncated: String = s.graphemes(true).take(MAX_LENGTH).collect();
        truncated + "..."
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use matrix_sdk::ruma::{event_id, room_id};

    fn message_event(relates_to: &str) -> AnySyncTimelineEvent {
        let json = format!(
            r#"{{
                "type": "m.room.message",
                "event_id": "$message:example.com",
                "sender": "@user:example.com",
                "origin_server_ts": 1,
                "content": {{ "msgtype": "m.text", "body": "hello"{relates_to} }}
            }}"#
        );

        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_event_thread_root_unthreaded() {
        assert_eq!(event_thread_root(&message_event("")), None);
    }

    #[test]
    fn test_event_thread_root_threaded() {
        let relates_to = r#", "m.relates_to": {
            "rel_type": "m.thread",
            "event_id": "$root:example.com",
            "is_falling_back": true,
            "m.in_reply_to": { "event_id": "$root:example.com" }
        }"#;

        assert_eq!(
            event_thread_root(&message_event(relates_to)),
            Some(event_id!("$root:example.com").to_owned())
        );
    }

    #[test]
    fn test_notification_target_window() {
        let room_id = room_id!("!room:example.com").to_owned();
        let thread_root = event_id!("$root:example.com").to_owned();
        let event_id = event_id!("$message:example.com").to_owned();

        let target = NotificationTarget {
            room_id: room_id.clone(),
            thread_root: None,
            event_id: event_id.clone(),
        };
        assert_eq!(IambId::from(&target), IambId::Room(room_id.clone(), None));

        let target = NotificationTarget {
            room_id: room_id.clone(),
            thread_root: Some(thread_root.clone()),
            event_id: event_id.clone(),
        };
        assert_eq!(IambId::from(&target), IambId::Room(room_id, Some(thread_root)));
    }

    /// A click has to carry the notified message, not just the room, so that it can be selected.
    #[test]
    fn test_notification_jump_keeps_the_event() {
        let room_id = room_id!("!room:example.com").to_owned();
        let thread_root = event_id!("$root:example.com").to_owned();
        let event_id = event_id!("$message:example.com").to_owned();

        let target = NotificationTarget {
            room_id: room_id.clone(),
            thread_root: Some(thread_root.clone()),
            event_id: event_id.clone(),
        };

        assert_eq!(NotificationJump::from(&target), NotificationJump {
            window: IambId::Room(room_id, Some(thread_root)),
            event_id,
        });
    }
}
