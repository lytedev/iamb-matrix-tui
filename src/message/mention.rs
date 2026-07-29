//! Code for completing and sending user mentions ("pills").
//!
//! A mention is written in the message bar as a Markdown link to a [matrix.to] user permalink,
//! e.g. `[Ada](https://matrix.to/#/@ada:example.com)`. That representation was chosen because it
//! survives ordinary editing, needs no hidden state alongside the buffer, and already turns into
//! the `<a href="https://matrix.to/#/@ada:example.com">Ada</a>` anchor that the spec asks for when
//! it goes through the existing Markdown pipeline in [crate::message::compose].
//!
//! Two things still have to happen at send time, and [parse_mentions] does both: the plain-text
//! `body` fallback gets the link syntax reduced back down to just the label, and the mentioned
//! user IDs are collected so that `m.mentions` can be populated.
//!
//! [matrix.to]: https://spec.matrix.org/v1.18/appendices/#matrixto-navigation
use std::collections::BTreeSet;

use matrix_sdk::ruma::{OwnedUserId, UserId};
use regex::{Captures, Regex};

/// The character that starts a mention, and also a Matrix user ID.
pub const MENTION_SIGIL: char = '@';

/// The prefix of a [matrix.to] permalink, which mentions point at.
///
/// [matrix.to]: https://spec.matrix.org/v1.18/appendices/#matrixto-navigation
const MATRIX_TO_PREFIX: &str = "https://matrix.to/#/";

/// The percent-encoded spelling of the `@` that starts a user ID.
///
/// Other clients encode the sigil in permalinks they generate, so mentions that arrive by way of
/// an edit or a paste can use it even though we never write it ourselves.
const PERCENT_ENCODED_AT: &str = "%40";

/// The most member completions to offer at once.
const MAX_MENTION_COMPLETIONS: usize = 50;

/// Added when a needle character matches immediately after the previous one did.
const CONSECUTIVE_MATCH_BONUS: isize = 8;

/// Added when a needle character matches at the start of the haystack or of a word within it.
const WORD_START_BONUS: isize = 6;

/// Subtracted for each haystack character skipped over to find a match.
const SKIPPED_CHAR_PENALTY: isize = 1;

/// The characters that separate words for the purposes of [WORD_START_BONUS].
const WORD_SEPARATORS: &[char] = &[' ', '\t', '-', '_', '.', '@', ':'];

/// A room member that a mention can be completed to.
pub struct MentionCandidate {
    /// The member's Matrix user ID.
    pub user_id: OwnedUserId,

    /// The member's display name, if they have set one.
    pub display_name: Option<String>,

    /// Whether the homeserver considers this member's display name ambiguous in the room.
    pub display_name_ambiguous: bool,
}

impl MentionCandidate {
    pub fn new(
        user_id: OwnedUserId,
        display_name: Option<String>,
        display_name_ambiguous: bool,
    ) -> Self {
        MentionCandidate { user_id, display_name, display_name_ambiguous }
    }

    /// The best score of matching the needle against either of the member's names.
    fn score(&self, needle: &str) -> Option<isize> {
        let by_localpart = fuzzy_score(needle, self.user_id.localpart());
        let by_display_name =
            self.display_name.as_deref().and_then(|name| fuzzy_score(needle, name));

        by_localpart.max(by_display_name)
    }

    /// The text to show inside the mention's link.
    ///
    /// Members without a display name are labelled with their full user ID. Members who share a
    /// display name with somebody else in the room get their user ID appended, matching how
    /// [crate::base::DisplayNameStore] disambiguates names in the scrollback. The link itself is
    /// always built from the user ID, so the mention resolves to the right person either way.
    fn label(&self, ambiguous: bool) -> String {
        match &self.display_name {
            None => self.user_id.to_string(),
            Some(name) if ambiguous => format!("{name} ({})", self.user_id),
            Some(name) => name.clone(),
        }
    }
}

/// Score how well `needle` fuzzy-matches `haystack`, if it matches at all.
///
/// This is a subsequence match: every character of the needle has to show up in the haystack, in
/// order, but not necessarily next to each other. Matches that run together, that start a word, and
/// that skip less of the haystack score higher. Higher scores are better matches.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<isize> {
    if needle.is_empty() {
        return Some(0);
    }

    let mut chars = haystack.chars().enumerate().peekable();
    let mut previous_was_separator = true;
    let mut previous_matched_at = None;
    let mut score = 0;

    for wanted in needle.chars().flat_map(char::to_lowercase) {
        loop {
            let (index, c) = chars.next()?;
            let is_separator = WORD_SEPARATORS.contains(&c);
            let at_word_start = previous_was_separator;
            previous_was_separator = is_separator;

            if c.to_lowercase().next() != Some(wanted) {
                score -= SKIPPED_CHAR_PENALTY;
                continue;
            }

            if at_word_start {
                score += WORD_START_BONUS;
            }

            if let (Some(previous), Some(preceding)) = (previous_matched_at, index.checked_sub(1)) {
                if previous == preceding {
                    score += CONSECUTIVE_MATCH_BONUS;
                }
            }

            previous_matched_at = Some(index);
            break;
        }
    }

    Some(score)
}

/// Escape the characters that would otherwise end a Markdown link label early.
fn escape_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]")
}

/// Build the text that a completed mention inserts into the message bar.
fn mention_link(label: &str, user_id: &UserId) -> String {
    format!("[{}]({MATRIX_TO_PREFIX}{user_id})", escape_label(label))
}

/// The display names that more than one of these members answers to.
///
/// The homeserver also tells us which names it thinks are ambiguous, but it only does so for the
/// membership state it has told us about, so we check the list we actually have in hand as well.
fn ambiguous_names(candidates: &[MentionCandidate]) -> BTreeSet<&str> {
    let mut ambiguous = BTreeSet::new();
    let mut seen = BTreeSet::new();

    for candidate in candidates {
        if let Some(name) = &candidate.display_name {
            if !seen.insert(name.as_str()) {
                ambiguous.insert(name.as_str());
            }
        }
    }

    ambiguous
}

/// Fuzzy-complete a mention against the members of a room.
///
/// `needle` is what the user has typed, including the leading `@` sigil. The returned strings are
/// mention links ready to be inserted in place of it.
pub fn complete_mentions(needle: &str, candidates: Vec<MentionCandidate>) -> Vec<String> {
    let needle = needle.strip_prefix(MENTION_SIGIL).unwrap_or(needle);
    let ambiguous_names = ambiguous_names(&candidates);

    let mut scored = candidates
        .iter()
        .filter_map(|candidate| Some((candidate.score(needle)?, candidate)))
        .collect::<Vec<_>>();

    // Sort by descending score, then by user ID so that equal scores stay in a stable order.
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score.cmp(a_score).then_with(|| a.user_id.cmp(&b.user_id))
    });

    scored
        .into_iter()
        .take(MAX_MENTION_COMPLETIONS)
        .map(|(_, candidate)| {
            let ambiguous = candidate.display_name_ambiguous ||
                candidate
                    .display_name
                    .as_deref()
                    .is_some_and(|name| ambiguous_names.contains(name));

            mention_link(&candidate.label(ambiguous), &candidate.user_id)
        })
        .collect()
}

/// What [parse_mentions] found in a composed message.
pub struct ParsedMentions {
    /// The message with each mention link reduced to its label, for the plain-text `body`.
    pub body: String,

    /// The users mentioned, for `m.mentions`.
    pub user_ids: BTreeSet<OwnedUserId>,
}

fn mention_regex() -> &'static Regex {
    static MENTION: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

    MENTION.get_or_init(|| {
        Regex::new(r"\[((?:[^\[\]\\]|\\.)*)\]\(https://matrix\.to/#/((?:@|%40)[^)\s]+)\)")
            .expect("the mention pattern should always compile")
    })
}

/// Undo the escaping that [escape_label] applies.
fn unescape_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            _ => out.push(c),
        }
    }

    out
}

fn captured_user_id(captures: &Captures) -> Option<OwnedUserId> {
    let target = captures.get(2)?.as_str();
    let target = match target.strip_prefix(PERCENT_ENCODED_AT) {
        Some(rest) => format!("@{rest}"),
        None => target.to_string(),
    };

    UserId::parse(target).ok()
}

/// Pull the mentions out of a composed message.
pub fn parse_mentions(input: &str) -> ParsedMentions {
    let mut user_ids = BTreeSet::new();

    let body = mention_regex().replace_all(input, |captures: &Captures| {
        let Some(user_id) = captured_user_id(captures) else {
            // Not a user permalink we understand, so leave it alone.
            return captures[0].to_string();
        };

        user_ids.insert(user_id);

        unescape_label(&captures[1])
    });

    ParsedMentions { body: body.into_owned(), user_ids }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    fn candidate(user_id: &UserId, display_name: Option<&str>) -> MentionCandidate {
        MentionCandidate::new(user_id.to_owned(), display_name.map(ToString::to_string), false)
    }

    /// A member the homeserver has told us has an ambiguous display name.
    fn ambiguous_candidate(user_id: &UserId, display_name: &str) -> MentionCandidate {
        MentionCandidate::new(user_id.to_owned(), Some(display_name.to_string()), true)
    }

    #[test]
    fn test_fuzzy_score_matches_subsequences() {
        assert!(fuzzy_score("ada", "ada").is_some());
        assert!(fuzzy_score("ad", "ada").is_some());
        assert!(fuzzy_score("AD", "ada").is_some());
        assert!(fuzzy_score("df", "Daniel Flanagan").is_some());
        assert!(fuzzy_score("", "ada").is_some());

        assert!(fuzzy_score("adam", "ada").is_none());
        assert!(fuzzy_score("dad", "ada").is_none());
    }

    #[test]
    fn test_fuzzy_score_prefers_tighter_matches() {
        let prefix = fuzzy_score("ada", "adalovelace").unwrap();
        let scattered = fuzzy_score("ada", "axdxaxlovelace").unwrap();
        assert!(prefix > scattered, "{} should beat {}", prefix, scattered);

        let word_start = fuzzy_score("df", "Daniel Flanagan").unwrap();
        let mid_word = fuzzy_score("df", "Dorothy Vaughan-f").unwrap();
        assert!(word_start > mid_word, "{} should beat {}", word_start, mid_word);
    }

    #[test]
    fn test_complete_mentions_by_localpart_and_display_name() {
        let members = vec![
            candidate(user_id!("@ada:example.com"), Some("Ada Lovelace")),
            candidate(user_id!("@grace:example.com"), Some("Grace Hopper")),
        ];

        assert_eq!(complete_mentions("@ada", members), vec![
            "[Ada Lovelace](https://matrix.to/#/@ada:example.com)".to_string()
        ]);

        let members = vec![
            candidate(user_id!("@ada:example.com"), Some("Ada Lovelace")),
            candidate(user_id!("@grace:example.com"), Some("Grace Hopper")),
        ];

        // "gh" matches neither localpart, but does match "Grace Hopper".
        assert_eq!(complete_mentions("@gh", members), vec![
            "[Grace Hopper](https://matrix.to/#/@grace:example.com)".to_string()
        ]);
    }

    #[test]
    fn test_complete_mentions_labels_members_without_display_names() {
        let members = vec![candidate(user_id!("@ada:example.com"), None)];

        assert_eq!(complete_mentions("@ada", members), vec![
            "[@ada:example.com](https://matrix.to/#/@ada:example.com)".to_string()
        ]);
    }

    #[test]
    fn test_complete_mentions_disambiguates_shared_display_names() {
        let members = vec![
            candidate(user_id!("@ada1:example.com"), Some("Ada")),
            candidate(user_id!("@ada2:example.com"), Some("Ada")),
            candidate(user_id!("@grace:example.com"), Some("Ada Lovelace")),
        ];

        assert_eq!(complete_mentions("@ada", members), vec![
            "[Ada (@ada1:example.com)](https://matrix.to/#/@ada1:example.com)".to_string(),
            "[Ada (@ada2:example.com)](https://matrix.to/#/@ada2:example.com)".to_string(),
            "[Ada Lovelace](https://matrix.to/#/@grace:example.com)".to_string(),
        ]);
    }

    #[test]
    fn test_complete_mentions_trusts_the_homeserver_about_ambiguity() {
        // The other member sharing the name isn't in the list we were handed, so the flag on the
        // candidate is the only thing that can tell us to disambiguate.
        let members = vec![ambiguous_candidate(user_id!("@ada1:example.com"), "Ada")];

        assert_eq!(complete_mentions("@ada", members), vec![
            "[Ada (@ada1:example.com)](https://matrix.to/#/@ada1:example.com)".to_string(),
        ]);
    }

    #[test]
    fn test_complete_mentions_escapes_brackets_in_labels() {
        let members = vec![candidate(user_id!("@ada:example.com"), Some("[Ada]"))];

        assert_eq!(complete_mentions("@ada", members), vec![
            "[\\[Ada\\]](https://matrix.to/#/@ada:example.com)".to_string()
        ]);
    }

    #[test]
    fn test_parse_mentions() {
        let parsed = parse_mentions("hi [Ada](https://matrix.to/#/@ada:example.com)!");
        assert_eq!(parsed.body, "hi Ada!");
        assert_eq!(parsed.user_ids, BTreeSet::from([user_id!("@ada:example.com").to_owned()]));

        let parsed = parse_mentions(
            "[Ada](https://matrix.to/#/@ada:example.com) & [Grace](https://matrix.to/#/%40grace:example.com)",
        );
        assert_eq!(parsed.body, "Ada & Grace");
        assert_eq!(
            parsed.user_ids,
            BTreeSet::from([
                user_id!("@ada:example.com").to_owned(),
                user_id!("@grace:example.com").to_owned(),
            ])
        );
    }

    #[test]
    fn test_parse_mentions_unescapes_labels() {
        let parsed = parse_mentions("[\\[Ada\\]](https://matrix.to/#/@ada:example.com)");
        assert_eq!(parsed.body, "[Ada]");
        assert_eq!(parsed.user_ids, BTreeSet::from([user_id!("@ada:example.com").to_owned()]));
    }

    #[test]
    fn test_parse_mentions_ignores_other_links() {
        let input = "see [the room](https://matrix.to/#/%23iamb:0x.badd.cafe) and [docs](https://iamb.chat)";
        let parsed = parse_mentions(input);
        assert_eq!(parsed.body, input);
        assert!(parsed.user_ids.is_empty());
    }

    #[test]
    fn test_parse_mentions_leaves_plain_text_alone() {
        let input = "no mentions here\n";
        let parsed = parse_mentions(input);
        assert_eq!(parsed.body, input);
        assert!(parsed.user_ids.is_empty());
    }
}
