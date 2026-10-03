//! One short announcement per article, sized for the strictest channel so the
//! same text can be posted anywhere, by the worker or by hand.

use markdown_compiler::{PostId, PostRevisionDigest};
use thiserror::Error;

use crate::render::{ContentCatalog, SiteSnapshot};

/// X measures a post in weighted units and counts every link as 23.
const MAX_WEIGHTED_LENGTH: usize = 280;
const LINK_WEIGHT: usize = 23;
const SEPARATOR: &str = "\n\n";
const ELLIPSIS: char = '…';
/// A summary clipped below this reads as noise; the title then stands alone.
const MIN_SUMMARY_WEIGHT: usize = 24;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_SUMMARY_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 2048;

/// The title, summary, and link always fit one X post together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Teaser {
    post_id: PostId,
    title: String,
    summary: String,
    url: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TeaserView<'a> {
    pub post_id: &'a PostId,
    pub title: &'a str,
    /// Empty when the title leaves no room for a readable summary.
    pub summary: &'a str,
    pub url: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TeaserError {
    #[error("the article revision or its public page is unavailable")]
    ArticleUnavailable,
    #[error("the teaser needs a title")]
    EmptyTitle,
    #[error("the teaser link is empty, too long, or contains whitespace")]
    InvalidUrl,
    #[error("the teaser text contains control characters or exceeds its stored size")]
    InvalidText,
    #[error("the teaser does not fit one post")]
    TooLong,
}

impl Teaser {
    /// Describe the exact revision being published, never the catalog's latest.
    pub(crate) fn for_publication(
        catalog: &ContentCatalog,
        snapshot: &SiteSnapshot,
        post_id: &PostId,
        revision: &PostRevisionDigest,
    ) -> Result<Self, TeaserError> {
        let post = catalog
            .get(post_id, revision)
            .ok_or(TeaserError::ArticleUnavailable)?;
        let metadata = &post.document.metadata;
        let url = snapshot
            .post_canonical_url(&metadata.slug)
            .ok_or(TeaserError::ArticleUnavailable)?;
        Self::compose(
            post_id.clone(),
            metadata.title.as_str(),
            metadata.description.as_str(),
            url.as_str(),
        )
    }

    /// Clip the title first, then give the summary whatever room remains.
    pub(crate) fn compose(
        post_id: PostId,
        title: &str,
        description: &str,
        url: &str,
    ) -> Result<Self, TeaserError> {
        let title = single_line(title);
        let description = single_line(description);
        let room = MAX_WEIGHTED_LENGTH - LINK_WEIGHT - SEPARATOR.len();
        let title = clip(&title, room);
        let remaining = room.saturating_sub(weighted_length(&title) + SEPARATOR.len());
        let summary = if description.is_empty() || remaining < MIN_SUMMARY_WEIGHT {
            String::new()
        } else {
            clip(&description, remaining)
        };
        Self::from_parts(post_id, title, summary, url.to_owned())
    }

    /// Validate composed or stored parts; a stored teaser is never re-clipped.
    pub(crate) fn from_parts(
        post_id: PostId,
        title: String,
        summary: String,
        url: String,
    ) -> Result<Self, TeaserError> {
        if title.is_empty() {
            return Err(TeaserError::EmptyTitle);
        }
        if url.is_empty() || url.len() > MAX_URL_BYTES || url.chars().any(char::is_whitespace) {
            return Err(TeaserError::InvalidUrl);
        }
        if title.len() > MAX_TITLE_BYTES
            || summary.len() > MAX_SUMMARY_BYTES
            || title.chars().chain(summary.chars()).any(char::is_control)
        {
            return Err(TeaserError::InvalidText);
        }
        let summary_weight = match summary.is_empty() {
            true => 0,
            false => SEPARATOR.len() + weighted_length(&summary),
        };
        if weighted_length(&title) + summary_weight + SEPARATOR.len() + LINK_WEIGHT
            > MAX_WEIGHTED_LENGTH
        {
            return Err(TeaserError::TooLong);
        }
        Ok(Self {
            post_id,
            title,
            summary,
            url,
        })
    }

    /// Replace the text an Owner edited by hand. The first line is the title
    /// and the rest is the summary; nothing is clipped, so text that no longer
    /// fits one post with its link is refused.
    pub(crate) fn edited(&self, text: &str) -> Result<Self, TeaserError> {
        let text = text.trim();
        let (title, summary) = text.split_once('\n').unwrap_or((text, ""));
        Self::from_parts(
            self.post_id.clone(),
            single_line(title),
            single_line(summary),
            self.url.clone(),
        )
    }

    pub(crate) fn view(&self) -> TeaserView<'_> {
        TeaserView {
            post_id: &self.post_id,
            title: &self.title,
            summary: &self.summary,
            url: &self.url,
        }
    }

    /// The title, then the summary when present: the post text without its
    /// link, for posting by hand with the link added below or in a reply.
    pub(crate) fn lead(&self) -> String {
        match self.summary.is_empty() {
            true => self.title.clone(),
            false => format!("{}{SEPARATOR}{}", self.title, self.summary),
        }
    }
}

fn single_line(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// X counts most Latin text as one unit per character and everything else,
/// including emoji and CJK, as two.
fn character_weight(character: char) -> usize {
    match u32::from(character) {
        0x0000..=0x10FF | 0x2000..=0x200D | 0x2010..=0x201F | 0x2032..=0x2037 => 1,
        _ => 2,
    }
}

/// X links bare domains as well as full addresses, and a link never counts
/// for less than its fixed weight.
fn resembles_link(word: &str) -> bool {
    word.contains("://")
        || word.as_bytes().windows(3).any(|window| {
            window[0].is_ascii_alphanumeric()
                && window[1] == b'.'
                && window[2].is_ascii_alphabetic()
        })
}

fn word_weight(word: &str) -> usize {
    let weight = word.chars().map(character_weight).sum();
    match resembles_link(word) {
        true => LINK_WEIGHT.max(weight),
        false => weight,
    }
}

fn weighted_length(text: &str) -> usize {
    let spaces = text
        .chars()
        .filter(|character| character.is_whitespace())
        .count();
    spaces + text.split_whitespace().map(word_weight).sum::<usize>()
}

/// Keep whole words where possible and mark the cut with an ellipsis.
fn clip(text: &str, budget: usize) -> String {
    if weighted_length(text) <= budget {
        return text.to_owned();
    }
    let room = budget.saturating_sub(character_weight(ELLIPSIS));
    let mut end = 0;
    let mut word_end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if weighted_length(&text[..next]) > room {
            break;
        }
        end = next;
        if text[next..].starts_with(' ') {
            word_end = next;
        }
    }
    let cut = if word_end > 0 { word_end } else { end };
    let kept = text[..cut].trim_end_matches(|character: char| !character.is_alphanumeric());
    format!("{kept}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str =
        "https://example.com/posts/a-long-enough-slug-that-exceeds-the-fixed-link-weight";

    fn post_id() -> PostId {
        PostId::parse("11111111-1111-4111-8111-111111111111").unwrap()
    }

    /// The weight of the lead and its link posted together as one X post.
    fn weighted_text(teaser: &Teaser) -> usize {
        weighted_length(&teaser.lead()) + SEPARATOR.len() + LINK_WEIGHT
    }

    #[test]
    fn short_metadata_is_kept_whole() {
        let teaser = Teaser::compose(
            post_id(),
            "A short title",
            "One sentence about the article.",
            URL,
        )
        .unwrap();
        assert_eq!(
            teaser.lead(),
            "A short title\n\nOne sentence about the article."
        );
        assert_eq!(teaser.view().url, URL);
        assert!(weighted_text(&teaser) <= MAX_WEIGHTED_LENGTH);
    }

    #[test]
    fn a_long_description_is_clipped_at_a_word_to_fit_one_post() {
        let description = "word ".repeat(200);
        let teaser = Teaser::compose(post_id(), "Title", &description, URL).unwrap();
        let view = teaser.view();
        assert!(view.summary.ends_with("word…"));
        assert!(weighted_text(&teaser) <= MAX_WEIGHTED_LENGTH);
        assert!(weighted_text(&teaser) > MAX_WEIGHTED_LENGTH - 6);
    }

    #[test]
    fn a_title_that_fills_the_post_drops_the_summary() {
        let title = "long ".repeat(60);
        let teaser = Teaser::compose(post_id(), &title, "Summary text.", URL).unwrap();
        let view = teaser.view();
        assert!(view.title.ends_with('…'));
        assert!(view.summary.is_empty());
        assert_eq!(teaser.lead(), view.title);
        assert!(weighted_text(&teaser) <= MAX_WEIGHTED_LENGTH);
    }

    #[test]
    fn wide_characters_and_bare_domains_use_their_heavier_weights() {
        assert_eq!(weighted_length("abc"), 3);
        assert_eq!(weighted_length("日本語"), 6);
        assert_eq!(weighted_length("see example.com now"), 4 + LINK_WEIGHT + 4);
        assert_eq!(weighted_length("v1.2"), 4);
        let description = "語".repeat(300);
        let teaser = Teaser::compose(post_id(), "Title", &description, URL).unwrap();
        assert!(weighted_text(&teaser) <= MAX_WEIGHTED_LENGTH);
    }

    #[test]
    fn metadata_is_flattened_to_one_line_without_control_characters() {
        let teaser =
            Teaser::compose(post_id(), "  Two\nlines\u{7}  ", "Tabbed\tsummary", URL).unwrap();
        let view = teaser.view();
        assert_eq!(view.title, "Two lines");
        assert_eq!(view.summary, "Tabbed summary");
    }

    #[test]
    fn an_edit_takes_its_first_line_as_the_title_and_is_never_clipped() {
        let teaser = Teaser::compose(post_id(), "Title", "Summary.", URL).unwrap();
        let edited = teaser
            .edited("  A better title\r\n\r\nTwo summary\r\nlines.  ")
            .unwrap();
        assert_eq!(edited.lead(), "A better title\n\nTwo summary lines.");
        assert_eq!(edited.view().url, URL);
        assert_eq!(edited.view().post_id, &post_id());
        assert_eq!(
            teaser.edited("Only a title").unwrap().lead(),
            "Only a title"
        );
        assert_eq!(teaser.edited(" \n "), Err(TeaserError::EmptyTitle));
        assert_eq!(
            teaser.edited(&format!("Title\n{}", "x".repeat(300))),
            Err(TeaserError::TooLong)
        );
    }

    #[test]
    fn stored_parts_are_validated_instead_of_clipped() {
        assert_eq!(
            Teaser::from_parts(post_id(), String::new(), String::new(), URL.to_owned()),
            Err(TeaserError::EmptyTitle)
        );
        assert_eq!(
            Teaser::from_parts(
                post_id(),
                "Title".to_owned(),
                String::new(),
                "https://example.com/a b".to_owned()
            ),
            Err(TeaserError::InvalidUrl)
        );
        assert_eq!(
            Teaser::from_parts(
                post_id(),
                "Title".to_owned(),
                "x".repeat(300),
                URL.to_owned()
            ),
            Err(TeaserError::TooLong)
        );
    }
}
