//! Command-line input and output models for the standalone validator.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Serialize;

use crate::{ContentValidationError, PostCollection};

#[derive(Debug, Parser)]
#[command(
    name = "markdowncompiler",
    version,
    about = "Validate one Maincopy Markdown post",
    after_long_help = "SINGLE-FILE LIMITATIONS:\n    This command validates one Markdown file in isolation. It cannot detect\n    cross-file duplicate IDs, slugs, aliases, or routes. It does not load\n    publication.toml, inspect the content tree, or perform asset resolution."
)]
pub(crate) struct Arguments {
    /// Treat the document as a member of this content collection.
    #[arg(long, value_enum, default_value_t = CollectionArgument::Posts)]
    pub(crate) collection: CollectionArgument,

    /// Emit one machine-readable JSON object on stdout.
    #[arg(long)]
    pub(crate) json: bool,

    /// Markdown file to validate.
    #[arg(value_name = "MARKDOWN")]
    pub(crate) markdown: PathBuf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum CollectionArgument {
    Posts,
    Drafts,
}

impl From<CollectionArgument> for PostCollection {
    fn from(value: CollectionArgument) -> Self {
        match value {
            CollectionArgument::Posts => Self::Posts,
            CollectionArgument::Drafts => Self::Drafts,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct JsonReport<'report> {
    pub(crate) path: &'report str,
    pub(crate) valid: bool,
    pub(crate) diagnostics: &'report [ContentValidationError],
}

#[derive(Serialize)]
pub(crate) struct JsonErrorReport<'report> {
    pub(crate) error: JsonError<'report>,
}

#[derive(Serialize)]
pub(crate) struct JsonError<'report> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<&'report str>,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_arguments_preserve_domain_semantics() {
        let posts = Arguments::try_parse_from(["markdowncompiler", "post.md"]).unwrap();
        assert!(matches!(posts.collection, CollectionArgument::Posts));
        assert_eq!(
            PostCollection::from(posts.collection),
            PostCollection::Posts
        );

        let drafts =
            Arguments::try_parse_from(["markdowncompiler", "--collection", "drafts", "post.md"])
                .unwrap();
        assert!(matches!(drafts.collection, CollectionArgument::Drafts));
        assert_eq!(
            PostCollection::from(drafts.collection),
            PostCollection::Drafts
        );
    }
}
