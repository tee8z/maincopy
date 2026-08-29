use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use super::LogicalContentPath;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FieldPath(String);

impl FieldPath {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn sort_rank(&self) -> u16 {
        let value = self.as_str();
        match value {
            "$path" => 0,
            "$document" => 1,
            "$frontmatter" => 2,
            "site" => 10,
            "site.title" => 11,
            "site.base_url" => 12,
            "site.description" => 13,
            "site.favicon" => 14,
            "author" => 20,
            "author.name" => 21,
            "assets" => 30,
            "assets.allowed_https_origins" => 31,
            "subscriptions" => 40,
            "subscriptions.enabled" => 41,
            "subscriptions.privacy_policy_revision" => 42,
            "tips.enabled" => 50,
            "tips.minimum_sats" => 51,
            "tips.maximum_sats" => 52,
            "id" => 100,
            "title" => 110,
            "slug" => 120,
            "authored_at" => 130,
            "updated_at" => 140,
            "description" => 150,
            "image" => 160,
            "tags" => 170,
            "aliases" => 180,
            "draft" => 190,
            "tips" => 200,
            "distribution" => 210,
            "distribution.x" => 211,
            "distribution.x.enabled" => 212,
            "distribution.x.text" => 213,
            "published_at" => 220,
            _ if value.starts_with("assets.allowed_https_origins[") => 32,
            _ if value.starts_with("tags[") => 171,
            _ if value.starts_with("aliases[") => 181,
            _ => 1_000,
        }
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentValidationCode {
    ContentPlatformUnsupported,
    ContentRootUnavailable,
    PublicationFileMissing,
    ContentNamespaceInvalid,
    ContentEntryUnreadable,
    UnsupportedFilenameEncoding,
    InvalidLogicalContentPath,
    UnexpectedPostEntry,
    ContentSymlinkUnsupported,
    UnsupportedContentEntryKind,
    ContentTextInvalidUtf8,
    AuthoredSvgUnsupported,
    ContentFileTooLarge,
    ContentTreeTooLarge,
    ContentEntryLimitExceeded,
    ContentDepthLimitExceeded,
    ContentPathTooLong,
    DuplicateLogicalAssetPath,
    LogicalPathCaseCollision,
    ContentEntryChanged,
    PublicationTomlInvalid,
    FrontmatterOpeningDelimiterMissing,
    FrontmatterOpeningDelimiterMalformed,
    FrontmatterClosingDelimiterMissing,
    FrontmatterClosingDelimiterMalformed,
    FrontmatterTomlInvalid,
    RequiredFieldMissing,
    InvalidFieldType,
    UnknownField,
    PublishedAtUnsupported,
    TextEmpty,
    TextContainsControl,
    InvalidBaseUrl,
    InvalidPostId,
    InvalidPostSlug,
    InvalidPostTag,
    InvalidPostAlias,
    DatetimeOffsetRequired,
    DatetimeInvalid,
    UpdatedAtBeforeAuthoredAt,
    DuplicateTag,
    AliasMatchesSlug,
    DuplicatePostId,
    DuplicatePostSlug,
    DuplicatePostAlias,
    DuplicatePostRoute,
    SubscriptionPrivacyRevisionRequired,
    DistributionEnabledRequired,
    TipRangeRequired,
    TipAmountInvalid,
    TipRangeInvalid,
    PostTipsUnconfigured,
    DraftDirectoryConflict,
    PostCollectionPathMismatch,
    InternalValidationInvariant,
}

impl ContentValidationCode {
    pub(crate) const fn sort_rank(self) -> u16 {
        match self {
            Self::ContentPlatformUnsupported => 0,
            Self::ContentRootUnavailable => 1,
            Self::PublicationFileMissing => 2,
            Self::ContentNamespaceInvalid => 3,
            Self::ContentEntryUnreadable => 4,
            Self::UnsupportedFilenameEncoding => 5,
            Self::InvalidLogicalContentPath => 6,
            Self::UnexpectedPostEntry => 7,
            Self::ContentSymlinkUnsupported => 8,
            Self::UnsupportedContentEntryKind => 9,
            Self::ContentTextInvalidUtf8 => 10,
            Self::AuthoredSvgUnsupported => 11,
            Self::ContentFileTooLarge => 12,
            Self::ContentTreeTooLarge => 13,
            Self::ContentEntryLimitExceeded => 14,
            Self::ContentDepthLimitExceeded => 15,
            Self::ContentPathTooLong => 16,
            Self::DuplicateLogicalAssetPath => 17,
            Self::LogicalPathCaseCollision => 18,
            Self::ContentEntryChanged => 19,
            Self::PublicationTomlInvalid => 30,
            Self::FrontmatterOpeningDelimiterMissing => 31,
            Self::FrontmatterOpeningDelimiterMalformed => 32,
            Self::FrontmatterClosingDelimiterMissing => 33,
            Self::FrontmatterClosingDelimiterMalformed => 34,
            Self::FrontmatterTomlInvalid => 35,
            Self::RequiredFieldMissing => 40,
            Self::InvalidFieldType => 41,
            Self::UnknownField => 42,
            Self::PublishedAtUnsupported => 43,
            Self::TextEmpty => 50,
            Self::TextContainsControl => 51,
            Self::InvalidBaseUrl => 52,
            Self::InvalidPostId => 53,
            Self::InvalidPostSlug => 54,
            Self::InvalidPostTag => 55,
            Self::InvalidPostAlias => 56,
            Self::DatetimeOffsetRequired => 57,
            Self::DatetimeInvalid => 58,
            Self::UpdatedAtBeforeAuthoredAt => 59,
            Self::DuplicateTag => 60,
            Self::AliasMatchesSlug => 61,
            Self::DuplicatePostId => 62,
            Self::DuplicatePostSlug => 63,
            Self::DuplicatePostAlias => 64,
            Self::DuplicatePostRoute => 65,
            Self::SubscriptionPrivacyRevisionRequired => 70,
            Self::DistributionEnabledRequired => 71,
            Self::TipRangeRequired => 72,
            Self::TipAmountInvalid => 73,
            Self::TipRangeInvalid => 74,
            Self::PostTipsUnconfigured => 75,
            Self::DraftDirectoryConflict => 76,
            Self::PostCollectionPathMismatch => 77,
            Self::InternalValidationInvariant => u16::MAX,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationLocation {
    path: LogicalContentPath,
    field: FieldPath,
}

impl ValidationLocation {
    pub(crate) const fn new(path: LogicalContentPath, field: FieldPath) -> Self {
        Self { path, field }
    }

    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn field(&self) -> &FieldPath {
        &self.field
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContentValidationError {
    path: LogicalContentPath,
    field: FieldPath,
    code: ContentValidationCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    related: Option<ValidationLocation>,
}

impl ContentValidationError {
    pub(crate) fn new(
        path: LogicalContentPath,
        field: impl Into<String>,
        code: ContentValidationCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path,
            field: FieldPath::new(field),
            code,
            message: message.into(),
            related: None,
        }
    }

    pub(crate) fn with_related(mut self, related: ValidationLocation) -> Self {
        self.related = Some(related);
        self
    }

    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn field(&self) -> &FieldPath {
        &self.field
    }

    pub const fn code(&self) -> ContentValidationCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn related(&self) -> Option<&ValidationLocation> {
        self.related.as_ref()
    }

    fn sort_key(&self) -> (&str, u16, &str, u16, Option<(&str, &str)>, &str) {
        (
            self.path.as_str(),
            self.field.sort_rank(),
            self.field.as_str(),
            self.code.sort_rank(),
            self.related
                .as_ref()
                .map(|related| (related.path.as_str(), related.field.as_str())),
            self.message.as_str(),
        )
    }
}

impl fmt::Display for ContentValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}: {}", self.path, self.field, self.message)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentValidationErrors(Vec<ContentValidationError>);

impl ContentValidationErrors {
    pub fn errors(&self) -> &[ContentValidationError] {
        &self.0
    }

    pub fn into_errors(self) -> Vec<ContentValidationError> {
        self.0
    }
}

impl fmt::Display for ContentValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "content validation failed with {} error(s)",
            self.0.len()
        )
    }
}

impl Error for ContentValidationErrors {}

#[derive(Default)]
pub(crate) struct DiagnosticCollector {
    errors: Vec<ContentValidationError>,
}

impl DiagnosticCollector {
    pub(crate) fn push(&mut self, error: ContentValidationError) {
        self.errors.push(error);
    }

    pub(crate) fn len(&self) -> usize {
        self.errors.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub(crate) fn finish(mut self) -> ContentValidationErrors {
        if self.errors.is_empty() {
            self.errors.push(ContentValidationError::new(
                LogicalContentPath::new("<content-validation>"),
                "$document",
                ContentValidationCode::InternalValidationInvariant,
                "an empty diagnostic collection was finalized as an error",
            ));
        }
        self.errors
            .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        self.errors.dedup();
        ContentValidationErrors(self.errors)
    }
}
