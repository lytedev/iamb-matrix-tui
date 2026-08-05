//! # Yanked message text
//!
//! This module holds the one and only plain text format for yanked messages. The reader of the
//! text is a person or an agent outside of the terminal, so the text carries no terminal
//! decoration: no reply arrows, no read receipts, no image placeholders and no styling.
//!
//! Change the format here, and every yank changes with it.

use std::borrow::Cow;

use chrono::Local as LocalTz;

use crate::base::RoomInfo;
use crate::config::ApplicationSettings;
use crate::message::{Message, MessageEvent, MessageTimeStamp};

/// The time format in the header of a yanked message.
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// The time shown for a message that the server has not accepted yet.
const TIME_UNSENT: &str = "unsent";

/// The mark that says a message belongs to a thread.
const THREAD_MARK: &str = " (thread)";

/// The text between two yanked messages.
/// A blank line between messages.
///
/// One newline is not enough. A body that runs to several lines becomes indistinguishable from the
/// next message, which is the whole problem the reader of a dump has to solve.
const MESSAGE_SEPARATOR: &str = "\n\n";

/// Format the timestamp for the header of a yanked message.
fn show_time(timestamp: &MessageTimeStamp) -> String {
    match timestamp {
        MessageTimeStamp::OriginServer(ms) => {
            let seconds = i64::from(*ms) / 1000;
            match chrono::DateTime::from_timestamp(seconds, 0) {
                Some(time) => {
                    let time: chrono::DateTime<LocalTz> = time.into();
                    time.format(TIME_FORMAT).to_string()
                },
                None => TIME_UNSENT.to_string(),
            }
        },
        MessageTimeStamp::LocalEcho => TIME_UNSENT.to_string(),
    }
}

/// Put an `@` in front of each name the message mentions.
///
/// A mention reaches the plain body as the bare display name, so a dump cannot tell a mention from
/// someone merely being talked about. The truth is in `m.mentions`, which names the users by id, so
/// this marks their display names rather than guessing from the text.
///
/// Returns the body untouched when there is nothing to mark, so the common case allocates nothing.
fn mark_mentions<'a>(body: &'a str, msg: &Message, info: &RoomInfo) -> Cow<'a, str> {
    let MessageEvent::Original(ev) = &msg.event else {
        return Cow::Borrowed(body);
    };

    let Some(mentions) = &ev.content.mentions else {
        return Cow::Borrowed(body);
    };

    let mut out = Cow::Borrowed(body);

    for user in &mentions.user_ids {
        let Some(name) = info.display_names.get(user) else {
            continue;
        };
        let name: &str = name.as_ref();

        if name.is_empty() || !out.contains(name) {
            continue;
        }

        // Skip a name that is already marked, so that a client which sends the `@` itself does not
        // end up with two.
        let marked = format!("@{name}");

        if out.contains(&marked) {
            continue;
        }

        out = Cow::Owned(out.replace(name, &marked));
    }

    out
}

/// Format a single message as plain text.
///
/// A body that fits on one line stays on the header line. A body with more than one line starts
/// on its own line, so that fenced code blocks survive the copy without a prefix.
pub fn show_message(msg: &Message, info: &RoomInfo, settings: &ApplicationSettings) -> String {
    let time = show_time(&msg.timestamp);
    // Prefer the display name over the Matrix user id, whatever the scrollback is set to show.
    // The reader of yanked text wants a human name. Two users with the same display name are
    // already told apart by the display name store, which adds the user id back for both of them.
    let sender = info
        .display_names
        .get(&msg.sender)
        .unwrap_or_else(|| settings.get_user_name(&msg.sender, info));
    let thread = if msg.thread_root().is_some() { THREAD_MARK } else { "" };
    let body = msg.event.body();
    let body = mark_mentions(body.trim_end_matches('\n'), msg, info);
    let body = body.as_ref();

    if body.contains('\n') {
        format!("[{time}] {sender}{thread}:\n{body}")
    } else {
        format!("[{time}] {sender}{thread}: {body}")
    }
}

/// Format a run of messages as plain text.
pub fn show_messages<'a>(
    msgs: impl Iterator<Item = &'a Message>,
    info: &RoomInfo,
    settings: &ApplicationSettings,
) -> String {
    let mut out = String::new();

    for msg in msgs {
        if !out.is_empty() {
            out.push_str(MESSAGE_SEPARATOR);
        }

        out.push_str(&show_message(msg, info, settings));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageEvent;
    use crate::tests::*;

    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
    use matrix_sdk::ruma::events::Mentions;

    /// Format the time the same way the yanked text does, but from `chrono` directly, so that the
    /// test does not depend on the timezone of the machine that runs it.
    fn expected_time(millis: i64) -> String {
        let time = chrono::DateTime::from_timestamp(millis / 1000, 0).unwrap();
        let time: chrono::DateTime<LocalTz> = time.into();
        time.format(TIME_FORMAT).to_string()
    }

    #[test]
    fn test_a_single_line_message_stays_on_the_header_line() {
        let info = mock_room();
        let settings = mock_settings();
        let msg = mock_message2();

        let expected = format!("[{}] @user2:example.com: helium", expected_time(1));
        assert_eq!(show_message(&msg, &info, &settings), expected);
    }

    #[test]
    fn test_a_multiline_message_starts_on_its_own_line() {
        let info = mock_room();
        let settings = mock_settings();
        let msg = mock_message3();

        let expected = format!(
            "[{}] @user2:example.com:\nthis\nis\na\nmultiline\nmessage",
            expected_time(2)
        );
        assert_eq!(show_message(&msg, &info, &settings), expected);
    }

    #[test]
    fn test_a_code_block_survives_without_a_prefix() {
        let info = mock_room();
        let settings = mock_settings();
        let body = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
        let content = RoomMessageEventContent::text_plain(body);
        let msg = mock_room1_message(content, TEST_USER1.clone(), MSG3_KEY.clone());

        let expected = format!("[{}] @user1:example.com:\n{body}", expected_time(2));
        assert_eq!(show_message(&msg, &info, &settings), expected);
    }

    #[test]
    fn test_a_redacted_message_says_it_was_removed() {
        let info = mock_room();
        let settings = mock_settings();
        let event = MessageEvent::Redacted(MSG2_EVID.clone(), Some("spam".into()));
        let msg = Message::new(event, TEST_USER1.clone(), MSG2_KEY.0);

        let text = show_message(&msg, &info, &settings);
        let expected = format!("[{}] @user1:example.com: [Redacted: \"spam\"]", expected_time(1));
        assert_eq!(text, expected);
    }

    #[test]
    fn test_the_display_name_is_used_when_the_room_knows_one() {
        let mut info = mock_room();
        info.display_names.set(TEST_USER2.clone(), Some("Ada Lovelace".into()));

        let settings = mock_settings();
        let msg = mock_message2();

        let expected = format!("[{}] Ada Lovelace: helium", expected_time(1));
        assert_eq!(show_message(&msg, &info, &settings), expected);
    }

    #[test]
    fn test_two_users_with_one_display_name_keep_their_user_ids() {
        let mut info = mock_room();
        info.display_names.set(TEST_USER1.clone(), Some("Ada".into()));
        info.display_names.set(TEST_USER2.clone(), Some("Ada".into()));

        let settings = mock_settings();
        let msg = mock_message2();

        let expected = format!("[{}] Ada (@user2:example.com): helium", expected_time(1));
        assert_eq!(show_message(&msg, &info, &settings), expected);
    }

    /// A message from TEST_USER2 whose body names TEST_USER1, optionally as a real mention.
    fn mock_message_naming_user1(mention: bool) -> Message {
        let content = RoomMessageEventContent::text_plain("Ada Lovelace let me know what you need");
        let mut msg = mock_room1_message(content, TEST_USER2.clone(), MSG2_KEY.clone());

        if mention {
            if let MessageEvent::Original(ev) = &mut msg.event {
                let mut mentions = Mentions::new();
                mentions.user_ids.insert(TEST_USER1.clone());
                ev.content.mentions = Some(mentions);
            }
        }

        msg
    }

    #[test]
    fn test_a_mentioned_name_is_marked_with_an_at_sign() {
        let mut info = mock_room();
        info.display_names.set(TEST_USER1.clone(), Some("Ada Lovelace".into()));

        let text = show_message(&mock_message_naming_user1(true), &info, &mock_settings());

        assert!(text.contains("@Ada Lovelace let me know"), "got: {text}");
    }

    #[test]
    fn test_a_name_that_is_only_talked_about_is_left_alone() {
        let mut info = mock_room();
        info.display_names.set(TEST_USER1.clone(), Some("Ada Lovelace".into()));

        let text = show_message(&mock_message_naming_user1(false), &info, &mock_settings());

        assert!(!text.contains("@Ada"), "got: {text}");
    }

    #[test]
    fn test_messages_are_separated_by_a_blank_line() {
        let info = mock_room();
        let settings = mock_settings();
        let msgs = [mock_message2(), mock_message4()];

        let text = show_messages(msgs.iter(), &info, &settings);
        let expected = format!(
            "[{}] @user2:example.com: helium\n\n[{}] @user1:example.com: help",
            expected_time(1),
            expected_time(2)
        );
        assert_eq!(text, expected);
    }
}
