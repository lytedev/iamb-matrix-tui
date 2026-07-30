//! Code for completing Emoji shortcodes.
//!
//! Typing `:` starts a shortcode, and what follows is fuzzy-matched against every Emoji the
//! [emojis] crate knows about. The matching and the ranking are [crate::message::mention]'s, so
//! that a shortcode and a mention feel like the same completion with a different sigil.
//!
//! What gets inserted is the shortcode rather than the Emoji itself. modalkit's completion popup
//! shows the same string it inserts, so inserting the Emoji would mean offering the user a column
//! of pictures with no names to search by, which defeats the point of matching on names. The
//! shortcode also turns into the Emoji on its way out: [crate::message::compose] renders messages
//! through Markdown with shortcodes enabled.
use emojis::Emoji;

use super::mention::fuzzy_score;

/// The character that starts an Emoji shortcode.
pub const EMOJI_SIGIL: char = ':';

/// The most Emoji completions to offer at once.
const MAX_EMOJI_COMPLETIONS: usize = 50;

/// The best score of matching the needle against any of the Emoji's names.
///
/// An Emoji answers to several shortcodes and to a written-out name ("grinning face with smiling
/// eyes"), and any of them is a reasonable thing for somebody to be typing towards.
fn score(emoji: &Emoji, needle: &str) -> Option<isize> {
    emoji
        .shortcodes()
        .filter_map(|shortcode| fuzzy_score(needle, shortcode))
        .chain(fuzzy_score(needle, emoji.name()))
        .max()
}

/// The canonical shortcodes of the Emoji that match `needle`, best match first.
///
/// Emoji without a shortcode cannot be completed to, since there would be nothing to insert.
fn matching_shortcodes(needle: &str) -> Vec<&'static str> {
    let needle = needle.strip_prefix(EMOJI_SIGIL).unwrap_or(needle);

    let mut scored = emojis::iter()
        .filter_map(|emoji| Some((score(emoji, needle)?, emoji.shortcode()?)))
        .collect::<Vec<_>>();

    // Best match first: that is the one shown at the top of the popup, and the one that <Tab> and
    // <C-N> take. Equal scores fall back to the shortcode so the order is at least stable.
    scored.sort_by(|(a_score, a), (b_score, b)| b_score.cmp(a_score).then_with(|| a.cmp(b)));

    scored.into_iter().take(MAX_EMOJI_COMPLETIONS).map(|(_, shortcode)| shortcode).collect()
}

/// Fuzzy-complete an Emoji shortcode being written in a message.
///
/// `needle` is what the user has typed, including the leading `:` sigil. The returned strings are
/// wrapped in sigils, which is the spelling Markdown recognises.
pub fn complete_emojis(needle: &str) -> Vec<String> {
    matching_shortcodes(needle)
        .into_iter()
        .map(|shortcode| format!("{EMOJI_SIGIL}{shortcode}{EMOJI_SIGIL}"))
        .collect()
}

/// Fuzzy-complete an Emoji shortcode given as a command argument.
///
/// `needle` may carry a leading `:` sigil, but commands that take a reaction name do not want one
/// around what they are given, so the completions are bare.
pub fn complete_emoji_names(needle: &str) -> Vec<String> {
    matching_shortcodes(needle).into_iter().map(ToString::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_emojis_wraps_shortcodes_in_sigils() {
        let completions = complete_emojis(":smiling_face_with_tear");
        assert_eq!(completions.first().map(String::as_str), Some(":smiling_face_with_tear:"));

        let completions = complete_emoji_names(":smiling_face_with_tear");
        assert_eq!(completions.first().map(String::as_str), Some("smiling_face_with_tear"));
    }

    #[test]
    fn test_complete_emojis_matches_without_the_sigil() {
        assert_eq!(complete_emojis("polar_bear"), complete_emojis(":polar_bear"));
    }

    #[test]
    fn test_complete_emojis_matches_fuzzily() {
        // Neither of these is a prefix of the shortcode they are meant to find.
        assert!(complete_emojis(":plrbear").contains(&":polar_bear:".to_string()));
        assert!(complete_emojis(":xploding").contains(&":exploding_head:".to_string()));
    }

    #[test]
    fn test_complete_emojis_puts_the_best_match_first() {
        // The first entry is the one <Tab> and <C-N> take, so the closest match has to be it.
        let completions = complete_emojis(":canada");
        assert_eq!(completions.first().map(String::as_str), Some(":canada:"));

        let completions = complete_emojis(":polar_bear");
        assert_eq!(completions.first().map(String::as_str), Some(":polar_bear:"));
    }

    #[test]
    fn test_complete_emojis_matches_written_out_names() {
        // "polar bear" is the Emoji's name; its shortcode spells the space as an underscore.
        assert!(complete_emojis(":polar bear").contains(&":polar_bear:".to_string()));
    }

    #[test]
    fn test_complete_emojis_offers_a_bounded_list() {
        // A bare sigil matches everything, and the popup should still be a sensible size.
        assert_eq!(complete_emojis(":").len(), MAX_EMOJI_COMPLETIONS);

        assert!(complete_emojis(":zzzzzzzzzz").is_empty());
    }
}
